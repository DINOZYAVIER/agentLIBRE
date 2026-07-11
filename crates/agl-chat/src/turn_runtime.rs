use std::path::Path;
use std::time::Instant;

use agl_capabilities::{ActionInvocation, DispatchDenial, DispatchDenialCode, HookInput};
use agl_events::{
    CapabilityExclusionEvent, EventDraft, EventScope, RuntimeEvent, RuntimeEventEnvelope,
    RuntimeEventWriter, SafeRuntimeEventEnvelope,
};
use agl_ids::{AttemptId, ExecutionScope, RequestId, RunId, SessionId, StepId, TurnId};
use agl_inference::InferenceCancellation;
use agl_loop::{
    HookBatchRequest, HookBatchResult, HookMessage, HookResult, HookStatus, ModelRequest,
    ModelResponse, ToolDispatchRequest, ToolDispatchResponse,
};
use agl_store::EffectDeliveryClass;
use agl_tools::ToolRuntime;
use anyhow::{Context, Result, ensure};

use crate::session::{InferenceExecutionControl, InferenceSession};
use crate::tools::{ChatToolRuntimeConfig, chat_tool_runtime};

pub struct ChatTurnRuntime {
    session: InferenceSession,
    active_effective_capabilities: Option<agl_capabilities::EffectiveCapabilitySet>,
    event_sink: Option<RuntimeEventWriter>,
    event_scope: Option<EventScope>,
    request_id: Option<RequestId>,
    runtime_events: Vec<SafeRuntimeEventEnvelope>,
    attempt_ids: Vec<AttemptId>,
    core_guards: agl_tools::guards::CoreGuards,
    core_tools: agl_tools::CoreTools,
    tool_runtime: ToolRuntime,
    generated_requests: usize,
    model_input_tokens: u64,
    model_output_tokens: u64,
}

impl ChatTurnRuntime {
    pub fn new(session: InferenceSession, workspace_root: impl AsRef<Path>) -> Result<Self> {
        let core_tools = agl_tools::CoreTools::new(workspace_root.as_ref())
            .context("failed to initialize core filesystem tools")?;
        let tool_runtime = build_chat_tool_runtime(&session, &core_tools, workspace_root.as_ref())?;
        Ok(Self {
            session,
            active_effective_capabilities: None,
            event_sink: None,
            event_scope: None,
            request_id: None,
            runtime_events: Vec::new(),
            attempt_ids: Vec::new(),
            core_guards: agl_tools::guards::CoreGuards::new(),
            core_tools,
            tool_runtime,
            generated_requests: 0,
            model_input_tokens: 0,
            model_output_tokens: 0,
        })
    }

    pub fn session(&self) -> &InferenceSession {
        &self.session
    }

    pub fn clear_context(&mut self) -> Result<()> {
        ensure!(
            self.active_effective_capabilities.is_none(),
            "cannot clear context during an active turn"
        );
        self.session.clear_context()
    }

    pub fn release_context(&self) -> Result<()> {
        ensure!(
            self.active_effective_capabilities.is_none(),
            "cannot release context during an active turn"
        );
        self.session.release_context()
    }

    pub(crate) fn release_context_for_teardown(&self) -> Result<()> {
        self.session.release_context()
    }

    pub fn begin_turn(
        &mut self,
        session_id: &SessionId,
        run_id: &RunId,
        turn_id: &TurnId,
        request_id: Option<RequestId>,
    ) -> Result<()> {
        self.initialize_turn(session_id, run_id, turn_id, request_id, None)
    }

    pub(crate) fn resume_turn(
        &mut self,
        session_id: &SessionId,
        run_id: &RunId,
        turn_id: &TurnId,
        request_id: Option<RequestId>,
        durable_event_sequence: u64,
    ) -> Result<()> {
        self.initialize_turn(
            session_id,
            run_id,
            turn_id,
            request_id,
            Some(durable_event_sequence),
        )
    }

