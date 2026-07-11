use std::path::{Path, PathBuf};
use std::time::Instant;

use agl_content::{Content, ContentPart};
use agl_events::{RuntimeEvent, SafeRuntimeEventEnvelope};
use agl_ids::{AttemptId, MessageId, RequestId, RunId, SessionId, StepId, TurnId};
use agl_inference::{InferenceCancellation, ModelManagerError};
use agl_loop::{
    EffectFailure, EffectFailureCode, EffectOutcome, HookEffectOutput, TurnAdvance,
    TurnAdvanceState, TurnCheckpoint, TurnEffect, TurnEffectResult, TurnExecutor, TurnInput,
    TurnOutput, TurnTerminal,
};
use agl_runtime::{AgentLibreRuntimeConfig, logged_message_fields};
use agl_session::{ChatSessionEvent, ChatSessionReplay, ChatSessionStore};
use agl_turn::{StopDetail, StopReason, TurnHookBatch, TurnMessage, VisibleTool};
use anyhow::{Context, Result, bail};

use crate::{
    ChatOptions, ChatTurnRuntime, InferenceClientHandle, InferenceSession, ToolAccessMode,
    assistant_text_for_terminal,
};

const MAX_TOOL_CALLS_PER_TURN: usize = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatSessionSummary {
    pub session_id: SessionId,
    pub artifact_root: PathBuf,
    pub workspace_root: PathBuf,
    pub tool_mode: &'static str,
    pub history_enabled: bool,
    pub resumed: bool,
    pub replayed_messages: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatTurnOutput {
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub attempt_ids: Vec<AttemptId>,
    pub runtime_events: Vec<SafeRuntimeEventEnvelope>,
    pub status: ChatTurnStatus,
    pub generated_requests: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChatTurnStatus {
    Answered { answer: String },
    Stopped { reason: StopReason },
    Failed { message: String },
    Cancelled,
}

pub struct ChatTurnExecution {
    run_id: RunId,
    turn_id: TurnId,
    executor: TurnExecutor,
    advance: TurnAdvance,
    cancellation: InferenceCancellation,
    deadline: Option<Instant>,
    previous_message_count: usize,
    attempt_ids: Vec<AttemptId>,
    emitted_events: Vec<SafeRuntimeEventEnvelope>,
    event_sequence: u64,
    output: Option<ChatTurnOutput>,
}

impl ChatTurnExecution {
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }

    pub fn turn_id(&self) -> &TurnId {
        &self.turn_id
    }

    pub fn checkpoint(&self) -> TurnCheckpoint {
        self.executor.checkpoint()
    }

    pub fn pending_effect(&self) -> Option<&TurnEffect> {
        match &self.advance.state {
            TurnAdvanceState::Pending { effect } => Some(effect),
            TurnAdvanceState::Terminal { .. } => None,
        }
    }

    pub fn cancellation_handle(&self) -> InferenceCancellation {
        self.cancellation.clone()
    }

    pub fn request_cancellation(&mut self) -> Result<()> {
        self.cancellation.cancel();
        self.executor.request_cancellation()?;
        Ok(())
    }

    pub fn set_deadline(&mut self, deadline: Instant) {
        self.deadline = Some(deadline);
    }

    pub fn take_events(&mut self) -> Vec<SafeRuntimeEventEnvelope> {
        std::mem::take(&mut self.emitted_events)
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self.advance.state, TurnAdvanceState::Terminal { .. })
    }

    pub fn take_output(&mut self) -> Option<ChatTurnOutput> {
        self.output.take()
    }
}

pub(crate) struct TurnInputSpec<'a> {
    run_id: &'a RunId,
    turn_id: &'a TurnId,
    request_index: usize,
    context_messages: &'a [TurnMessage],
    hook_batches: &'a [TurnHookBatch],
    hook_payload: serde_json::Value,
    max_hook_repair_attempts: usize,
    visible_tools: &'a [VisibleTool],
    capability_policy_hash: Option<String>,
    user_input: &'a Content,
}

pub struct ChatService {
    runtime: AgentLibreRuntimeConfig,
    session_id: SessionId,
    tool_mode: ToolAccessMode,
    history_enabled: bool,
    resumed_session: bool,
    chat_history: Option<ChatSessionStore>,
    turn_runtime: ChatTurnRuntime,
    messages: Vec<TurnMessage>,
    context_released: bool,
    session_finished: bool,
}

impl ChatService {
    pub fn open(
        options: ChatOptions,
        runtime: &AgentLibreRuntimeConfig,
        inference_client: InferenceClientHandle,
    ) -> Result<Self> {
        if options.new_session && options.session_id.is_some() {
            bail!("new session cannot be requested with a specific session id");
        }

        let history_enabled = runtime.history.enabled && !options.no_history;
        let session_id = if options.new_session {
            SessionId::generate()
        } else if let Some(session_id) = &options.session_id {
            session_id.clone()
        } else {
            SessionId::generate()
        };
        let resumed_session = history_enabled
            && !options.new_session
            && options.session_id.is_some()
            && ChatSessionStore::exists(runtime.paths.sessions_root(), &session_id);
        let explicit_artifact_root = InferenceSession::resolve_artifact_root(&options.inference);
        let workspace_root = runtime.resolve_workspace_root(options.workspace_root.as_deref())?;
        let tool_mode = options.inference.tool_mode;
        let artifact_root_override = if history_enabled {
            explicit_artifact_root.or_else(|| Some(runtime.paths.session_dir(&session_id)))
        } else {
            explicit_artifact_root
        };
        let session = InferenceSession::new(
            options.inference,
            runtime,
            artifact_root_override,
            session_id.clone(),
            inference_client,
        )?;
        let (chat_history, replay) = if history_enabled {
            if resumed_session {
                let history =
                    ChatSessionStore::open(runtime.paths.sessions_root(), session_id.clone())?;
                let replay = history.read_replay()?;
                (Some(history), Some(replay))
            } else {
                (
                    Some(ChatSessionStore::start(
                        runtime.paths.sessions_root(),
                        session_id.clone(),
                        session.config_path().to_path_buf(),
                        session.backend_name(),
                    )?),
                    None,
                )
            }
        } else {
            (None, None)
        };
        let turn_runtime = ChatTurnRuntime::new(session, &workspace_root)?;
        let messages = replay
            .as_ref()
            .map(replay_turn_messages)
            .unwrap_or_default();
        Ok(Self {
            runtime: runtime.clone(),
            session_id,
            tool_mode,
            history_enabled,
            resumed_session,
            chat_history,
            turn_runtime,
            messages,
            context_released: false,
            session_finished: false,
        })
    }

