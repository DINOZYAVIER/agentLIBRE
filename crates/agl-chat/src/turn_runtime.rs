use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agl_events::{
    EventDraft, EventScope, RuntimeEvent, RuntimeEventEnvelope, RuntimeEventWriter,
    SafeRuntimeEventEnvelope, ToolExclusionEvent,
};
use agl_ids::{
    AttemptId, EventId, ExecutionScope, MessageId, RequestId, RunId, SessionId, StepId, TurnId,
};
use agl_inference::{InferenceCancellation, InferenceFinishReason, InferenceOutputSink};
use agl_kernel::{
    DispatchDenial, DispatchDenialCode, ToolEffectJournal, ToolEffectJournalError,
    ToolEffectJournalRecord, ToolRuntime,
};
use agl_kernel::{
    HookBatchRequest, HookBatchResult, HookMessage, HookResult, HookStatus, IncompleteOutputReason,
    ModelRequest, ModelResponse, ModelResponseOutcome, ToolDispatchRequest, ToolDispatchResponse,
};
use agl_kernel::{HookInput, ToolId, ToolInvocation};
use agl_kernel::{RunDelivery, RunRepository};
use anyhow::{Context, Result, ensure};

use crate::session::{InferenceExecutionControl, InferenceSession};
use crate::tools::{ChatToolRuntimeConfig, chat_tool_runtime};
use crate::{
    ChildRunPresentation, ModelAttemptOutcome, NoopTurnPresentationSink, PolicyPresentationOutcome,
    PresentationDelivery, ToolActionOutcome, ToolPresentationCompleteness, ToolPresentationDetail,
    ToolPresentationExecutionProfile, TurnPresentationEvent, TurnPresentationOutcome,
    TurnPresentationSink,
    presentation::{InferencePresentationSink, InferencePresentationTarget},
};

struct ToolCancellation(InferenceCancellation);

impl agl_kernel::CancellationSignal for ToolCancellation {
    fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
}

struct RuntimeToolEffectJournal {
    writer: RuntimeEventWriter,
    scope: EventScope,
    request_id: Option<RequestId>,
    caused_by: Option<EventId>,
    events: Vec<SafeRuntimeEventEnvelope>,
}

impl ToolEffectJournal for RuntimeToolEffectJournal {
    fn append(
        &mut self,
        record: &ToolEffectJournalRecord,
    ) -> std::result::Result<String, ToolEffectJournalError> {
        let mut draft = EventDraft::new(
            self.scope.clone(),
            RuntimeEvent::ToolEffectLifecycle {
                call_id: record.call_id().to_string(),
                tool_id: record.tool_id().as_str().to_owned(),
                extension_id: record.extension_id().as_str().to_owned(),
                schema_digest: record.schema_digest().as_str().to_owned(),
                delivery: record.delivery().as_str().to_owned(),
                state: record.state().as_str().to_owned(),
                admitted_effects: record
                    .admitted_effects()
                    .iter()
                    .map(|effect| effect.as_str().to_owned())
                    .collect(),
                observed_effects: record
                    .observed_effects()
                    .iter()
                    .map(|effect| agl_events::ObservedEffectEvent {
                        effect_id: effect.effect_id.as_str().to_owned(),
                        scope: effect.scope.clone(),
                    })
                    .collect(),
                outcome_code: record.outcome_code().map(str::to_owned),
            },
        );
        if let Some(request_id) = &self.request_id {
            draft = draft.with_request_id(request_id.clone());
        }
        if let Some(caused_by) = &self.caused_by {
            draft = draft.with_causation(caused_by.clone());
        }
        let (full, safe) = self
            .writer
            .append_with_full(draft)
            .map_err(|error| ToolEffectJournalError::new(format!("{error:#}")))?;
        self.caused_by = Some(full.event_id.clone());
        self.events.push(safe);
        Ok(full.event_id.to_string())
    }
}

struct ChatProcessExecutionContext {
    state: Arc<Mutex<agl_exec::ExecutionContextSnapshot>>,
    runs: Arc<dyn RunRepository>,
    sessions_root: PathBuf,
    scope_session_id: Option<SessionId>,
    persist_session_context: bool,
}

impl ChatProcessExecutionContext {
    fn admission_identity(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(agl_exec::ExecutionOwner, SessionId)> {
        let run = self
            .runs
            .run(scope.run_id())?
            .with_context(|| format!("execution owner run {} does not exist", scope.run_id()))?;
        match &self.scope_session_id {
            Some(session_id) => {
                ensure!(
                    scope.session_id() == Some(session_id)
                        && run.session_id.as_ref() == Some(session_id),
                    "process invocation session does not match its owning chat session"
                );
                Ok((
                    crate::execution_owner::session_owner(session_id, &run.root_run_id),
                    session_id.clone(),
                ))
            }
            None => {
                ensure!(
                    scope.session_id().is_none() && run.session_id.is_none(),
                    "run-owned process invocation unexpectedly carries session authority"
                );
                let root_run = self
                    .runs
                    .run(&run.root_run_id)?
                    .with_context(|| format!("root run {} does not exist", run.root_run_id))?;
                let durable_session_id = root_run.session_id.with_context(|| {
                    format!(
                        "root run {} has no durable session for subagent terminal ownership",
                        run.root_run_id
                    )
                })?;
                Ok((
                    crate::execution_owner::run_owner(scope.run_id(), &run.root_run_id),
                    durable_session_id,
                ))
            }
        }
    }

    fn snapshot(&self) -> Result<agl_exec::ExecutionContextSnapshot> {
        self.state
            .lock()
            .map_err(|error| anyhow::anyhow!("execution context lock is poisoned: {error}"))
            .map(|snapshot| snapshot.clone())
    }
}

impl agl_core_tools::ProcessExecutionContext for ChatProcessExecutionContext {
    fn load(&self, scope: &ExecutionScope) -> Result<agl_core_tools::ProcessExecutionAdmission> {
        let (owner, durable_session_id) = self.admission_identity(scope)?;
        Ok(agl_core_tools::ProcessExecutionAdmission {
            snapshot: self.snapshot()?,
            owner,
            durable_session_id,
        })
    }

