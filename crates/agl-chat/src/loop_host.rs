use std::path::Path;

use agl_events::{
    EventDraft, EventScope, RuntimeEvent, RuntimeEventEnvelope, RuntimeEventWriter,
    SafeRuntimeEventEnvelope,
};
use agl_ids::{AttemptId, RequestId, RunId, SessionId, TurnId};
use agl_loop::{
    AgentLoopHost, ModelRequest, ModelResponse, ToolDispatchRequest, ToolDispatchResponse,
    TurnMessage, TurnTransitionRecord, VisibleTool,
};
use agl_tools::{
    HookBatchRequest, HookBatchResult, HookInput, HookMessage, HookResult, HookStatus, ToolId,
    ToolInput, ToolRuntime,
};
use anyhow::{Context, Result, ensure};

use crate::session::InferenceSession;
use crate::tools::{ChatToolRuntimeConfig, chat_tool_runtime};

pub struct ChatLoopHost {
    session: InferenceSession,
    event_sink: Option<RuntimeEventWriter>,
    event_scope: Option<EventScope>,
    request_id: Option<RequestId>,
    runtime_events: Vec<SafeRuntimeEventEnvelope>,
    pending_terminal_event: Option<RuntimeEvent>,
    attempt_ids: Vec<AttemptId>,
    core_guards: agl_tools::guards::CoreGuards,
    core_tools: agl_tools::CoreTools,
    tool_runtime: ToolRuntime,
    generated_requests: usize,
    turn_messages: Vec<TurnMessage>,
}

impl ChatLoopHost {
    pub fn new(session: InferenceSession, workspace_root: impl AsRef<Path>) -> Result<Self> {
        let core_tools = agl_tools::CoreTools::new(workspace_root.as_ref())
            .context("failed to initialize core filesystem tools")?;
        let tool_runtime = build_chat_tool_runtime(&session, &core_tools, workspace_root.as_ref())?;
        Ok(Self {
            session,
            event_sink: None,
            event_scope: None,
            request_id: None,
            runtime_events: Vec::new(),
            pending_terminal_event: None,
            attempt_ids: Vec::new(),
            core_guards: agl_tools::guards::CoreGuards::new(),
            core_tools,
            tool_runtime,
            generated_requests: 0,
            turn_messages: Vec::new(),
        })
    }