    pub fn summary(&self) -> ChatSessionSummary {
        ChatSessionSummary {
            session_id: self.session_id.clone(),
            artifact_root: self.turn_runtime.session().artifact_root().to_path_buf(),
            workspace_root: self.turn_runtime.workspace_root().to_path_buf(),
            tool_mode: self.tool_mode.as_str(),
            history_enabled: self.history_enabled,
            resumed: self.resumed_session,
            replayed_messages: self.messages.len(),
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn artifact_root(&self) -> &Path {
        self.turn_runtime.session().artifact_root()
    }

    pub fn workspace_root(&self) -> &Path {
        self.turn_runtime.workspace_root()
    }

    pub(crate) fn capability_delivery_class(
        &self,
        capability_id: &agl_capabilities::CapabilityId,
    ) -> Result<agl_store::EffectDeliveryClass> {
        self.turn_runtime.capability_delivery_class(capability_id)
    }

    pub(crate) fn model_token_usage(&self) -> (u64, u64) {
        self.turn_runtime.model_token_usage()
    }

    pub fn set_workspace_root(&mut self, workspace_root: impl AsRef<Path>) -> Result<()> {
        if self.context_released {
            bail!("cannot change workspace root after the chat session context was released");
        }
        self.turn_runtime.set_workspace_root(workspace_root)
    }

    pub fn reload_runtime_context(&mut self) -> Result<usize> {
        if self.context_released {
            bail!("cannot reload a released chat session context");
        }
        self.turn_runtime.reload_runtime_context()?;
        Ok(self.turn_runtime.session().turn_visible_tools().len())
    }

    pub fn clear_context(&mut self) -> Result<usize> {
        if self.context_released {
            bail!("cannot clear a released chat session context");
        }
        let cleared_messages = self.messages.len();
        self.turn_runtime.clear_context()?;
        self.messages.clear();
        if let Some(history) = &mut self.chat_history {
            history.append_context_cleared()?;
        }
        Ok(cleared_messages)
    }

    pub fn request_exit(&mut self) -> Result<()> {
        if self.session_finished {
            return Ok(());
        }
        self.release_inference_context()?;
        if let Some(history) = &mut self.chat_history {
            history.request_exit()?;
        }
        self.session_finished = true;
        Ok(())
    }

    pub fn finish_eof_if_needed(&mut self) -> Result<()> {
        if self.session_finished {
            return Ok(());
        }
        self.release_inference_context()?;
        if let Some(history) = &mut self.chat_history {
            history.finish_eof()?;
        }
        self.session_finished = true;
        Ok(())
    }

    fn release_inference_context(&mut self) -> Result<()> {
        if self.context_released {
            return Ok(());
        }
        self.turn_runtime.release_context()?;
        self.context_released = true;
        Ok(())
    }

    #[cfg(test)]
    pub fn run_user_turn(&mut self, input: &str) -> Result<ChatTurnOutput> {
        self.run_user_turn_with_ids(RunId::generate(), TurnId::generate(), None, input)
    }

    #[cfg(test)]
    pub fn run_user_turn_with_ids(
        &mut self,
        run_id: RunId,
        turn_id: TurnId,
        request_id: Option<RequestId>,
        input: &str,
    ) -> Result<ChatTurnOutput> {
        let mut execution =
            self.start_user_turn_with_ids(run_id, turn_id, request_id, Content::text(input)?)?;
        while !execution.is_terminal() {
            if let Err(error) = self.advance_user_turn(&mut execution) {
                let attempt_ids = execution.attempt_ids.clone();
                let generated_requests = self.turn_runtime.generated_requests();
                return self.finish_failed_turn(
                    execution.run_id.clone(),
                    execution.turn_id.clone(),
                    attempt_ids,
                    generated_requests,
                    format!("turn driver failed: {error:#}"),
                    true,
                );
            }
        }
        execution
            .take_output()
            .context("terminal chat execution has no output")
    }

    pub fn start_user_turn_with_ids(
        &mut self,
        run_id: RunId,
        turn_id: TurnId,
        request_id: Option<RequestId>,
        input: Content,
    ) -> Result<ChatTurnExecution> {
        if self.context_released {
            bail!("cannot run a turn after the chat session context was released");
        }
        self.turn_runtime
            .begin_turn(&self.session_id, &run_id, &turn_id, request_id)?;
        let user_message_id = MessageId::generate();
        log_message_metadata(
            "user",
            &self.session_id,
            &user_message_id,
            &input,
            &self.runtime,
        );
        let envelope = self
            .turn_runtime
            .append_runtime_event(RuntimeEvent::UserMessage {
                message_id: user_message_id,
                content: input.clone(),
            })?;
        if let Some(history) = &mut self.chat_history
            && let Err(error) = history.append_user_message(envelope)
        {
            bail!("failed to record user message: {error:#}");
        }
        let capability_policy_hash = Some(self.turn_runtime.policy_hash()?);
        let turn_input = build_turn_input(TurnInputSpec {
            run_id: &run_id,
            turn_id: &turn_id,
            request_index: 0,
            context_messages: &self.messages,
            hook_batches: self.turn_runtime.session().turn_hook_batches(),
            hook_payload: self.turn_runtime.session().turn_hook_payload(),
            max_hook_repair_attempts: self.turn_runtime.session().max_hook_repair_attempts(),
            visible_tools: self.turn_runtime.session().turn_visible_tools(),
            capability_policy_hash,
            user_input: &input,
        });
        let previous_message_count = self.messages.len();
        let mut executor = TurnExecutor::new(turn_input);
        let advance = executor.next_effect()?;
        self.turn_runtime
            .append_executor_events(advance.events.clone())?;
        let emitted_events = self.turn_runtime.read_runtime_events_after(0)?;
        let event_sequence = emitted_events.last().map_or(0, |event| event.sequence);
        Ok(ChatTurnExecution {
            run_id,
            turn_id,
            executor,
            advance,
            cancellation: InferenceCancellation::new(),
            deadline: None,
            previous_message_count,
            attempt_ids: Vec::new(),
            emitted_events,
            event_sequence,
            output: None,
        })
    }

    pub fn resume_user_turn_from_checkpoint(
        &mut self,
        run_id: RunId,
        turn_id: TurnId,
        request_id: Option<RequestId>,
        checkpoint: TurnCheckpoint,
        event_sequence: u64,
        attempt_ids: Vec<AttemptId>,
    ) -> Result<ChatTurnExecution> {
        if self.context_released {
            bail!("cannot resume a turn after the chat session context was released");
        }
        if checkpoint.state().input.run_id != run_id || checkpoint.state().input.turn_id != turn_id
        {
            bail!("turn checkpoint identity does not match the durable run");
        }
        self.turn_runtime.resume_turn(
            &self.session_id,
            &run_id,
            &turn_id,
            request_id,
            event_sequence,
        )?;
        let previous_message_count = checkpoint.state().input.context_messages.len();
        let mut executor = TurnExecutor::from_checkpoint(checkpoint)?;
        let advance = executor.next_effect()?;
        let mut execution = ChatTurnExecution {
            run_id,
            turn_id,
            executor,
            advance,
            cancellation: InferenceCancellation::new(),
            deadline: None,
            previous_message_count,
            attempt_ids,
            emitted_events: Vec::new(),
            event_sequence,
            output: None,
        };
        if execution.is_terminal() {
            self.finish_execution(&mut execution)?;
        }
        Ok(execution)
    }

    #[cfg(test)]
    pub fn advance_user_turn(&mut self, execution: &mut ChatTurnExecution) -> Result<()> {
        let result = self.execute_user_turn_effect(execution)?;
        self.resume_user_turn_effect(execution, result)
    }

    pub fn execute_user_turn_effect(
        &mut self,
        execution: &mut ChatTurnExecution,
    ) -> Result<TurnEffectResult> {
        self.execute_user_turn_effect_with_step(execution, None)
    }

    pub(crate) fn execute_user_turn_effect_with_step(
        &mut self,
        execution: &mut ChatTurnExecution,
        step_id: Option<&StepId>,
    ) -> Result<TurnEffectResult> {
        if execution.is_terminal() {
            bail!("cannot execute an effect for a terminal chat execution");
        }
        let effect = match &execution.advance.state {
            TurnAdvanceState::Pending { effect } => effect.clone(),
            TurnAdvanceState::Terminal { .. } => unreachable!("terminal state was rejected above"),
        };
        let result = self.execute_turn_effect(execution, effect, step_id);
        execution
            .attempt_ids
            .extend(self.turn_runtime.take_attempt_ids());
        self.collect_execution_events(execution)?;
        Ok(result)
    }

    pub fn resume_user_turn_effect(
        &mut self,
        execution: &mut ChatTurnExecution,
        result: TurnEffectResult,
    ) -> Result<()> {
        if execution.is_terminal() {
            bail!("cannot resume a terminal chat execution");
        }
        let advance = execution.executor.resume(result)?;
        if matches!(
            advance.state,
            TurnAdvanceState::Terminal {
                terminal: TurnTerminal::Failed { .. }
            }
        ) {
            self.link_failed_attempts(&execution.attempt_ids, "turn effect failed")?;
        }
        self.turn_runtime
            .append_executor_events(advance.events.clone())?;
        execution.advance = advance;
        self.collect_execution_events(execution)?;
        if matches!(execution.advance.state, TurnAdvanceState::Terminal { .. }) {
            self.finish_execution(execution)?;
        }
        Ok(())
    }

    fn execute_turn_effect(
        &mut self,
        execution: &mut ChatTurnExecution,
        effect: TurnEffect,
        step_id: Option<&StepId>,
    ) -> TurnEffectResult {
        if execution.cancellation.is_cancelled() {
            return cancelled_effect_result(effect);
        }
        match effect {
            TurnEffect::HookBatch { key, request } => {
                let started = Instant::now();
                let outcome = match self.turn_runtime.execute_hooks(request) {
                    Ok(result) => EffectOutcome::Succeeded(HookEffectOutput {
                        result,
                        duration_ms: u64::try_from(started.elapsed().as_millis()).ok(),
                    }),
                    Err(error) => EffectOutcome::Failed(EffectFailure::new(
                        EffectFailureCode::Hook,
                        format!("{error:#}"),
                        false,
                    )),
                };
                TurnEffectResult::HookBatch { key, outcome }
            }
            TurnEffect::ModelGeneration { key, request } => {
                let outcome = match self.turn_runtime.execute_model(
                    request,
                    execution.cancellation.clone(),
                    execution.deadline,
                ) {
                    Ok(response) => EffectOutcome::Succeeded(response),
                    Err(error) => model_effect_failure(error),
                };
                TurnEffectResult::ModelGeneration { key, outcome }
            }
            TurnEffect::CapabilityDispatch { key, request } => {
                let outcome = match self.turn_runtime.execute_capability(request, step_id) {
                    Ok(response) => EffectOutcome::Succeeded(response),
                    Err(error) => EffectOutcome::Failed(EffectFailure::new(
                        EffectFailureCode::Capability,
                        format!("{error:#}"),
                        false,
                    )),
                };
                TurnEffectResult::CapabilityDispatch { key, outcome }
            }
            TurnEffect::TranscriptAppend {
                key,
                messages,
                output,
            } => {
                let outcome = match self.record_transcript_effect(execution, messages, &output) {
                    Ok(()) => EffectOutcome::Succeeded(()),
                    Err(error) => EffectOutcome::Failed(EffectFailure::new(
                        EffectFailureCode::Transcript,
                        format!("{error:#}"),
                        false,
                    )),
                };
                TurnEffectResult::TranscriptAppend { key, outcome }
            }
        }
    }

    fn record_transcript_effect(
        &mut self,
        execution: &ChatTurnExecution,
        mut messages: Vec<TurnMessage>,
        output: &TurnOutput,
    ) -> Result<()> {
        let stop_reason = match output {
            TurnOutput::Answered { answer } => {
                ensure_final_assistant_message(
                    &mut messages,
                    Content::text(assistant_text_for_terminal(answer))?,
                );
                None
            }
            TurnOutput::Stopped { reason, detail } => {
                messages.push(TurnMessage::Assistant {
                    content: Content::text(stopped_turn_context_message(
                        *reason,
                        detail.as_ref(),
                        self.turn_runtime.session().turn_visible_tools(),
                    ))?,
                });
                Some(*reason)
            }
        };
        let mut recording = CompletedTurnRecording {
            session_id: &self.session_id,
            remaining_attempt_ids: execution.attempt_ids.iter(),
            runtime: &self.runtime,
        };
        record_completed_turn_messages(
            &mut self.chat_history,
            &mut self.turn_runtime,
            &mut recording,
            messages
                .get(execution.previous_message_count..)
                .context("turn transcript is shorter than prior chat context")?,
            stop_reason,
        )?;
        self.messages = messages;
        Ok(())
    }

    fn collect_execution_events(&mut self, execution: &mut ChatTurnExecution) -> Result<()> {
        let events = self
            .turn_runtime
            .read_runtime_events_after(execution.event_sequence)?;
        if let Some(last) = events.last() {
            execution.event_sequence = last.sequence;
        }
        execution.emitted_events.extend(events);
        Ok(())
    }

    fn finish_execution(&mut self, execution: &mut ChatTurnExecution) -> Result<()> {
        let terminal = match &execution.advance.state {
            TurnAdvanceState::Terminal { terminal } => terminal.clone(),
            TurnAdvanceState::Pending { .. } => bail!("chat execution still has a pending effect"),
        };
        let generated_requests = self.turn_runtime.generated_requests();
        let output = match terminal {
            TurnTerminal::Completed { output } => {
                let status = match output {
                    TurnOutput::Answered { answer } => ChatTurnStatus::Answered {
                        answer: assistant_text_for_terminal(&answer),
                    },
                    TurnOutput::Stopped { reason, .. } => ChatTurnStatus::Stopped { reason },
                };
                let runtime_events = self.turn_runtime.take_runtime_events()?;
                ChatTurnOutput {
                    run_id: execution.run_id.clone(),
                    turn_id: execution.turn_id.clone(),
                    attempt_ids: execution.attempt_ids.clone(),
                    runtime_events,
                    status,
                    generated_requests,
                }
            }
            TurnTerminal::Failed { failure } => self.finish_failed_turn(
                execution.run_id.clone(),
                execution.turn_id.clone(),
                execution.attempt_ids.clone(),
                generated_requests,
                failure.message,
                false,
            )?,
            TurnTerminal::Cancelled => {
                self.messages = execution.executor.checkpoint().state().messages.clone();
                let runtime_events = self.turn_runtime.take_runtime_events()?;
                ChatTurnOutput {
                    run_id: execution.run_id.clone(),
                    turn_id: execution.turn_id.clone(),
                    attempt_ids: execution.attempt_ids.clone(),
                    runtime_events,
                    status: ChatTurnStatus::Cancelled,
                    generated_requests,
                }
            }
        };
        execution.output = Some(output);
        Ok(())
    }

    fn finish_failed_turn(
        &mut self,
        run_id: RunId,
        turn_id: TurnId,
        attempt_ids: Vec<AttemptId>,
        generated_requests: usize,
        message: String,
        append_terminal_event: bool,
    ) -> Result<ChatTurnOutput> {
        self.link_failed_attempts(&attempt_ids, &message)?;
        if let Some(history) = &mut self.chat_history
            && let Err(error) = history.fail(message.clone())
        {
            tracing::warn!(
                target: "agentlibre::app",
                session_id = %self.session_id,
                turn_error = %message,
                history_error = %error,
                "failed to record chat session failure"
            );
        }
        if append_terminal_event {
            self.turn_runtime
                .append_failed_terminal_event()
                .with_context(|| {
                    format!("failed to append terminal event after turn failure: {message}")
                })?;
        }
        let runtime_events = self.turn_runtime.take_runtime_events()?;
        self.release_inference_context()
            .context("failed to release inference context after turn failure")?;
        self.session_finished = true;
        Ok(ChatTurnOutput {
            run_id,
            turn_id,
            attempt_ids,
            runtime_events,
            status: ChatTurnStatus::Failed { message },
            generated_requests,
        })
    }

    fn link_failed_attempts(&mut self, attempt_ids: &[AttemptId], message: &str) -> Result<()> {
        for attempt_id in attempt_ids {
            if self.turn_runtime.has_linked_attempt(attempt_id) {
                continue;
            }
            let envelope = self
                .turn_runtime
                .append_attempt_linked_event(attempt_id)
                .with_context(|| {
                    format!("failed to link model attempt after turn failure: {message}")
                })?;
            if let Some(history) = &mut self.chat_history
                && let Err(error) = history.link_attempt(envelope)
            {
                tracing::warn!(
                    target: "agentlibre::app",
                    session_id = %self.session_id,
                    attempt_id = %attempt_id,
                    history_error = %error,
                    "failed to record model attempt link after turn failure"
                );
            }
        }
        Ok(())
    }
}

impl Drop for ChatService {
    fn drop(&mut self) {
        if self.context_released {
            return;
        }
        if let Err(error) = self.turn_runtime.release_context_for_teardown() {
            tracing::warn!(
                target: "agentlibre::app",
                session_id = %self.session_id,
                error = %error,
                "failed to release inference context while dropping chat session"
            );
        } else {
            self.context_released = true;
        }
    }
}

fn cancelled_effect_result(effect: TurnEffect) -> TurnEffectResult {
    match effect {
        TurnEffect::HookBatch { key, .. } => TurnEffectResult::HookBatch {
            key,
            outcome: EffectOutcome::Cancelled,
        },
        TurnEffect::ModelGeneration { key, .. } => TurnEffectResult::ModelGeneration {
            key,
            outcome: EffectOutcome::Cancelled,
        },
        TurnEffect::CapabilityDispatch { key, .. } => TurnEffectResult::CapabilityDispatch {
            key,
            outcome: EffectOutcome::Cancelled,
        },
        TurnEffect::TranscriptAppend { key, .. } => TurnEffectResult::TranscriptAppend {
            key,
            outcome: EffectOutcome::Cancelled,
        },
    }
}

fn model_effect_failure(error: anyhow::Error) -> EffectOutcome<agl_turn::ModelResponse> {
    if let Some(manager_error) = error.downcast_ref::<ModelManagerError>() {
        return match manager_error {
            ModelManagerError::Cancelled => EffectOutcome::Cancelled,
            ModelManagerError::DeadlineExceeded => EffectOutcome::Failed(EffectFailure::new(
                EffectFailureCode::Deadline,
                format!("model request failed: {manager_error}"),
                false,
            )),
            _ => EffectOutcome::Failed(EffectFailure::new(
                EffectFailureCode::Inference,
                format!("model request failed: {manager_error}"),
                manager_error.retryable(),
            )),
        };
    }
    EffectOutcome::Failed(EffectFailure::new(
        EffectFailureCode::Inference,
        format!("model request failed: {error:#}"),
        false,
    ))
}

pub(crate) fn build_turn_input(spec: TurnInputSpec<'_>) -> TurnInput {
    let mut input = TurnInput::user(
        spec.run_id.clone(),
        spec.turn_id.clone(),
        spec.user_input.clone(),
    )
    .with_context_messages(spec.context_messages.to_vec())
    .with_request_index_start(spec.request_index)
    .with_hook_payload(spec.hook_payload)
    .with_max_hook_repair_attempts(spec.max_hook_repair_attempts);
    if let Some(policy_hash) = spec.capability_policy_hash {
        input = input.with_capability_policy_hash(policy_hash);
    }
    for hook_batch in spec.hook_batches {
        input = input.with_hook_batch(hook_batch.clone());
    }
    for tool in spec.visible_tools {
        input = input.with_visible_tool(tool.clone());
    }
    if !spec.visible_tools.is_empty() {
        input = input.with_max_tool_calls(MAX_TOOL_CALLS_PER_TURN);
    }
    input
}

fn ensure_final_assistant_message(messages: &mut Vec<TurnMessage>, content: Content) {
    match messages.last_mut() {
        Some(TurnMessage::Assistant { content: existing }) => *existing = content,
        _ => messages.push(TurnMessage::Assistant { content }),
    }
}

struct CompletedTurnRecording<'a> {
    session_id: &'a SessionId,
    remaining_attempt_ids: std::slice::Iter<'a, AttemptId>,
    runtime: &'a AgentLibreRuntimeConfig,
}

fn record_completed_turn_messages(
    chat_history: &mut Option<ChatSessionStore>,
    turn_runtime: &mut ChatTurnRuntime,
    recording: &mut CompletedTurnRecording<'_>,
    messages: &[TurnMessage],
    stop_reason: Option<StopReason>,
) -> Result<()> {
    let mut pending_stop_reason = stop_reason;
    for message in messages {
        match message {
            TurnMessage::System { .. } | TurnMessage::User { .. } => {}
            TurnMessage::Assistant { content } => {
                link_next_attempt(chat_history, turn_runtime, recording)?;
                let message_id = MessageId::generate();
                let is_stop_marker = pending_stop_reason.take().is_some();
                log_message_metadata(
                    "assistant",
                    recording.session_id,
                    &message_id,
                    content,
                    recording.runtime,
                );
                let envelope =
                    turn_runtime.append_runtime_event(RuntimeEvent::AssistantMessage {
                        message_id,
                        content: content.clone(),
                    })?;
                if let Some(history) = chat_history.as_mut() {
                    if is_stop_marker {
                        history.append_assistant_stop_marker(envelope)?;
                    } else {
                        history.append_assistant_message(envelope)?;
                    }
                }
            }
            TurnMessage::AssistantToolCall { name, arguments } => {
                link_next_attempt(chat_history, turn_runtime, recording)?;
                let message_id = MessageId::generate();
                let arguments_content = Content::text(arguments.to_string())?;
                log_message_metadata(
                    "assistant_tool_call",
                    recording.session_id,
                    &message_id,
                    &arguments_content,
                    recording.runtime,
                );
                let envelope =
                    turn_runtime.append_runtime_event(RuntimeEvent::AssistantToolCall {
                        message_id,
                        name: name.clone(),
                        arguments: arguments.clone(),
                    })?;
                if let Some(history) = chat_history.as_mut() {
                    history.append_assistant_tool_call(envelope)?;
                }
            }
            TurnMessage::ToolObservation { name, result } => {
                let message_id = MessageId::generate();
                log_action_result_metadata(
                    "tool",
                    recording.session_id,
                    &message_id,
                    result,
                    recording.runtime,
                );
                let envelope = turn_runtime.append_runtime_event(RuntimeEvent::ToolMessage {
                    message_id,
                    name: name.clone(),
                    data: result.data.clone(),
                })?;
                if let Some(history) = chat_history.as_mut() {
                    history.append_tool_message(envelope)?;
                }
            }
        }
    }
    while recording.remaining_attempt_ids.len() > 0 {
        link_next_attempt(chat_history, turn_runtime, recording)?;
    }
    Ok(())
}

fn link_next_attempt(
    chat_history: &mut Option<ChatSessionStore>,
    turn_runtime: &mut ChatTurnRuntime,
    recording: &mut CompletedTurnRecording<'_>,
) -> Result<()> {
    let Some(attempt_id) = recording.remaining_attempt_ids.next() else {
        return Ok(());
    };
    let envelope = turn_runtime.append_attempt_linked_event(attempt_id)?;
    if let Some(history) = chat_history.as_mut() {
        history.link_attempt(envelope)?;
    }
    Ok(())
}

pub fn stopped_turn_context_message(
    reason: StopReason,
    detail: Option<&StopDetail>,
    available_tools: &[VisibleTool],
) -> String {
    let available = render_available_tool_names(available_tools);
    let permission_recovery = if available_tools
        .iter()
        .any(|tool| tool.id.as_str() == "permissions.request")
    {
        "request exact tool access with `permissions.request`, or answer with the CLI/daemon path"
    } else {
        "answer with the CLI/daemon path or ask for a write-capable/tool-enabled session"
    };
    match (reason, detail) {
        (StopReason::ToolJsonUnrepairable, _) => format!(
            "The previous turn stopped because the model produced malformed tool JSON. No tool was executed. Available tools in this session: {available}."
        ),
        (StopReason::ToolLimitReached, Some(StopDetail::ToolLimitReached { limit })) => format!(
            "The previous turn stopped because the tool-call limit was reached ({limit}). No tool was executed. Available tools in this session: {available}."
        ),
        (StopReason::ToolLimitReached, _) => format!(
            "The previous turn stopped because tool use is not available in this CLI session. No tool was executed. Available tools in this session: {available}."
        ),
        (StopReason::HiddenTool, Some(StopDetail::HiddenTool { name })) => format!(
            "The previous turn stopped because the model requested unavailable tool `{name}`. No tool was executed. Available tools in this session: {available}. Recovery: do not call `{name}` again unless it appears in `<agentlibre_tool_context>`; {permission_recovery} instead."
        ),
        (StopReason::HiddenTool, _) => format!(
            "The previous turn stopped because the requested tool is not available in this CLI session. No tool was executed. Available tools in this session: {available}. Recovery: do not repeat hidden tool calls; {permission_recovery} instead."
        ),
        (
            StopReason::InvalidToolArguments,
            Some(StopDetail::InvalidToolArguments { name, message }),
        ) => format!(
            "The previous turn stopped because tool `{name}` received invalid arguments: {message}. No tool was executed. Available tools in this session: {available}."
        ),
        (StopReason::InvalidToolArguments, _) => format!(
            "The previous turn stopped because the requested tool arguments were invalid. No tool was executed. Available tools in this session: {available}."
        ),
    }
}

fn render_available_tool_names(tools: &[VisibleTool]) -> String {
    if tools.is_empty() {
        return "none".to_string();
    }
    tools
        .iter()
        .map(|tool| format!("`{}`", tool.id))
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn chat_workspace_root(input: &str, current_root: &Path) -> PathBuf {
    let path = PathBuf::from(input);
    if path.is_absolute() {
        path
    } else {
        current_root.join(path)
    }
}

pub fn replay_turn_messages(replay: &ChatSessionReplay) -> Vec<TurnMessage> {
    let mut messages = Vec::new();
    for event in &replay.events {
        match event {
            ChatSessionEvent::Runtime { envelope } => match &envelope.payload {
                RuntimeEvent::UserMessage { content, .. } => messages.push(TurnMessage::User {
                    content: content.clone(),
                }),
                RuntimeEvent::AssistantMessage { content, .. } => {
                    messages.push(TurnMessage::Assistant {
                        content: content.clone(),
                    });
                }
                RuntimeEvent::AssistantToolCall {
                    name, arguments, ..
                } => messages.push(TurnMessage::AssistantToolCall {
                    name: name.clone(),
                    arguments: arguments.clone(),
                }),
                RuntimeEvent::ToolMessage { name, data, .. } => {
                    messages.push(TurnMessage::ToolObservation {
                        name: name.clone(),
                        result: agl_capabilities::ActionResult::new(data.clone()),
                    });
                }
                _ => {}
            },
            ChatSessionEvent::ContextCleared { .. } => messages.clear(),
            _ => {}
        }
    }
    messages
}

fn log_message_metadata(
    role: &str,
    session_id: &SessionId,
    message_id: &MessageId,
    content: &Content,
    runtime: &AgentLibreRuntimeConfig,
) {
    let text = content
        .parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            ContentPart::Artifact { .. } => None,
        })
        .collect::<String>();
    let fields = logged_message_fields(role, &text, runtime.logging.include_message_text);
    tracing::info!(
        target: "agentlibre::app",
        session_id = %session_id,
        message_id = %message_id,
        role = %fields.role,
        content_bytes = fields.content_bytes,
        content = ?fields.content,
        "chat message recorded"
    );
}