    fn initialize_turn(
        &mut self,
        session_id: &SessionId,
        run_id: &RunId,
        turn_id: &TurnId,
        request_id: Option<RequestId>,
        durable_event_sequence: Option<u64>,
    ) -> Result<()> {
        self.refresh_runtime_context(run_id)?;
        self.active_effective_capabilities = Some(self.session.effective_capabilities().clone());
        self.event_sink = Some(match durable_event_sequence {
            Some(sequence) => RuntimeEventWriter::open_evidence_at_sequence(
                self.session.event_stream_path(run_id),
                run_id,
                sequence,
            )?,
            None => RuntimeEventWriter::open(self.session.event_stream_path(run_id))?,
        });
        self.event_scope = Some(
            EventScope::builder(run_id.clone())
                .session_id(session_id.clone())
                .turn_id(turn_id.clone())
                .build()?,
        );
        self.request_id = request_id;
        self.generated_requests = 0;
        self.runtime_events.clear();
        self.attempt_ids.clear();
        if durable_event_sequence.is_none() {
            self.append_runtime_event(capability_policy_resolved_event(
                self.active_effective_capabilities
                    .as_ref()
                    .expect("active capability snapshot was just initialized"),
            ))?;
        }
        Ok(())
    }

    pub fn generated_requests(&self) -> usize {
        self.generated_requests
    }

    pub(crate) fn model_token_usage(&self) -> (u64, u64) {
        (self.model_input_tokens, self.model_output_tokens)
    }

    #[cfg(test)]
    pub(crate) fn active_policy_hash(&self) -> Option<&agl_capabilities::PolicyHash> {
        self.active_effective_capabilities
            .as_ref()
            .map(agl_capabilities::EffectiveCapabilitySet::policy_hash)
    }

    pub fn take_attempt_ids(&mut self) -> Vec<AttemptId> {
        std::mem::take(&mut self.attempt_ids)
    }

    pub fn has_linked_attempt(&self, attempt_id: &AttemptId) -> bool {
        self.runtime_events.iter().any(|event| {
            matches!(
                event.payload,
                agl_events::SafeRuntimeEvent::ModelAttemptLinked
            ) && event.scope.attempt_id() == Some(attempt_id)
        })
    }

    pub fn take_runtime_events(&mut self) -> Result<Vec<SafeRuntimeEventEnvelope>> {
        let events = self.read_runtime_events_after(0)?;
        self.runtime_events.clear();
        self.active_effective_capabilities = None;
        Ok(events)
    }

    pub(crate) fn read_runtime_events_after(
        &self,
        sequence: u64,
    ) -> Result<Vec<SafeRuntimeEventEnvelope>> {
        let path = self
            .event_sink
            .as_ref()
            .context("turn event writer is not initialized")?
            .path();
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read runtime event stream {}", path.display()))?;
        content
            .lines()
            .map(|line| {
                serde_json::from_str(line).with_context(|| {
                    format!("failed to decode runtime event from {}", path.display())
                })
            })
            .collect::<Result<Vec<_>>>()
            .map(|events| {
                events
                    .into_iter()
                    .filter(|event: &SafeRuntimeEventEnvelope| event.sequence > sequence)
                    .collect()
            })
    }

    pub fn append_runtime_event(&mut self, event: RuntimeEvent) -> Result<RuntimeEventEnvelope> {
        let scope = self
            .event_scope
            .as_ref()
            .context("turn event scope is not initialized")?
            .clone();
        self.append_event(EventDraft::new(scope, event))
    }

    pub fn append_attempt_linked_event(
        &mut self,
        attempt_id: &AttemptId,
    ) -> Result<RuntimeEventEnvelope> {
        let active = self
            .event_scope
            .as_ref()
            .context("turn event scope is not initialized")?;
        let mut builder =
            EventScope::builder(active.run_id().clone()).attempt_id(attempt_id.clone());
        if let Some(session_id) = active.session_id() {
            builder = builder.session_id(session_id.clone());
        }
        if let Some(turn_id) = active.turn_id() {
            builder = builder.turn_id(turn_id.clone());
        }
        let scope = builder.build()?;
        self.append_event(EventDraft::new(scope, RuntimeEvent::ModelAttemptLinked))
    }

    pub fn append_failed_terminal_event(&mut self) -> Result<RuntimeEventEnvelope> {
        self.append_runtime_event(RuntimeEvent::TurnFinished {
            status: agl_events::TurnFinishStatus::Failed,
        })
    }

    pub fn workspace_root(&self) -> &Path {
        self.core_tools.root()
    }

    pub fn set_workspace_root(&mut self, workspace_root: impl AsRef<Path>) -> Result<()> {
        ensure!(
            self.active_effective_capabilities.is_none(),
            "cannot change workspace root during an active turn"
        );
        let core_tools = agl_tools::CoreTools::new(workspace_root.as_ref())
            .context("failed to update core filesystem tool root")?;
        self.session
            .set_workspace_root_and_refresh(workspace_root.as_ref())?;
        let tool_runtime =
            build_chat_tool_runtime(&self.session, &core_tools, workspace_root.as_ref())?;
        self.core_tools = core_tools;
        self.tool_runtime = tool_runtime;
        Ok(())
    }