    fn compare_and_set_working_directory(
        &self,
        scope: &ExecutionScope,
        expected_revision: u64,
        next: agl_exec::ExecutionContextSnapshot,
    ) -> Result<agl_core_tools::ProcessExecutionAdmission> {
        let (owner, durable_session_id) = self.admission_identity(scope)?;
        let current = self.snapshot()?;
        ensure!(
            current.revision == expected_revision,
            "execution context revision changed from expected {expected_revision} to {}",
            current.revision
        );
        let persisted = if self.persist_session_context {
            let session_id = self
                .scope_session_id
                .as_ref()
                .context("session execution persistence lacks a session identity")?;
            agl_session::ChatSessionStore::compare_and_set_execution_context_at(
                &self.sessions_root,
                session_id,
                expected_revision,
                next,
            )?
        } else {
            self.runs.compare_and_set_run_execution_context(
                scope.run_id(),
                expected_revision,
                &next,
            )?
        };
        *self
            .state
            .lock()
            .map_err(|error| anyhow::anyhow!("execution context lock is poisoned: {error}"))? =
            persisted.clone();
        Ok(agl_core_tools::ProcessExecutionAdmission {
            snapshot: persisted,
            owner,
            durable_session_id,
        })
    }
}

pub struct ChatTurnRuntime {
    session: InferenceSession,
    execution_context: agl_exec::ExecutionContextSnapshot,
    execution_context_state: Arc<Mutex<agl_exec::ExecutionContextSnapshot>>,
    process_tools: agl_core_tools::ProcessTools,
    active_effective_capabilities: Option<agl_kernel::EffectiveToolSet>,
    event_sink: Option<RuntimeEventWriter>,
    event_scope: Option<EventScope>,
    request_id: Option<RequestId>,
    runtime_events: Vec<SafeRuntimeEventEnvelope>,
    attempt_ids: Vec<AttemptId>,
    core_guards: agl_core_tools::guards::CoreGuards,
    core_tools: agl_core_tools::CoreTools,
    tool_runtime: ToolRuntime,
    generated_requests: usize,
    model_input_tokens: u64,
    model_output_tokens: u64,
    presentation_sink: Arc<dyn TurnPresentationSink>,
    presentation_session_id: SessionId,
    presentation_child_run: Option<ChildRunPresentation>,
    presentation_attempt_id: Option<AttemptId>,
    presentation_message_id: Option<agl_ids::MessageId>,
}

impl ChatTurnRuntime {
    pub fn new(
        session: InferenceSession,
        runtime: &agl_runtime::AgentLibreRuntimeConfig,
        workspace_root: impl AsRef<Path>,
        execution_context: agl_exec::ExecutionContextSnapshot,
        scope_session_id: Option<SessionId>,
        persist_session_context: bool,
    ) -> Result<Self> {
        let terminal_endpoint = runtime.execution.terminal_endpoint(&runtime.paths)?;
        Self::new_with_terminal_endpoint(
            session,
            runtime,
            workspace_root,
            execution_context,
            scope_session_id,
            persist_session_context,
            terminal_endpoint,
        )
    }

    pub(crate) fn new_with_terminal_endpoint(
        session: InferenceSession,
        runtime: &agl_runtime::AgentLibreRuntimeConfig,
        workspace_root: impl AsRef<Path>,
        execution_context: agl_exec::ExecutionContextSnapshot,
        scope_session_id: Option<SessionId>,
        persist_session_context: bool,
        terminal_endpoint: agl_process::TerminalEndpoint,
    ) -> Result<Self> {
        execution_context
            .validate()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        ensure!(
            execution_context.workspace_root == workspace_root.as_ref(),
            "execution context workspace does not match the chat workspace"
        );
        let core_tools = agl_core_tools::CoreTools::new(workspace_root.as_ref())
            .context("failed to initialize core filesystem tools")?;
        let execution_context_state = Arc::new(Mutex::new(execution_context.clone()));
        let process_context = Arc::new(ChatProcessExecutionContext {
            state: Arc::clone(&execution_context_state),
            runs: session.repositories().runs.clone(),
            sessions_root: runtime.paths.sessions_root(),
            scope_session_id,
            persist_session_context,
        });
        let process_tools = build_process_tools(runtime, process_context, terminal_endpoint)?;
        let tool_runtime = build_chat_tool_runtime(
            &session,
            &core_tools,
            workspace_root.as_ref(),
            &process_tools,
            &execution_context_state,
        )?;
        let presentation_session_id = session.session_id().clone();
        Ok(Self {
            session,
            execution_context,
            execution_context_state,
            process_tools,
            active_effective_capabilities: None,
            event_sink: None,
            event_scope: None,
            request_id: None,
            runtime_events: Vec::new(),
            attempt_ids: Vec::new(),
            core_guards: agl_core_tools::guards::CoreGuards::new(),
            core_tools,
            tool_runtime,
            generated_requests: 0,
            model_input_tokens: 0,
            model_output_tokens: 0,
            presentation_sink: Arc::new(NoopTurnPresentationSink),
            presentation_session_id,
            presentation_child_run: None,
            presentation_attempt_id: None,
            presentation_message_id: None,
        })
    }