fn log_action_result_metadata(
    role: &str,
    session_id: &SessionId,
    message_id: &MessageId,
    result: &agl_capabilities::ActionResult,
    runtime: &AgentLibreRuntimeConfig,
) {
    let data_bytes = serde_json::to_vec(&result.data)
        .expect("serializing an action result JSON value cannot fail")
        .len();
    let data = runtime.logging.include_message_text.then_some(&result.data);
    tracing::info!(
        target: "agentlibre::app",
        session_id = %session_id,
        message_id = %message_id,
        role,
        data_bytes,
        data = ?data,
        "chat action result recorded"
    );
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use agl_capabilities::DispatchDenialCode;
    use agl_config::LocalInferenceConfig;
    use agl_events::{
        EVENT_SCHEMA, EventDraft, EventEnvelope, EventScope, SafeRuntimeEvent, TurnFinishStatus,
    };
    use agl_ids::{EventId, MessageId, RequestId, RunId, SessionId, TurnId};
    use agl_inference::{
        InferenceFinishReason, InferenceResponse, InferenceResponseMetadata, ModelManagerStatus,
    };

    use crate::{ChatInferenceJob, InferenceClient};

    use super::*;

    const TEST_SESSION_ID: &str = "ses_01890f17-4a00-7000-8000-000000000001";
    const TEST_RUN_ID: &str = "run_01890f17-4a00-7000-8000-000000000002";
    const TEST_TURN_ID: &str = "turn_01890f17-4a00-7000-8000-000000000003";
    const TEST_REQUEST_ID: &str = "req_01890f17-4a00-7000-8000-000000000008";
    static TEST_ROOT_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn session_id() -> SessionId {
        SessionId::parse(TEST_SESSION_ID).unwrap()
    }

    fn run_id() -> RunId {
        RunId::parse(TEST_RUN_ID).unwrap()
    }

    fn turn_id() -> TurnId {
        TurnId::parse(TEST_TURN_ID).unwrap()
    }

    fn request_id() -> RequestId {
        RequestId::parse(TEST_REQUEST_ID).unwrap()
    }

    fn text(value: impl Into<String>) -> Content {
        Content::text(value).unwrap()
    }

    fn visible_tool(id: &str) -> VisibleTool {
        let catalog = crate::tools::chat_extension_catalog().unwrap();
        let id = agl_capabilities::CapabilityId::new(id).unwrap();
        VisibleTool::from_declaration(catalog.action(&id).unwrap())
    }

    fn message_id(last_hex: char) -> MessageId {
        MessageId::parse(&format!(
            "msg_01890f17-4a00-7000-8000-00000000000{last_hex}"
        ))
        .unwrap()
    }

    fn runtime_event(sequence: u64, payload: RuntimeEvent) -> ChatSessionEvent {
        ChatSessionEvent::Runtime {
            envelope: Box::new(EventEnvelope {
                schema: EVENT_SCHEMA.to_string(),
                event_id: EventId::parse(&format!("evt_01890f17-4a00-7000-8000-{sequence:012x}"))
                    .unwrap(),
                sequence,
                occurred_at_unix_ms: sequence,
                scope: EventScope::builder(run_id())
                    .session_id(session_id())
                    .turn_id(turn_id())
                    .build()
                    .unwrap(),
                request_id: None,
                caused_by: None,
                payload,
            }),
        }
    }

    struct TestChatService {
        service: ChatService,
        root: PathBuf,
    }

    impl Drop for TestChatService {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn test_chat_service(label: &str) -> TestChatService {
        test_chat_service_with_history(label, false)
    }

    fn test_chat_service_with_history(label: &str, history_enabled: bool) -> TestChatService {
        test_chat_service_with_client(
            label,
            history_enabled,
            crate::inference_client::test_inference_client(),
        )
    }

    fn test_chat_service_with_client(
        label: &str,
        history_enabled: bool,
        inference_client: InferenceClientHandle,
    ) -> TestChatService {
        let counter = TEST_ROOT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "agl-chat-service-{label}-{}-{counter}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let config_path = root.join("inference.toml");
        let missing_model = root.join("missing-model.gguf");
        std::fs::write(
            &config_path,
            format!(
                r#"[backend]
kind = "llama_cpp"
model = "{}"

[runtime]
gpu_layers = 0
context_tokens = 128
threads = 1
batch_size = 16
ubatch_size = 16

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
"#,
                missing_model.display()
            ),
        )
        .unwrap();
        let runtime = AgentLibreRuntimeConfig {
            paths: agl_runtime::AgentLibrePaths::from_agl_home(root.join("home")),
            logging: agl_runtime::AgentLibreLoggingConfig::default(),
            history: agl_runtime::AgentLibreHistoryConfig::default(),
            workspace: agl_runtime::AgentLibreWorkspaceConfig::default(),
        };
        let options = ChatOptions {
            inference: crate::InferenceOptions {
                config: Some(config_path),
                artifact_root: Some(root.join("artifacts")),
                workspace_root: Some(root.clone()),
                max_output_tokens: 1,
                ..Default::default()
            },
            workspace_root: Some(root.clone()),
            session_id: None,
            no_history: !history_enabled,
            new_session: true,
        };
        let service = ChatService::open(options, &runtime, inference_client).unwrap();
        TestChatService { service, root }
    }

    #[derive(Default)]
    struct ContextLifecycleCalls {
        cleared: Vec<SessionId>,
        released: Vec<SessionId>,
    }

    struct ContextLifecycleClient {
        calls: Arc<Mutex<ContextLifecycleCalls>>,
    }

    impl InferenceClient for ContextLifecycleClient {
        fn generate(&self, _job: ChatInferenceJob) -> Result<InferenceResponse> {
            bail!("context lifecycle test does not generate")
        }

        fn clear_context(
            &self,
            _config: &LocalInferenceConfig,
            session_id: &SessionId,
        ) -> Result<()> {
            self.calls.lock().unwrap().cleared.push(session_id.clone());
            Ok(())
        }

        fn release_context(
            &self,
            _config: &LocalInferenceConfig,
            session_id: &SessionId,
        ) -> Result<()> {
            self.calls.lock().unwrap().released.push(session_id.clone());
            Ok(())
        }

        fn status(&self) -> Result<ModelManagerStatus> {
            Ok(ModelManagerStatus::default())
        }
    }

    struct ScriptedInferenceClient {
        responses: Mutex<VecDeque<String>>,
    }

    impl InferenceClient for ScriptedInferenceClient {
        fn generate(&self, job: ChatInferenceJob) -> Result<InferenceResponse> {
            let content = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .context("scripted inference response was not configured")?;
            Ok(InferenceResponse {
                attempt_id: job.request.attempt_id,
                content,
                finish_reason: InferenceFinishReason::Stop,
                metadata: InferenceResponseMetadata {
                    model_state: Some("scripted".to_string()),
                    selected_device: None,
                    duration_ms: 0,
                    input_tokens: 4,
                    output_tokens: 2,
                },
            })
        }

        fn clear_context(
            &self,
            _config: &LocalInferenceConfig,
            _session_id: &SessionId,
        ) -> Result<()> {
            Ok(())
        }

        fn release_context(
            &self,
            _config: &LocalInferenceConfig,
            _session_id: &SessionId,
        ) -> Result<()> {
            Ok(())
        }

        fn status(&self) -> Result<ModelManagerStatus> {
            Ok(ModelManagerStatus::default())
        }
    }

    fn scripted_inference_client(responses: &[&str]) -> InferenceClientHandle {
        InferenceClientHandle::new(ScriptedInferenceClient {
            responses: Mutex::new(responses.iter().map(|value| value.to_string()).collect()),
        })
    }

    #[test]
    fn stepping_driver_exposes_initial_events_before_answer_terminal() {
        let mut chat = test_chat_service_with_client(
            "stepping-answer",
            false,
            scripted_inference_client(&["done"]),
        );
        let mut execution = chat
            .service
            .start_user_turn_with_ids(run_id(), turn_id(), Some(request_id()), text("hello"))
            .unwrap();

        let initial = execution.take_events();
        assert!(
            initial
                .iter()
                .any(|event| matches!(event.payload, SafeRuntimeEvent::TurnStarted { .. }))
        );
        assert!(
            initial
                .iter()
                .all(|event| !matches!(event.payload, SafeRuntimeEvent::TurnFinished { .. }))
        );
        assert!(matches!(
            execution.pending_effect(),
            Some(TurnEffect::ModelGeneration { .. })
        ));

        while !execution.is_terminal() {
            chat.service.advance_user_turn(&mut execution).unwrap();
        }
        let output = execution.take_output().unwrap();
        assert_eq!(
            output.status,
            ChatTurnStatus::Answered {
                answer: "done".to_string()
            }
        );
    }

    #[test]
    fn direct_driver_executes_tools_and_stops_hidden_calls() {
        let mut tool_chat = test_chat_service_with_client(
            "stepping-tool",
            false,
            scripted_inference_client(&[
                r#"<tool_call>{"name":"fs.read","arguments":{"path":"inference.toml"}}</tool_call>"#,
                "tool complete",
            ]),
        );
        let tool_output = tool_chat.service.run_user_turn("read config").unwrap();
        assert!(matches!(
            tool_output.status,
            ChatTurnStatus::Answered { ref answer } if answer == "tool complete"
        ));
        assert!(tool_output.runtime_events.iter().any(|event| matches!(
            event.payload,
            SafeRuntimeEvent::ToolCallFinished { .. }
                | SafeRuntimeEvent::CapabilityCallAdmitted { .. }
        )));

        let mut stop_chat = test_chat_service_with_client(
            "stepping-stop",
            false,
            scripted_inference_client(&[
                r#"<tool_call>{"name":"hidden.write","arguments":{}}</tool_call>"#,
            ]),
        );
        let stop_output = stop_chat.service.run_user_turn("write").unwrap();
        assert!(matches!(
            stop_output.status,
            ChatTurnStatus::Stopped {
                reason: StopReason::HiddenTool
            }
        ));
    }

    #[test]
    fn stepping_driver_cancels_before_pending_model_effect() {
        let mut chat = test_chat_service_with_client(
            "stepping-cancel",
            false,
            scripted_inference_client(&["unused"]),
        );
        let mut execution = chat
            .service
            .start_user_turn_with_ids(run_id(), turn_id(), Some(request_id()), text("cancel"))
            .unwrap();

        execution.request_cancellation().unwrap();
        chat.service.advance_user_turn(&mut execution).unwrap();

        let output = execution.take_output().unwrap();
        assert_eq!(output.status, ChatTurnStatus::Cancelled);
        assert!(
            output
                .runtime_events
                .iter()
                .any(|event| matches!(event.payload, SafeRuntimeEvent::TurnCancelled { .. }))
        );
        assert!(matches!(
            output.runtime_events.last().unwrap().payload,
            SafeRuntimeEvent::TurnFinished {
                status: TurnFinishStatus::Cancelled
            }
        ));
    }

    #[test]
    fn clear_and_finish_target_the_session_managed_context_once() {
        let calls = Arc::new(Mutex::new(ContextLifecycleCalls::default()));
        let client = InferenceClientHandle::new(ContextLifecycleClient {
            calls: Arc::clone(&calls),
        });
        let mut chat = test_chat_service_with_client("context-lifecycle", false, client);
        let session_id = chat.service.session_id().clone();
        chat.service.messages.push(TurnMessage::User {
            content: text("discard me"),
        });

        assert_eq!(chat.service.clear_context().unwrap(), 1);
        assert!(chat.service.messages.is_empty());
        chat.service.request_exit().unwrap();
        chat.service.request_exit().unwrap();

        let calls = calls.lock().unwrap();
        assert_eq!(calls.cleared, vec![session_id.clone()]);
        assert_eq!(calls.released, vec![session_id]);
    }

    #[test]
    fn dropping_an_active_turn_releases_its_managed_context() {
        let calls = Arc::new(Mutex::new(ContextLifecycleCalls::default()));
        let client = InferenceClientHandle::new(ContextLifecycleClient {
            calls: Arc::clone(&calls),
        });
        let mut chat = test_chat_service_with_client("active-drop", false, client);
        let session_id = chat.service.session_id().clone();
        chat.service
            .turn_runtime
            .begin_turn(&session_id, &run_id(), &turn_id(), Some(request_id()))
            .unwrap();

        drop(chat);

        assert_eq!(calls.lock().unwrap().released, vec![session_id]);
    }

    #[test]
    fn executor_terminal_envelope_is_appended_after_transcript_events() {
        let mut chat = test_chat_service("terminal-last");
        let run_id = run_id();
        let turn_id = turn_id();
        let session_id = chat.service.session_id().clone();
        chat.service
            .turn_runtime
            .begin_turn(&session_id, &run_id, &turn_id, Some(request_id()))
            .unwrap();
        chat.service
            .turn_runtime
            .append_runtime_event(RuntimeEvent::UserMessage {
                message_id: message_id('4'),
                content: text("hello"),
            })
            .unwrap();
        chat.service
            .turn_runtime
            .append_runtime_event(RuntimeEvent::AssistantMessage {
                message_id: message_id('5'),
                content: text("answer"),
            })
            .unwrap();
        chat.service
            .turn_runtime
            .append_executor_events(vec![EventDraft::new(
                EventScope::builder(run_id.clone())
                    .turn_id(turn_id.clone())
                    .build()
                    .unwrap(),
                RuntimeEvent::TurnFinished {
                    status: TurnFinishStatus::Answered,
                },
            )])
            .unwrap();

        let events = chat.service.turn_runtime.take_runtime_events().unwrap();
        let terminal = events.last().unwrap();
        assert!(matches!(
            terminal.payload,
            SafeRuntimeEvent::TurnFinished {
                status: TurnFinishStatus::Answered
            }
        ));
        assert_eq!(
            terminal.caused_by.as_ref(),
            Some(&events[events.len() - 2].event_id)
        );
        assert!(matches!(
            events[events.len() - 2].payload,
            SafeRuntimeEvent::AssistantMessage { .. }
        ));
    }

    #[test]
    fn active_turn_rejects_runtime_refresh_and_keeps_its_policy_snapshot() {
        let mut chat = test_chat_service("frozen-policy");
        let run_id = run_id();
        let turn_id = turn_id();
        let session_id = chat.service.session_id().clone();
        chat.service
            .turn_runtime
            .begin_turn(&session_id, &run_id, &turn_id, Some(request_id()))
            .unwrap();
        let policy_hash = chat
            .service
            .turn_runtime
            .active_policy_hash()
            .unwrap()
            .clone();

        let reload_error = chat
            .service
            .turn_runtime
            .reload_runtime_context()
            .unwrap_err();
        let workspace_error = chat
            .service
            .turn_runtime
            .set_workspace_root(chat.root.join("other"))
            .unwrap_err();

        assert!(reload_error.to_string().contains("active turn"));
        assert!(workspace_error.to_string().contains("active turn"));
        assert_eq!(
            chat.service.turn_runtime.active_policy_hash(),
            Some(&policy_hash)
        );
    }

    #[test]
    fn short_circuited_invalid_call_records_safe_denial_without_raw_name() {
        let mut chat = test_chat_service("invalid-call-denial");
        let run_id = run_id();
        let turn_id = turn_id();
        let session_id = chat.service.session_id().clone();
        chat.service
            .turn_runtime
            .begin_turn(&session_id, &run_id, &turn_id, Some(request_id()))
            .unwrap();
        let policy_hash = chat
            .service
            .turn_runtime
            .active_policy_hash()
            .unwrap()
            .as_str()
            .to_string();

        let raw_name = "model-controlled-secret\ninvalid-capability";
        chat.service
            .turn_runtime
            .append_executor_events(vec![EventDraft::new(
                EventScope::builder(run_id)
                    .turn_id(turn_id)
                    .build()
                    .unwrap(),
                RuntimeEvent::CapabilityCallDenied {
                    policy_hash: policy_hash.clone(),
                    capability_id: None,
                    reason_code: DispatchDenialCode::InvalidArguments.as_str().to_string(),
                },
            )])
            .unwrap();

        let events = chat.service.turn_runtime.take_runtime_events().unwrap();
        let denial = events
            .iter()
            .find_map(|event| match &event.payload {
                SafeRuntimeEvent::CapabilityCallDenied {
                    policy_hash,
                    capability_id,
                    reason_code,
                } => Some((policy_hash, capability_id, reason_code)),
                _ => None,
            })
            .unwrap();
        assert_eq!(denial.0, &policy_hash);
        assert_eq!(denial.1, &None);
        assert_eq!(denial.2, DispatchDenialCode::InvalidArguments.as_str());
        assert!(!serde_json::to_string(&events).unwrap().contains(raw_name));
    }

    #[test]
    fn failed_output_retains_runtime_events_and_ends_with_failed_terminal() {
        let mut chat = test_chat_service("failed-output");
        let run_id = run_id();
        let turn_id = turn_id();
        let request_id = request_id();

        let output = chat
            .service
            .run_user_turn_with_ids(
                run_id.clone(),
                turn_id.clone(),
                Some(request_id.clone()),
                "hello",
            )
            .unwrap();

        assert_eq!(output.run_id, run_id);
        assert_eq!(output.turn_id, turn_id);
        assert_eq!(output.generated_requests, 1);
        assert_eq!(output.attempt_ids.len(), 1);
        assert!(matches!(
            output.status,
            ChatTurnStatus::Failed { ref message } if message.contains("model request failed")
        ));
        assert!(output.runtime_events.len() > 2);
        assert!(output.runtime_events.iter().any(|event| matches!(
            event.payload,
            SafeRuntimeEvent::ModelRequestFailed { .. }
                | SafeRuntimeEvent::InferenceAttemptFailed { .. }
        )));
        assert!(output.runtime_events.iter().any(|event| {
            matches!(event.payload, SafeRuntimeEvent::ModelAttemptLinked)
                && event.scope.attempt_id() == output.attempt_ids.first()
        }));
        assert!(output.runtime_events.iter().all(|event| {
            event.scope.session_id() == Some(chat.service.session_id())
                && event.request_id.as_ref() == Some(&request_id)
        }));
        assert!(
            output.runtime_events[..output.runtime_events.len() - 1]
                .iter()
                .all(|event| !matches!(event.payload, SafeRuntimeEvent::TurnFinished { .. }))
        );
        assert!(matches!(
            output.runtime_events.last().unwrap().payload,
            SafeRuntimeEvent::TurnFinished {
                status: TurnFinishStatus::Failed
            }
        ));
    }

    #[test]
    fn failed_attempt_link_keeps_the_same_full_and_safe_envelope() {
        let mut chat = test_chat_service_with_history("failed-attempt-transcript", true);
        let output = chat
            .service
            .run_user_turn_with_ids(run_id(), turn_id(), Some(request_id()), "hello")
            .unwrap();
        let attempt_id = output.attempt_ids.first().unwrap();
        let safe_link = output
            .runtime_events
            .iter()
            .find(|event| {
                matches!(event.payload, SafeRuntimeEvent::ModelAttemptLinked)
                    && event.scope.attempt_id() == Some(attempt_id)
            })
            .unwrap();
        let replay = chat
            .service
            .chat_history
            .as_ref()
            .unwrap()
            .read_replay()
            .unwrap();
        let full_link = replay
            .events
            .iter()
            .find_map(|event| match event {
                ChatSessionEvent::Runtime { envelope }
                    if matches!(envelope.payload, RuntimeEvent::ModelAttemptLinked)
                        && envelope.scope.attempt_id() == Some(attempt_id) =>
                {
                    Some(envelope.as_ref())
                }
                _ => None,
            })
            .unwrap();

        assert_eq!(full_link.event_id, safe_link.event_id);
        assert_eq!(full_link.sequence, safe_link.sequence);
        assert_eq!(full_link.occurred_at_unix_ms, safe_link.occurred_at_unix_ms);
        assert_eq!(full_link.scope, safe_link.scope);
        assert_eq!(full_link.request_id, safe_link.request_id);
        assert_eq!(full_link.caused_by, safe_link.caused_by);
        assert!(
            replay
                .events
                .iter()
                .any(|event| matches!(event, ChatSessionEvent::SessionFailed { .. }))
        );
    }

    #[test]
    fn replay_turn_messages_keeps_transcript_order() {
        let session_id = session_id();
        let replay = ChatSessionReplay {
            events: vec![
                ChatSessionEvent::SessionStarted {
                    session_id: session_id.clone(),
                },
                runtime_event(
                    1,
                    RuntimeEvent::UserMessage {
                        message_id: message_id('4'),
                        content: text("hello"),
                    },
                ),
                runtime_event(
                    2,
                    RuntimeEvent::AssistantMessage {
                        message_id: message_id('5'),
                        content: text("hi"),
                    },
                ),
                runtime_event(
                    3,
                    RuntimeEvent::AssistantToolCall {
                        message_id: message_id('6'),
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({"path": "README.MD"}),
                    },
                ),
                runtime_event(
                    4,
                    RuntimeEvent::ToolMessage {
                        message_id: message_id('7'),
                        name: "read_file".to_string(),
                        data: serde_json::json!({"content": "content"}),
                    },
                ),
            ],
        };

        assert_eq!(
            replay_turn_messages(&replay),
            vec![
                TurnMessage::User {
                    content: text("hello")
                },
                TurnMessage::Assistant {
                    content: text("hi")
                },
                TurnMessage::AssistantToolCall {
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({"path": "README.MD"})
                },
                TurnMessage::ToolObservation {
                    name: "read_file".to_string(),
                    result: agl_capabilities::ActionResult::new(
                        serde_json::json!({"content": "content"})
                    )
                }
            ]
        );
    }

    #[test]
    fn replay_turn_messages_honors_context_clear() {
        let session_id = session_id();
        let replay = ChatSessionReplay {
            events: vec![
                runtime_event(
                    1,
                    RuntimeEvent::UserMessage {
                        message_id: message_id('4'),
                        content: text("old"),
                    },
                ),
                ChatSessionEvent::ContextCleared {
                    session_id: session_id.clone(),
                },
                runtime_event(
                    2,
                    RuntimeEvent::UserMessage {
                        message_id: message_id('5'),
                        content: text("new"),
                    },
                ),
            ],
        };

        assert_eq!(
            replay_turn_messages(&replay),
            vec![TurnMessage::User {
                content: text("new")
            }]
        );
    }

    #[test]
    fn build_turn_input_preserves_context_and_request_index() {
        let run_id = run_id();
        let turn_id = turn_id();
        let context = vec![
            TurnMessage::User {
                content: text("old"),
            },
            TurnMessage::Assistant {
                content: text("previous"),
            },
        ];

        let hook_batches = vec![
            TurnHookBatch::new(agl_loop::HookEvent::ArtifactWrite)
                .with_required_hook(agl_loop::HookId::new("task_spec.validate").unwrap()),
        ];

        let visible_tools = vec![visible_tool("fs.read")];
        let user_input = text("new");

        let input = build_turn_input(TurnInputSpec {
            run_id: &run_id,
            turn_id: &turn_id,
            request_index: 7,
            context_messages: &context,
            hook_batches: &hook_batches,
            hook_payload: serde_json::json!({"runtime_identity": {"skills": []}}),
            max_hook_repair_attempts: 1,
            visible_tools: &visible_tools,
            capability_policy_hash: Some("sha256:test".to_string()),
            user_input: &user_input,
        });

        assert_eq!(input.run_id, run_id);
        assert_eq!(input.turn_id, turn_id);
        assert_eq!(input.user_input, text("new"));
        assert_eq!(input.context_messages, context);
        assert_eq!(input.hook_batches, hook_batches);
        assert_eq!(
            input.hook_payload,
            serde_json::json!({"runtime_identity": {"skills": []}})
        );
        assert_eq!(input.request_index_start, 7);
        assert_eq!(input.visible_tools, visible_tools);
        assert_eq!(input.max_tool_calls, MAX_TOOL_CALLS_PER_TURN);
        assert_eq!(input.max_hook_repair_attempts, 1);
        assert_eq!(input.capability_policy_hash.as_deref(), Some("sha256:test"));
    }

    #[test]
    fn build_turn_input_keeps_tools_disabled_without_visible_tools() {
        let run_id = run_id();
        let turn_id = turn_id();
        let user_input = text("new");
        let input = build_turn_input(TurnInputSpec {
            run_id: &run_id,
            turn_id: &turn_id,
            request_index: 1,
            context_messages: &[],
            hook_batches: &[],
            hook_payload: serde_json::json!({}),
            max_hook_repair_attempts: 0,
            visible_tools: &[],
            capability_policy_hash: None,
            user_input: &user_input,
        });

        assert!(input.visible_tools.is_empty());
        assert_eq!(input.max_tool_calls, 0);
    }

    #[test]
    fn chat_workspace_root_resolves_relative_to_current_root() {
        assert_eq!(
            chat_workspace_root("../next", std::path::Path::new("/tmp/root/current")),
            PathBuf::from("/tmp/root/current/../next")
        );
        assert_eq!(
            chat_workspace_root("/tmp/absolute", std::path::Path::new("/tmp/root")),
            PathBuf::from("/tmp/absolute")
        );
    }

    #[test]
    fn stop_reason_names_are_cli_stable() {
        assert_eq!(
            StopReason::ToolJsonUnrepairable.as_str(),
            "tool_json_unrepairable"
        );
        assert_eq!(StopReason::ToolLimitReached.as_str(), "tool_limit_reached");
        assert_eq!(StopReason::HiddenTool.as_str(), "hidden_tool");
        assert_eq!(
            StopReason::InvalidToolArguments.as_str(),
            "invalid_tool_arguments"
        );
    }

    #[test]
    fn stopped_turn_context_message_explains_no_tool_execution() {
        let visible_tools = vec![
            visible_tool("fs.list"),
            visible_tool("fs.read"),
            visible_tool("fs.search"),
        ];
        for reason in [
            StopReason::ToolJsonUnrepairable,
            StopReason::ToolLimitReached,
            StopReason::InvalidToolArguments,
        ] {
            let message = stopped_turn_context_message(reason, None, &visible_tools);

            assert!(message.contains("previous turn stopped"));
            assert!(message.contains("No tool was executed."));
            assert!(message.contains("`fs.list`, `fs.read`, `fs.search`"));
        }
    }

    #[test]
    fn hidden_tool_stop_message_names_rejected_tool_and_recovery() {
        let visible_tools = vec![
            visible_tool("fs.list"),
            visible_tool("fs.read"),
            visible_tool("fs.search"),
        ];
        let message = stopped_turn_context_message(
            StopReason::HiddenTool,
            Some(&StopDetail::HiddenTool {
                name: "matrix".to_string(),
            }),
            &visible_tools,
        );

        assert!(message.contains("unavailable tool `matrix`"));
        assert!(message.contains("No tool was executed."));
        assert!(message.contains("`fs.list`, `fs.read`, `fs.search`"));
        assert!(message.contains("do not call `matrix` again"));
        assert!(message.contains("CLI/daemon path"));
    }

    #[test]
    fn hidden_tool_stop_message_mentions_permission_request_when_visible() {
        let visible_tools = vec![
            visible_tool("fs.list"),
            visible_tool("permissions.request"),
            visible_tool("permissions.status"),
        ];
        let message = stopped_turn_context_message(
            StopReason::HiddenTool,
            Some(&StopDetail::HiddenTool {
                name: "matrix.outbox.enqueue".to_string(),
            }),
            &visible_tools,
        );

        assert!(message.contains("unavailable tool `matrix.outbox.enqueue`"));
        assert!(message.contains("`permissions.request`"));
        assert!(message.contains("request exact tool access"));
        assert!(message.contains("do not call `matrix.outbox.enqueue` again"));
    }
}