    pub fn reload_runtime_context(&mut self) -> Result<()> {
        ensure!(
            self.active_effective_capabilities.is_none(),
            "cannot reload runtime context during an active turn"
        );
        self.session.refresh_runtime_context(None)?;
        self.rebuild_tool_runtime()
    }

    pub fn refresh_runtime_context(&mut self, run_id: &RunId) -> Result<()> {
        ensure!(
            self.active_effective_capabilities.is_none(),
            "cannot refresh runtime context during an active turn"
        );
        self.session.refresh_runtime_context(Some(run_id))?;
        self.rebuild_tool_runtime()
    }

    fn rebuild_tool_runtime(&mut self) -> Result<()> {
        self.tool_runtime =
            build_chat_tool_runtime(&self.session, &self.core_tools, self.core_tools.root())?;
        Ok(())
    }

    fn append_event(
        &mut self,
        mut draft: EventDraft<RuntimeEvent>,
    ) -> Result<RuntimeEventEnvelope> {
        if let Some(request_id) = &self.request_id {
            draft = draft.with_request_id(request_id.clone());
        }
        if let Some(previous) = self.runtime_events.last() {
            draft = draft.with_causation(previous.event_id.clone());
        }
        let (full_envelope, safe_envelope) = self
            .event_sink
            .as_ref()
            .context("turn event writer is not initialized")?
            .append_with_full(draft)?;
        self.runtime_events.push(safe_envelope);
        Ok(full_envelope)
    }
}

impl ChatTurnRuntime {
    pub(crate) fn policy_hash(&self) -> Result<String> {
        Ok(self
            .active_effective_capabilities
            .as_ref()
            .context("turn capability snapshot is not initialized")?
            .policy_hash()
            .as_str()
            .to_string())
    }

    pub(crate) fn capability_delivery_class(
        &self,
        capability_id: &agl_capabilities::CapabilityId,
    ) -> Result<EffectDeliveryClass> {
        let capability = self
            .active_effective_capabilities
            .as_ref()
            .context("turn capability snapshot is not initialized")?
            .capability(capability_id)
            .context("pending capability is not in the effective turn snapshot")?;
        Ok(match capability.declaration().delivery {
            agl_capabilities::ActionDelivery::ReplaySafe => EffectDeliveryClass::ReplaySafe,
            agl_capabilities::ActionDelivery::IdempotentRunStep => EffectDeliveryClass::Idempotent,
            agl_capabilities::ActionDelivery::AtMostOnce => EffectDeliveryClass::AtMostOnce,
        })
    }

    pub(crate) fn append_executor_events(
        &mut self,
        drafts: Vec<EventDraft<RuntimeEvent>>,
    ) -> Result<()> {
        let active = self
            .event_scope
            .as_ref()
            .context("turn event scope is not initialized")?
            .clone();
        for draft in drafts {
            ensure!(
                draft.scope.run_id() == active.run_id()
                    && draft.scope.turn_id() == active.turn_id(),
                "turn event draft identity does not match the active event scope"
            );
            self.append_event(EventDraft::new(active.clone(), draft.payload))?;
        }
        Ok(())
    }

    pub(crate) fn execute_hooks(&mut self, request: HookBatchRequest) -> Result<HookBatchResult> {
        let results = request
            .hooks
            .iter()
            .map(|hook_id| {
                if self
                    .core_guards
                    .declaration()
                    .hooks
                    .iter()
                    .any(|hook| hook.id == *hook_id)
                {
                    self.core_guards.run_hook(HookInput {
                        hook_id: hook_id.clone(),
                        event: request.event,
                        payload: request.payload.clone(),
                    })
                } else {
                    missing_hook_result(hook_id.clone())
                }
            })
            .collect();
        Ok(HookBatchResult {
            event: request.event,
            results,
        })
    }