    pub(crate) fn set_presentation_sink(&mut self, sink: Arc<dyn TurnPresentationSink>) {
        self.presentation_sink = sink;
    }

    pub(crate) fn set_presentation_context(
        &mut self,
        session_id: SessionId,
        child_run: ChildRunPresentation,
    ) {
        self.presentation_session_id = session_id;
        self.presentation_child_run = Some(child_run);
    }

    pub(crate) fn publish_turn_finished(
        &self,
        run_id: RunId,
        turn_id: TurnId,
        outcome: TurnPresentationOutcome,
    ) {
        self.presentation_sink
            .try_publish(TurnPresentationEvent::TurnFinished {
                session_id: self.presentation_session_id.clone(),
                run_id,
                turn_id,
                attempt_id: self.presentation_attempt_id.clone(),
                provisional_message_id: self.presentation_message_id.clone(),
                outcome,
                child_run: self.presentation_child_run.clone(),
            });
    }

    pub(crate) fn publish_final_assistant_message(
        &self,
        message_id: MessageId,
        content: agl_content::Content,
    ) {
        if self.presentation_child_run.is_some() {
            return;
        }
        let Some(scope) = self.event_scope.as_ref() else {
            return;
        };
        let Some(turn_id) = scope.turn_id().cloned() else {
            return;
        };
        self.presentation_sink
            .try_publish(TurnPresentationEvent::AssistantMessageFinal {
                session_id: self.presentation_session_id.clone(),
                run_id: scope.run_id().clone(),
                turn_id,
                attempt_id: self.presentation_attempt_id.clone(),
                message_id,
                content,
            });
    }

    pub(crate) fn publish_incomplete_assistant_message(
        &self,
        message_id: MessageId,
        content: agl_content::Content,
        source_attempt_id: AttemptId,
        reason: IncompleteOutputReason,
        continuation_index: u16,
    ) {
        if self.presentation_child_run.is_some() {
            return;
        }
        let Some(scope) = self.event_scope.as_ref() else {
            return;
        };
        let Some(turn_id) = scope.turn_id().cloned() else {
            return;
        };
        self.presentation_sink
            .try_publish(TurnPresentationEvent::AssistantMessageIncomplete {
                session_id: self.presentation_session_id.clone(),
                run_id: scope.run_id().clone(),
                turn_id,
                message_id,
                content,
                source_attempt_id,
                reason,
                continuation_index,
            });
    }

    pub fn session(&self) -> &InferenceSession {
        &self.session
    }

    pub(crate) fn session_mut(&mut self) -> &mut InferenceSession {
        &mut self.session
    }

    pub fn execution_context(&self) -> &agl_exec::ExecutionContextSnapshot {
        &self.execution_context
    }

    fn reconcile_process_grants(&self) -> Result<usize> {
        // Revocation is fenced atomically by `agl-terminald`; agentLIBRE no
        // longer enumerates or mutates canonical execution lifecycle here.
        Ok(0)
    }