    pub fn session(&self) -> &InferenceSession {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut InferenceSession {
        &mut self.session
    }

    pub fn begin_turn(
        &mut self,
        session_id: &SessionId,
        run_id: &RunId,
        turn_id: &TurnId,
        request_id: Option<RequestId>,
    ) -> Result<()> {
        self.refresh_runtime_context(run_id)?;
        self.event_sink = Some(RuntimeEventWriter::open(
            self.session.event_stream_path(run_id),
        )?);
        self.event_scope = Some(
            EventScope::builder(run_id.clone())
                .session_id(session_id.clone())
                .turn_id(turn_id.clone())
                .build()?,
        );
        self.request_id = request_id;
        self.generated_requests = 0;
        self.turn_messages.clear();
        self.runtime_events.clear();
        self.pending_terminal_event = None;
        self.attempt_ids.clear();
        Ok(())
    }

    pub fn generated_requests(&self) -> usize {
        self.generated_requests
    }

    pub fn take_turn_messages(&mut self) -> Vec<TurnMessage> {
        std::mem::take(&mut self.turn_messages)
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
        let path = self
            .event_sink
            .as_ref()
            .context("turn event writer is not initialized")?
            .path();
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read runtime event stream {}", path.display()))?;
        let events = content
            .lines()
            .map(|line| {
                serde_json::from_str(line).with_context(|| {
                    format!("failed to decode runtime event from {}", path.display())
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.runtime_events.clear();
        Ok(events)
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

    pub fn append_pending_terminal_event(&mut self) -> Result<RuntimeEventEnvelope> {
        let event = self
            .pending_terminal_event
            .take()
            .context("turn terminal event is not pending")?;
        self.append_runtime_event(event)
    }

    pub fn append_failed_terminal_event(&mut self) -> Result<RuntimeEventEnvelope> {
        self.pending_terminal_event = None;
        self.append_runtime_event(RuntimeEvent::TurnFinished {
            status: agl_events::TurnFinishStatus::Failed,
        })
    }

    pub fn workspace_root(&self) -> &Path {
        self.core_tools.root()
    }

    pub fn set_workspace_root(&mut self, workspace_root: impl AsRef<Path>) -> Result<()> {
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
        self.session.refresh_runtime_context(None)?;
        self.rebuild_tool_runtime()
    }

    pub fn refresh_runtime_context(&mut self, run_id: &RunId) -> Result<()> {
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

impl AgentLoopHost for ChatLoopHost {
    fn run_hooks(&mut self, request: HookBatchRequest) -> Result<HookBatchResult> {
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

    fn generate(&mut self, request: ModelRequest) -> Result<ModelResponse> {
        self.generated_requests += 1;
        let (session_id, request_id) = inference_correlation(
            self.event_scope.as_ref(),
            self.request_id.as_ref(),
            &request,
        )?;
        let attempt_id = AttemptId::generate();
        self.attempt_ids.push(attempt_id.clone());
        let response =
            self.session
                .generate(request, attempt_id.clone(), session_id, request_id)?;
        ensure!(
            response.attempt_id == attempt_id,
            "inference response attempt ID does not match the admitted attempt"
        );
        Ok(ModelResponse {
            content: response.content,
        })
    }

    fn dispatch_tool(&mut self, request: ToolDispatchRequest) -> Result<ToolDispatchResponse> {
        self.session.prepare_artifact_write_for_tool(
            &request.run_id,
            &request.name,
            &request.arguments,
        )?;
        let tool_id = ToolId::new(request.name.clone())
            .with_context(|| format!("tool id is invalid: {}", request.name))?;
        let output = self
            .tool_runtime
            .dispatch(ToolInput {
                id: tool_id,
                arguments: request.arguments,
            })
            .with_context(|| format!("tool `{}` failed", request.name))?;
        Ok(ToolDispatchResponse {
            observation: output.observation,
        })
    }

    fn record_turn_messages(&mut self, messages: &[TurnMessage]) -> Result<()> {
        self.turn_messages = messages.to_vec();
        Ok(())
    }

    fn emit_transition(
        &mut self,
        record: &TurnTransitionRecord,
        event: &RuntimeEvent,
    ) -> Result<()> {
        let scope = self
            .event_scope
            .as_ref()
            .context("turn event scope is not initialized")?;
        ensure!(
            scope.run_id() == &record.run_id && scope.turn_id() == Some(&record.turn_id),
            "turn transition identity does not match the active event scope"
        );
        if matches!(event, RuntimeEvent::TurnFinished { .. }) {
            ensure!(
                self.pending_terminal_event.is_none(),
                "turn terminal event is already pending"
            );
            self.pending_terminal_event = Some(event.clone());
            return Ok(());
        }
        self.append_event(EventDraft::new(scope.clone(), event.clone()))
            .map(|_| ())
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

fn missing_hook_result(hook_id: agl_tools::HookId) -> HookResult {
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
            .map(|tool| tool.name.clone())
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
    let mut tool_runtime = chat_tool_runtime(ChatToolRuntimeConfig {
        core_tools,
        store_root: session.store_root(),
        trust_store_path: session.trust_store_path(),
        workspace_root,
        permission_status: permission_runtime_status(session),
    })?;
    tool_runtime.set_allowed_tools(visible_tool_ids(session.turn_visible_tools())?);
    Ok(tool_runtime)
}

fn visible_tool_ids(visible_tools: &[VisibleTool]) -> Result<Vec<ToolId>> {
    visible_tools
        .iter()
        .map(|tool| {
            ToolId::new(tool.name.clone())
                .with_context(|| format!("visible tool id is invalid: {}", tool.name))
        })
        .collect()
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