    pub(crate) fn execute_model(
        &mut self,
        request: ModelRequest,
        cancellation: InferenceCancellation,
        deadline: Option<Instant>,
    ) -> Result<ModelResponse> {
        self.generated_requests += 1;
        let (session_id, request_id) = inference_correlation(
            self.event_scope.as_ref(),
            self.request_id.as_ref(),
            &request,
        )?;
        let attempt_id = AttemptId::generate();
        self.attempt_ids.push(attempt_id.clone());
        let response = self.session.generate(
            request,
            attempt_id.clone(),
            session_id,
            request_id,
            self.active_effective_capabilities
                .as_ref()
                .context("turn capability snapshot is not initialized")?,
            InferenceExecutionControl {
                cancellation,
                deadline,
            },
        )?;
        ensure!(
            response.attempt_id == attempt_id,
            "inference response attempt ID does not match the admitted attempt"
        );
        self.model_input_tokens = self
            .model_input_tokens
            .saturating_add(response.metadata.input_tokens);
        self.model_output_tokens = self
            .model_output_tokens
            .saturating_add(response.metadata.output_tokens);
        Ok(ModelResponse {
            content: agl_content::Content::text(response.content)?,
        })
    }

    pub(crate) fn execute_capability(
        &mut self,
        request: ToolDispatchRequest,
        step_id: Option<&StepId>,
    ) -> Result<ToolDispatchResponse> {
        let active_scope = self
            .event_scope
            .as_ref()
            .context("turn event scope is not initialized")?
            .clone();
        ensure!(
            active_scope.run_id() == &request.run_id
                && active_scope.turn_id() == Some(&request.turn_id),
            "tool request identity does not match the active event scope"
        );
        let effective = self
            .active_effective_capabilities
            .as_ref()
            .context("turn capability snapshot is not initialized")?
            .clone();
        let policy_hash = effective.policy_hash().as_str().to_string();
        let capability_id = request.capability_id.clone();
        let Some(capability) = effective.capability(&capability_id).cloned() else {
            let denial = DispatchDenial {
                capability_id: capability_id.clone(),
                code: DispatchDenialCode::CapabilityNotEffective,
            };
            self.append_runtime_event(RuntimeEvent::CapabilityCallDenied {
                policy_hash,
                capability_id: Some(capability_id.as_str().to_string()),
                reason_code: denial.code.as_str().to_string(),
            })?;
            return Err(denial).context("capability dispatch was denied");
        };
        let scope = execution_scope(&active_scope, step_id)?;
        let mut invocation = ActionInvocation::new(
            scope,
            capability_id.clone(),
            capability.provider_id().clone(),
            capability.declaration_digest().clone(),
            effective.policy_hash().clone(),
            request.arguments.clone(),
        );
        if let Some(request_id) = &self.request_id {
            invocation = invocation.with_request_id(request_id.clone());
        }
        if let Err(denial) =
            effective.authorize(&invocation, self.tool_runtime.catalog().providers())
        {
            self.append_runtime_event(RuntimeEvent::CapabilityCallDenied {
                policy_hash,
                capability_id: Some(capability_id.as_str().to_string()),
                reason_code: denial.code.as_str().to_string(),
            })?;
            return Err(denial).context("capability dispatch was denied");
        }
        self.append_runtime_event(RuntimeEvent::CapabilityCallAdmitted {
            policy_hash,
            capability_id: capability_id.as_str().to_string(),
            provider_id: capability.provider_id().as_str().to_string(),
            declaration_digest: capability.declaration_digest().as_str().to_string(),
        })?;
        self.session.prepare_artifact_write_for_tool(
            &request.run_id,
            request.capability_id.as_str(),
            &request.arguments,
        )?;
        let output = self
            .tool_runtime
            .dispatch(invocation, &effective)
            .map_err(|error| {
                anyhow::Error::new(error)
                    .context(format!("capability `{}` failed", request.capability_id))
            })?;
        Ok(ToolDispatchResponse { result: output })
    }
}

fn inference_correlation(
    active_scope: Option<&EventScope>,
    request_id: Option<&RequestId>,
    request: &ModelRequest,
) -> Result<(Option<SessionId>, Option<RequestId>)> {
    let active_scope = active_scope.context("turn event scope is not initialized")?;
    ensure!(
        active_scope.run_id() == &request.run_id
            && active_scope.turn_id() == Some(&request.turn_id),
        "model request identity does not match the active event scope"
    );
    Ok((active_scope.session_id().cloned(), request_id.cloned()))
}

fn capability_policy_resolved_event(
    effective: &agl_capabilities::EffectiveCapabilitySet,
) -> RuntimeEvent {
    RuntimeEvent::CapabilityPolicyResolved {
        policy_hash: effective.policy_hash().as_str().to_string(),
        capability_ids: effective
            .capabilities()
            .map(|capability| capability.declaration().id.as_str().to_string())
            .collect(),
        exclusions: effective
            .exclusions()
            .map(|exclusion| CapabilityExclusionEvent {
                capability_id: exclusion.capability_id.as_str().to_string(),
                reason_code: exclusion.reason.code().to_string(),
            })
            .collect(),
    }
}