    pub fn install_execution_context(
        &mut self,
        execution_context: agl_exec::ExecutionContextSnapshot,
    ) -> Result<()> {
        ensure!(
            self.active_effective_capabilities.is_none(),
            "cannot replace execution context during an active turn"
        );
        execution_context
            .validate()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        ensure!(
            execution_context.workspace_root == self.workspace_root(),
            "durable execution context workspace does not match the chat workspace"
        );
        *self
            .execution_context_state
            .lock()
            .map_err(|error| anyhow::anyhow!("execution context lock is poisoned: {error}"))? =
            execution_context.clone();
        self.execution_context = execution_context;
        Ok(())
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

    pub(crate) fn suspend_durable_turn(&mut self) {
        self.active_effective_capabilities = None;
        self.event_sink = None;
        self.event_scope = None;
        self.request_id = None;
        self.runtime_events.clear();
        self.attempt_ids.clear();
        self.presentation_attempt_id = None;
        self.presentation_message_id = None;
    }

    pub fn begin_turn(
        &mut self,
        session_id: Option<&SessionId>,
        run_id: &RunId,
        turn_id: &TurnId,
        request_id: Option<RequestId>,
    ) -> Result<()> {
        self.initialize_turn(session_id, run_id, turn_id, request_id, None, None)
    }

    pub(crate) fn resume_turn(
        &mut self,
        session_id: Option<&SessionId>,
        run_id: &RunId,
        turn_id: &TurnId,
        request_id: Option<RequestId>,
        durable_event_sequence: u64,
        delegation_authority_ceiling: BTreeSet<ToolId>,
    ) -> Result<()> {
        self.initialize_turn(
            session_id,
            run_id,
            turn_id,
            request_id,
            Some(durable_event_sequence),
            Some(delegation_authority_ceiling),
        )
    }

    fn initialize_turn(
        &mut self,
        session_id: Option<&SessionId>,
        run_id: &RunId,
        turn_id: &TurnId,
        request_id: Option<RequestId>,
        durable_event_sequence: Option<u64>,
        persisted_delegation_authority: Option<BTreeSet<ToolId>>,
    ) -> Result<()> {
        ensure!(
            self.active_effective_capabilities.is_none(),
            "cannot refresh runtime context during an active turn"
        );
        self.session
            .refresh_runtime_context(Some(run_id), Some(turn_id))?;
        self.reconcile_process_grants()?;
        self.session
            .freeze_delegation_authority(persisted_delegation_authority);
        self.rebuild_tool_runtime()?;
        self.active_effective_capabilities = Some(self.session.effective_capabilities().clone());
        self.event_sink = Some(match durable_event_sequence {
            Some(sequence) => RuntimeEventWriter::open_evidence_at_sequence(
                self.session.event_stream_path(run_id),
                run_id,
                sequence,
            )?,
            None => RuntimeEventWriter::open(self.session.event_stream_path(run_id))?,
        });
        let mut scope = EventScope::builder(run_id.clone()).turn_id(turn_id.clone());
        if let Some(session_id) = session_id {
            scope = scope.session_id(session_id.clone());
        }
        self.event_scope = Some(scope.build()?);
        self.request_id = request_id;
        self.generated_requests = 0;
        self.runtime_events.clear();
        self.attempt_ids.clear();
        self.presentation_attempt_id = None;
        self.presentation_message_id = None;
        if durable_event_sequence.is_none() {
            self.append_runtime_event(tool_policy_resolved_event(
                self.active_effective_capabilities
                    .as_ref()
                    .expect("active tool snapshot was just initialized"),
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
    pub(crate) fn active_policy_hash(&self) -> Option<&agl_kernel::PolicyHash> {
        self.active_effective_capabilities
            .as_ref()
            .map(agl_kernel::EffectiveToolSet::policy_hash)
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

    pub fn preflight_workspace_root(&self, workspace_root: impl AsRef<Path>) -> Result<()> {
        ensure!(
            self.active_effective_capabilities.is_none(),
            "cannot change workspace root during an active turn"
        );
        let core_tools = agl_core_tools::CoreTools::new(workspace_root.as_ref())
            .context("failed to validate core filesystem tool root")?;
        let mut session = self.session.clone();
        session.set_workspace_root_and_refresh(workspace_root.as_ref())?;
        build_chat_tool_runtime(
            &session,
            &core_tools,
            workspace_root.as_ref(),
            &self.process_tools,
            &self.execution_context_state,
        )?;
        Ok(())
    }

    pub fn set_workspace_root(&mut self, workspace_root: impl AsRef<Path>) -> Result<()> {
        ensure!(
            self.active_effective_capabilities.is_none(),
            "cannot change workspace root during an active turn"
        );
        let core_tools = agl_core_tools::CoreTools::new(workspace_root.as_ref())
            .context("failed to update core filesystem tool root")?;
        let mut session = self.session.clone();
        session.set_workspace_root_and_refresh(workspace_root.as_ref())?;
        let tool_runtime = build_chat_tool_runtime(
            &session,
            &core_tools,
            workspace_root.as_ref(),
            &self.process_tools,
            &self.execution_context_state,
        )?;
        self.session = session;
        self.core_tools = core_tools;
        self.tool_runtime = tool_runtime;
        Ok(())
    }

    pub fn reload_runtime_context(&mut self) -> Result<()> {
        ensure!(
            self.active_effective_capabilities.is_none(),
            "cannot reload runtime context during an active turn"
        );
        self.session.refresh_runtime_context(None, None)?;
        self.rebuild_tool_runtime()
    }

    pub fn refresh_runtime_context(&mut self, run_id: &RunId) -> Result<()> {
        ensure!(
            self.active_effective_capabilities.is_none(),
            "cannot refresh runtime context during an active turn"
        );
        self.session.refresh_runtime_context(Some(run_id), None)?;
        self.rebuild_tool_runtime()
    }

    pub(crate) fn rebuild_tool_runtime(&mut self) -> Result<()> {
        self.tool_runtime = build_chat_tool_runtime(
            &self.session,
            &self.core_tools,
            self.core_tools.root(),
            &self.process_tools,
            &self.execution_context_state,
        )?;
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
            .context("turn tool snapshot is not initialized")?
            .policy_hash()
            .as_str()
            .to_string())
    }

    pub(crate) fn tool_delivery_class(&self, tool_id: &agl_kernel::ToolId) -> Result<RunDelivery> {
        let tool = self
            .active_effective_capabilities
            .as_ref()
            .context("turn tool snapshot is not initialized")?
            .tool(tool_id)
            .context("pending tool is not in the effective turn snapshot")?;
        Ok(tool.declaration().delivery.into())
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
        provisional_message_id: MessageId,
        cancellation: InferenceCancellation,
        deadline: Option<Instant>,
    ) -> Result<ModelResponse> {
        self.generated_requests += 1;
        let (scope_session_id, request_id) = inference_correlation(
            self.event_scope.as_ref(),
            self.request_id.as_ref(),
            &request,
        )?;
        let attempt_id = AttemptId::generate();
        self.attempt_ids.push(attempt_id.clone());
        self.presentation_attempt_id = Some(attempt_id.clone());
        self.presentation_message_id = Some(provisional_message_id.clone());
        let presentation_session_id = self.presentation_session_id.clone();
        let run_id = request.run_id.clone();
        let turn_id = request.turn_id.clone();
        let started_delivery =
            self.presentation_sink
                .try_publish(TurnPresentationEvent::ModelAttemptStarted {
                    session_id: presentation_session_id.clone(),
                    run_id: run_id.clone(),
                    turn_id: turn_id.clone(),
                    attempt_id: attempt_id.clone(),
                    provisional_message_id: provisional_message_id.clone(),
                    child_run: self.presentation_child_run.clone(),
                });
        let output_sink: Arc<dyn InferenceOutputSink> = Arc::new(InferencePresentationSink::new(
            Arc::clone(&self.presentation_sink),
            InferencePresentationTarget {
                session_id: presentation_session_id,
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                attempt_id: attempt_id.clone(),
                provisional_message_id: provisional_message_id.clone(),
            },
            started_delivery == PresentationDelivery::Delivered,
            self.presentation_child_run.is_none(),
        ));
        let response = self
            .session
            .generate(
                request,
                attempt_id.clone(),
                scope_session_id,
                request_id,
                self.active_effective_capabilities
                    .as_ref()
                    .context("turn tool snapshot is not initialized")?,
                InferenceExecutionControl {
                    cancellation,
                    deadline,
                    output_sink,
                },
            )
            .and_then(|response| {
                ensure!(
                    response.attempt_id == attempt_id,
                    "inference response attempt ID does not match the admitted attempt"
                );
                Ok(response)
            });
        self.presentation_sink
            .try_publish(TurnPresentationEvent::ModelAttemptFinished {
                session_id: self.presentation_session_id.clone(),
                run_id,
                turn_id,
                attempt_id: attempt_id.clone(),
                provisional_message_id,
                outcome: match &response {
                    Ok(response)
                        if matches!(
                            response.finish_reason,
                            InferenceFinishReason::Length | InferenceFinishReason::ContentByteLimit
                        ) =>
                    {
                        ModelAttemptOutcome::Incomplete
                    }
                    Ok(_) => ModelAttemptOutcome::Completed,
                    Err(_) => ModelAttemptOutcome::Failed,
                },
            });
        let response = response?;
        self.model_input_tokens = self
            .model_input_tokens
            .saturating_add(response.metadata.input_tokens);
        self.model_output_tokens = self
            .model_output_tokens
            .saturating_add(response.metadata.output_tokens);
        Ok(ModelResponse {
            content: agl_content::Content::text(response.content)?,
            outcome: match response.finish_reason {
                InferenceFinishReason::Stop => ModelResponseOutcome::Complete,
                InferenceFinishReason::Length => ModelResponseOutcome::Incomplete {
                    reason: IncompleteOutputReason::ModelLength,
                },
                InferenceFinishReason::ContentByteLimit => ModelResponseOutcome::Incomplete {
                    reason: IncompleteOutputReason::ContentByteLimit,
                },
            },
        })
    }

    pub(crate) fn execute_tool(
        &mut self,
        request: ToolDispatchRequest,
        step_id: Option<&StepId>,
        cancellation: InferenceCancellation,
        deadline: Option<std::time::Instant>,
    ) -> Result<ToolDispatchResponse> {
        let run_id = request.run_id.clone();
        let turn_id = request.turn_id.clone();
        let tool_id = request.tool_id.clone();
        let arguments = request.arguments.clone();
        let presentation_step_id = step_id.cloned().unwrap_or_else(StepId::generate);
        self.presentation_sink
            .try_publish(TurnPresentationEvent::ToolActionStarted {
                session_id: self.presentation_session_id.clone(),
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                attempt_id: self.presentation_attempt_id.clone(),
                provisional_message_id: self.presentation_message_id.clone(),
                step_id: presentation_step_id.clone(),
                tool_id: tool_id.clone(),
            });
        let result = self.execute_tool_inner(
            request,
            step_id,
            &presentation_step_id,
            cancellation,
            deadline,
        );
        let outcome = match &result {
            Ok(response)
                if tool_id.as_str() == crate::delegation_contract::AGENT_DELEGATE_TOOL_ID
                    && response
                        .result
                        .data
                        .as_ref()
                        .and_then(|data| data.get("status"))
                        .and_then(serde_json::Value::as_str)
                        == Some("waiting") =>
            {
                ToolActionOutcome::Waiting
            }
            Ok(_) => ToolActionOutcome::Succeeded,
            Err(_) => ToolActionOutcome::Failed,
        };
        let detail = result.as_ref().ok().and_then(|response| {
            response
                .result
                .data
                .as_ref()
                .and_then(|data| tool_presentation_detail(tool_id.as_str(), &arguments, data))
        });
        self.presentation_sink
            .try_publish(TurnPresentationEvent::ToolActionFinished {
                session_id: self.presentation_session_id.clone(),
                run_id,
                turn_id,
                attempt_id: self.presentation_attempt_id.clone(),
                provisional_message_id: self.presentation_message_id.clone(),
                step_id: presentation_step_id,
                tool_id,
                outcome,
                detail,
            });
        result
    }

    fn execute_tool_inner(
        &mut self,
        request: ToolDispatchRequest,
        step_id: Option<&StepId>,
        presentation_step_id: &StepId,
        cancellation: InferenceCancellation,
        deadline: Option<std::time::Instant>,
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
            .context("turn tool snapshot is not initialized")?
            .clone();
        let policy_hash = effective.policy_hash().as_str().to_string();
        let tool_id = request.tool_id.clone();
        let Some(tool) = effective.tool(&tool_id).cloned() else {
            let denial = DispatchDenial {
                tool_id: tool_id.clone(),
                code: DispatchDenialCode::ToolNotEffective,
            };
            self.append_runtime_event(RuntimeEvent::ToolCallDenied {
                policy_hash,
                tool_id: Some(tool_id.as_str().to_string()),
                reason_code: denial.code.as_str().to_string(),
            })?;
            self.publish_policy_check(
                &request.run_id,
                &request.turn_id,
                presentation_step_id,
                tool_id,
                PolicyPresentationOutcome::Denied,
            );
            return Err(denial).context("tool dispatch was denied");
        };
        let scope = execution_scope(&active_scope, step_id)?;
        let mut invocation = ToolInvocation::new(
            scope,
            tool_id.clone(),
            tool.extension_id().clone(),
            tool.declaration_digest().clone(),
            effective.policy_hash().clone(),
            request.arguments.clone(),
        );
        if let Some(request_id) = &self.request_id {
            invocation = invocation.with_request_id(request_id.clone());
        }
        if let Err(denial) =
            effective.authorize(&invocation, self.tool_runtime.catalog().extensions())
        {
            self.append_runtime_event(RuntimeEvent::ToolCallDenied {
                policy_hash,
                tool_id: Some(tool_id.as_str().to_string()),
                reason_code: denial.code.as_str().to_string(),
            })?;
            self.publish_policy_check(
                &request.run_id,
                &request.turn_id,
                presentation_step_id,
                tool_id,
                PolicyPresentationOutcome::Denied,
            );
            return Err(denial).context("tool dispatch was denied");
        }
        self.publish_policy_check(
            &request.run_id,
            &request.turn_id,
            presentation_step_id,
            tool_id.clone(),
            PolicyPresentationOutcome::Allowed,
        );
        self.append_runtime_event(RuntimeEvent::ToolCallAdmitted {
            policy_hash,
            tool_id: tool_id.as_str().to_string(),
            extension_id: tool.extension_id().as_str().to_string(),
            declaration_digest: tool.declaration_digest().as_str().to_string(),
        })?;
        self.session.prepare_artifact_write_for_tool(
            &request.run_id,
            request.tool_id.as_str(),
            &request.arguments,
        )?;
        let mut effect_journal = RuntimeToolEffectJournal {
            writer: self
                .event_sink
                .as_ref()
                .context("turn event writer is not initialized")?
                .clone(),
            scope: active_scope,
            request_id: self.request_id.clone(),
            caused_by: self
                .runtime_events
                .last()
                .map(|event| event.event_id.clone()),
            events: Vec::new(),
        };
        let dispatched = self.tool_runtime.dispatch_with_journal(
            invocation,
            &effective,
            agl_kernel::ToolDispatchControl::new(
                std::sync::Arc::new(ToolCancellation(cancellation)),
                deadline,
            ),
            &mut effect_journal,
        );
        self.runtime_events.extend(effect_journal.events);
        let output = dispatched.map_err(|error| {
            anyhow::Error::new(error).context(format!("tool `{}` failed", request.tool_id))
        })?;
        self.execution_context = self
            .execution_context_state
            .lock()
            .map_err(|error| anyhow::anyhow!("execution context lock is poisoned: {error}"))?
            .clone();
        Ok(ToolDispatchResponse { result: output })
    }

    fn publish_policy_check(
        &self,
        run_id: &RunId,
        turn_id: &TurnId,
        step_id: &StepId,
        tool_id: ToolId,
        outcome: PolicyPresentationOutcome,
    ) {
        self.presentation_sink
            .try_publish(TurnPresentationEvent::PolicyCheck {
                session_id: self.presentation_session_id.clone(),
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                attempt_id: self.presentation_attempt_id.clone(),
                step_id: step_id.clone(),
                tool_id,
                outcome,
            });
    }
}

fn tool_presentation_detail(
    tool_id: &str,
    arguments: &serde_json::Value,
    result: &serde_json::Value,
) -> Option<ToolPresentationDetail> {
    match tool_id {
        agl_core_tools::FS_LIST_TOOL_ID => {
            let path = safe_repository_path(result.get("path")?.as_str()?)?;
            let entries = u32::try_from(result.get("entry_count")?.as_u64()?).ok()?;
            let completeness = match result.get("outcome")?.get("state")?.as_str()? {
                "complete" => ToolPresentationCompleteness::Complete,
                "truncated" => ToolPresentationCompleteness::Truncated,
                _ => return None,
            };
            Some(ToolPresentationDetail::FilesystemList {
                path,
                entries,
                completeness,
            })
        }
        agl_core_tools::FS_READ_TOOL_ID => {
            let path = safe_repository_path(result.get("path")?.as_str()?)?;
            let bytes = result
                .get("lines")?
                .as_array()?
                .iter()
                .filter_map(|line| line.get("text").and_then(serde_json::Value::as_str))
                .fold(0u64, |total, line| {
                    total.saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX))
                });
            Some(ToolPresentationDetail::FilesystemRead { path, bytes })
        }
        agl_core_tools::FS_SEARCH_TOOL_ID => {
            let scope = safe_repository_path(result.get("path")?.as_str()?)?;
            let matches = u32::try_from(result.get("match_count")?.as_u64()?).ok()?;
            let complete = !result.get("truncated")?.as_bool()?;
            Some(ToolPresentationDetail::RepositorySearch {
                scope,
                matches,
                complete,
            })
        }
        agl_core_tools::PROCESS_EXEC_TOOL_ID
        | agl_core_tools::PROCESS_START_TOOL_ID
        | agl_core_tools::SHELL_EXEC_TOOL_ID => {
            let profile = match arguments.get("profile").and_then(serde_json::Value::as_str) {
                Some("host") => ToolPresentationExecutionProfile::Host,
                None | Some("workspace") => ToolPresentationExecutionProfile::Workspace,
                Some(_) => return None,
            };
            let exit_status = result
                .get("exit")
                .and_then(serde_json::Value::as_object)
                .filter(|exit| exit.get("kind").and_then(serde_json::Value::as_str) == Some("code"))
                .and_then(|exit| exit.get("code"))
                .and_then(serde_json::Value::as_i64)
                .and_then(|code| i32::try_from(code).ok());
            Some(ToolPresentationDetail::ProcessExecution {
                profile,
                exit_status,
            })
        }
        _ => None,
    }
}

fn safe_repository_path(path: &str) -> Option<String> {
    if path == "." {
        return Some("workspace".to_owned());
    }
    if path.is_empty()
        || path.len() > 4_096
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || path
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return None;
    }
    Some(path.to_owned())
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

fn tool_policy_resolved_event(effective: &agl_kernel::EffectiveToolSet) -> RuntimeEvent {
    RuntimeEvent::ToolPolicyResolved {
        policy_hash: effective.policy_hash().as_str().to_string(),
        tool_ids: effective
            .tools()
            .map(|tool| tool.declaration().id.as_str().to_string())
            .collect(),
        exclusions: effective
            .exclusions()
            .map(|exclusion| ToolExclusionEvent {
                tool_id: exclusion.tool_id.as_str().to_string(),
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
        .context("active event scope is invalid for tool invocation")
}

fn missing_hook_result(hook_id: agl_kernel::HookId) -> HookResult {
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
) -> agl_core_tools::PermissionRuntimeStatus {
    agl_core_tools::PermissionRuntimeStatus {
        current_mode: session.tool_mode().as_str().to_string(),
        visible_tools: session
            .turn_visible_tools()
            .iter()
            .map(|tool| tool.id.as_str().to_string())
            .collect(),
        dynamic_grants: session.dynamic_grants_enabled(),
        granted_visible_tools: session.permission_grants().granted_visible_tools(),
        ignored_grants: session.permission_grants().ignored_grants(),
    }
}

fn build_process_tools(
    runtime: &agl_runtime::AgentLibreRuntimeConfig,
    context: Arc<dyn agl_core_tools::ProcessExecutionContext>,
    terminal_endpoint: agl_process::TerminalEndpoint,
) -> Result<agl_core_tools::ProcessTools> {
    agl_core_tools::ProcessTools::new(
        Arc::new(terminal_endpoint),
        context,
        agl_core_tools::ProcessToolRuntimeConfig {
            base_environment: runtime.execution.admitted_environment()?,
            maximum_environment_bytes: runtime.execution.environment.maximum_bytes,
            runtime_read_only_roots: runtime.execution.runtime_read_only_roots.clone(),
            default_foreground_timeout: Duration::from_millis(
                runtime.execution.default_foreground_timeout_ms,
            ),
            maximum_foreground_timeout: Duration::from_millis(
                runtime.execution.maximum_foreground_timeout_ms,
            ),
            max_input_bytes: runtime.execution.max_input_bytes,
            max_result_bytes: runtime.execution.max_result_bytes,
            max_spool_bytes: runtime.execution.max_spool_bytes,
            default_terminal_size: agl_exec::TerminalSize {
                columns: runtime.execution.default_terminal_columns,
                rows: runtime.execution.default_terminal_rows,
            },
        },
    )
}

fn build_chat_tool_runtime(
    session: &InferenceSession,
    core_tools: &agl_core_tools::CoreTools,
    workspace_root: &Path,
    process_tools: &agl_core_tools::ProcessTools,
    execution_context_state: &Arc<Mutex<agl_exec::ExecutionContextSnapshot>>,
) -> Result<ToolRuntime> {
    let screen_id = agl_kernel::ToolId::new(agl_host_tools::SCREEN_CAPTURE_TOOL_ID)?;
    chat_tool_runtime(ChatToolRuntimeConfig {
        core_tools,
        repositories: session.repositories(),
        trust_store_path: session.trust_store_path(),
        workspace_root,
        runtime_paths: session.runtime_paths(),
        permission_status: permission_runtime_status(session),
        process_tools: Some(process_tools.clone()),
        screen_admitted_run: session
            .permission_grants()
            .sensitive_input_run(&screen_id, agl_kernel::SensitiveInput::ScreenCapture)
            .cloned(),
        delegation_handler: crate::delegation::DelegationHandler::from_session(
            session,
            Arc::clone(execution_context_state),
        ),
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

    #[test]
    fn tool_presentation_details_are_closed_redacted_facts() {
        let list = tool_presentation_detail(
            agl_core_tools::FS_LIST_TOOL_ID,
            &serde_json::json!({"path": "IGNORED_ARGUMENT_SENTINEL"}),
            &serde_json::json!({
                "path": ".",
                "entry_count": 17,
                "entries": [{"name": "PRIVATE_ENTRY_SENTINEL"}],
                "outcome": {"state": "truncated", "next_cursor": "PRIVATE_CURSOR"},
            }),
        );
        assert_eq!(
            list,
            Some(ToolPresentationDetail::FilesystemList {
                path: "workspace".to_owned(),
                entries: 17,
                completeness: ToolPresentationCompleteness::Truncated,
            })
        );

        let read = tool_presentation_detail(
            agl_core_tools::FS_READ_TOOL_ID,
            &serde_json::json!({"path": "ignored"}),
            &serde_json::json!({
                "path": "src/lib.rs",
                "lines": [
                    {"line": 1, "text": "PRIVATE_FILE_SENTINEL"},
                    {"line": 2, "text": "ok"},
                ],
            }),
        );
        assert_eq!(
            read,
            Some(ToolPresentationDetail::FilesystemRead {
                path: "src/lib.rs".to_owned(),
                bytes: 23,
            })
        );

        let search = tool_presentation_detail(
            agl_core_tools::FS_SEARCH_TOOL_ID,
            &serde_json::json!({"pattern": "PRIVATE_PATTERN_SENTINEL"}),
            &serde_json::json!({
                "path": "crates",
                "match_count": 4,
                "truncated": false,
                "matches": [{"text": "PRIVATE_MATCH_SENTINEL"}],
            }),
        );
        assert_eq!(
            search,
            Some(ToolPresentationDetail::RepositorySearch {
                scope: "crates".to_owned(),
                matches: 4,
                complete: true,
            })
        );

        let process = tool_presentation_detail(
            agl_core_tools::PROCESS_EXEC_TOOL_ID,
            &serde_json::json!({
                "program": "/PRIVATE/HOST/PROGRAM_SENTINEL",
                "args": ["PRIVATE_ARG_SENTINEL"],
                "env": {"TOKEN": "PRIVATE_SECRET_SENTINEL"},
                "profile": "host",
            }),
            &serde_json::json!({
                "exit": {"kind": "code", "code": 7},
                "chunks": [{"bytes": "PRIVATE_OUTPUT_SENTINEL"}],
            }),
        );
        assert_eq!(
            process,
            Some(ToolPresentationDetail::ProcessExecution {
                profile: ToolPresentationExecutionProfile::Host,
                exit_status: Some(7),
            })
        );

        let safe = format!("{list:?}{read:?}{search:?}{process:?}");
        for sentinel in [
            "IGNORED_ARGUMENT_SENTINEL",
            "PRIVATE_ENTRY_SENTINEL",
            "PRIVATE_CURSOR",
            "PRIVATE_FILE_SENTINEL",
            "PRIVATE_PATTERN_SENTINEL",
            "PRIVATE_MATCH_SENTINEL",
            "PRIVATE_SECRET_SENTINEL",
            "PRIVATE_OUTPUT_SENTINEL",
            "/PRIVATE/HOST/PROGRAM_SENTINEL",
        ] {
            assert!(!safe.contains(sentinel));
        }
    }

    #[test]
    fn tool_presentation_paths_fail_closed_on_host_or_unnormalized_values() {
        for path in [
            "/home/user/private",
            "../private",
            "safe/../private",
            "safe\\private",
            "safe\nprivate",
        ] {
            assert_eq!(
                tool_presentation_detail(
                    agl_core_tools::FS_READ_TOOL_ID,
                    &serde_json::json!({}),
                    &serde_json::json!({"path": path, "lines": []}),
                ),
                None
            );
        }
    }
}