fn execution_scope(scope: &EventScope, step_id: Option<&StepId>) -> Result<ExecutionScope> {
    let mut builder = ExecutionScope::builder(scope.run_id().clone());
    if let Some(session_id) = scope.session_id() {
        builder = builder.session_id(session_id.clone());
    }
    if let Some(turn_id) = scope.turn_id() {
        builder = builder.turn_id(turn_id.clone());
    }
    if let Some(step_id) = step_id.or_else(|| scope.step_id()) {
        builder = builder.step_id(step_id.clone());
    }
    if let Some(attempt_id) = scope.attempt_id() {
        builder = builder.attempt_id(attempt_id.clone());
    }
    builder
        .build()
        .context("active event scope is invalid for capability invocation")
}

fn missing_hook_result(hook_id: agl_capabilities::HookId) -> HookResult {
    HookResult {
        hook_id,
        status: HookStatus::Fail,
        messages: vec![HookMessage {
            code: "cli_hook.missing".to_string(),
            message: "hook is not available in the CLI host".to_string(),
            fix: None,
        }],
    }
}

fn permission_runtime_status(
    session: &crate::InferenceSession,
) -> agl_tools::PermissionRuntimeStatus {
    agl_tools::PermissionRuntimeStatus {
        current_mode: session.tool_mode().as_str().to_string(),
        visible_tools: session
            .turn_visible_tools()
            .iter()
            .map(|tool| tool.id.as_str().to_string())
            .collect(),
        dynamic_grants: true,
        granted_visible_tools: session.permission_grants().granted_visible_tools(),
        ignored_grants: session.permission_grants().ignored_grants(),
    }
}

fn build_chat_tool_runtime(
    session: &InferenceSession,
    core_tools: &agl_tools::CoreTools,
    workspace_root: &Path,
) -> Result<ToolRuntime> {
    let screen_id = agl_capabilities::CapabilityId::new(agl_host_tools::SCREEN_CAPTURE_TOOL_ID)?;
    chat_tool_runtime(ChatToolRuntimeConfig {
        core_tools,
        store_root: session.store_root(),
        trust_store_path: session.trust_store_path(),
        workspace_root,
        permission_status: permission_runtime_status(session),
        screen_admitted_run: session
            .permission_grants()
            .sensitive_input_run(&screen_id, agl_capabilities::SensitiveInput::ScreenCapture)
            .cloned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN_ID: &str = "run_01890f17-4a00-7000-8000-000000000001";
    const TURN_ID: &str = "turn_01890f17-4a00-7000-8000-000000000002";
    const SESSION_ID: &str = "ses_01890f17-4a00-7000-8000-000000000003";
    const REQUEST_ID: &str = "req_01890f17-4a00-7000-8000-000000000004";

    fn model_request(run_id: RunId, turn_id: TurnId) -> ModelRequest {
        ModelRequest {
            run_id,
            turn_id,
            request_index: 0,
            messages: Vec::new(),
            visible_tools: Vec::new(),
        }
    }

    #[test]
    fn inference_correlation_comes_from_active_turn_admission() {
        let run_id = RunId::parse(RUN_ID).unwrap();
        let turn_id = TurnId::parse(TURN_ID).unwrap();
        let session_id = SessionId::parse(SESSION_ID).unwrap();
        let request_id = RequestId::parse(REQUEST_ID).unwrap();
        let scope = EventScope::builder(run_id.clone())
            .session_id(session_id.clone())
            .turn_id(turn_id.clone())
            .build()
            .unwrap();

        let correlation = inference_correlation(
            Some(&scope),
            Some(&request_id),
            &model_request(run_id, turn_id),
        )
        .unwrap();

        assert_eq!(correlation, (Some(session_id), Some(request_id)));
    }

    #[test]
    fn inference_correlation_rejects_a_different_turn() {
        let run_id = RunId::parse(RUN_ID).unwrap();
        let turn_id = TurnId::parse(TURN_ID).unwrap();
        let scope = EventScope::builder(run_id.clone())
            .turn_id(turn_id)
            .build()
            .unwrap();

        let error = inference_correlation(
            Some(&scope),
            None,
            &model_request(run_id, TurnId::generate()),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("model request identity does not match")
        );
    }
}
