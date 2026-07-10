use std::path::{Path, PathBuf};

use agl_events::{RuntimeEvent, SafeRuntimeEventEnvelope};
use agl_ids::{AttemptId, MessageId, RequestId, RunId, SessionId, TurnId};
use agl_loop::{TurnInput, TurnOutput, run_turn};
use agl_runtime::{AgentLibreRuntimeConfig, logged_message_fields};
use agl_session::{ChatSessionEvent, ChatSessionReplay, ChatSessionStore};
use agl_turn::{StopDetail, StopReason, TurnHookBatch, TurnMessage, VisibleTool};
use anyhow::{Context, Result, bail};

use crate::{
    ChatLoopHost, ChatOptions, InferenceSession, ToolAccessMode, assistant_text_for_terminal,
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
    user_input: &'a str,
}

pub struct ChatService {
    runtime: AgentLibreRuntimeConfig,
    session_id: SessionId,
    tool_mode: ToolAccessMode,
    history_enabled: bool,
    resumed_session: bool,
    chat_history: Option<ChatSessionStore>,
    loop_host: ChatLoopHost,
    messages: Vec<TurnMessage>,
    session_finished: bool,
}

impl ChatService {
    pub fn open(options: ChatOptions, runtime: &AgentLibreRuntimeConfig) -> Result<Self> {
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
        let session = InferenceSession::new(options.inference, runtime, artifact_root_override)?;
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
        let loop_host = ChatLoopHost::new(session, &workspace_root)?;
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
            loop_host,
            messages,
            session_finished: false,
        })
    }

    pub fn summary(&self) -> ChatSessionSummary {
        ChatSessionSummary {
            session_id: self.session_id.clone(),
            artifact_root: self.loop_host.session().artifact_root().to_path_buf(),
            workspace_root: self.loop_host.workspace_root().to_path_buf(),
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
        self.loop_host.session().artifact_root()
    }

    pub fn workspace_root(&self) -> &Path {
        self.loop_host.workspace_root()
    }

    pub fn set_workspace_root(&mut self, workspace_root: impl AsRef<Path>) -> Result<()> {
        self.loop_host.set_workspace_root(workspace_root)
    }

    pub fn reload_runtime_context(&mut self) -> Result<usize> {
        self.loop_host.reload_runtime_context()?;
        Ok(self.loop_host.session().turn_visible_tools().len())
    }

    pub fn clear_context(&mut self) -> Result<usize> {
        let cleared_messages = self.messages.len();
        self.messages.clear();
        self.loop_host.clear_context()?;
        if let Some(history) = &mut self.chat_history {
            history.append_context_cleared()?;
        }
        Ok(cleared_messages)
    }

    pub fn request_exit(&mut self) -> Result<()> {
        if let Some(history) = &mut self.chat_history {
            history.request_exit()?;
            self.session_finished = true;
        }
        Ok(())
    }

    pub fn finish_eof_if_needed(&mut self) -> Result<()> {
        if !self.session_finished
            && let Some(history) = &mut self.chat_history
        {
            history.finish_eof()?;
            self.session_finished = true;
        }
        Ok(())
    }

    pub fn run_user_turn(&mut self, input: &str) -> Result<ChatTurnOutput> {
        self.run_user_turn_with_ids(RunId::generate(), TurnId::generate(), None, input)
    }

    pub fn run_user_turn_with_ids(
        &mut self,
        run_id: RunId,
        turn_id: TurnId,
        request_id: Option<RequestId>,
        input: &str,
    ) -> Result<ChatTurnOutput> {
        self.loop_host
            .begin_turn(&self.session_id, &run_id, &turn_id, request_id)?;
        let user_message_id = MessageId::generate();
        log_message_metadata(
            "user",
            &self.session_id,
            &user_message_id,
            input,
            &self.runtime,
        );
        let envelope = self
            .loop_host
            .append_runtime_event(RuntimeEvent::UserMessage {
                message_id: user_message_id,
                content: input.to_string(),
            })?;
        if let Some(history) = &mut self.chat_history
            && let Err(error) = history.append_user_message(envelope)
        {
            return self.finish_failed_turn(
                run_id,
                turn_id,
                Vec::new(),
                0,
                format!("failed to record user message: {error:#}"),
            );
        }
        let turn_input = build_turn_input(TurnInputSpec {
            run_id: &run_id,
            turn_id: &turn_id,
            request_index: 0,
            context_messages: &self.messages,
            hook_batches: self.loop_host.session().turn_hook_batches(),
            hook_payload: self.loop_host.session().turn_hook_payload(),
            max_hook_repair_attempts: self.loop_host.session().max_hook_repair_attempts(),
            visible_tools: self.loop_host.session().turn_visible_tools(),
            user_input: input,
        });
        let output = match run_turn(&mut self.loop_host, turn_input) {
            Ok(output) => output,
            Err(error) => {
                let generated_requests = self.loop_host.generated_requests();
                let attempt_ids = self.loop_host.take_attempt_ids();
                return self.finish_failed_turn(
                    run_id,
                    turn_id,
                    attempt_ids,
                    generated_requests,
                    format!("{error:#}"),
                );
            }
        };
        let generated_requests = self.loop_host.generated_requests();
        let attempt_ids = self.loop_host.take_attempt_ids();
        let previous_message_count = self.messages.len();
        let mut turn_messages = self.loop_host.take_turn_messages();
        if turn_messages.is_empty() {
            turn_messages = self.messages.clone();
            turn_messages.push(TurnMessage::User {
                content: input.to_string(),
            });
        }
        let status_result: Result<ChatTurnStatus> = {
            let mut turn_recording = CompletedTurnRecording {
                session_id: &self.session_id,
                remaining_attempt_ids: attempt_ids.iter(),
                runtime: &self.runtime,
            };
            (|| match output {
                TurnOutput::Answered { answer } => {
                    let content = assistant_text_for_terminal(&answer);
                    ensure_final_assistant_message(&mut turn_messages, content.clone());
                    record_completed_turn_messages(
                        &mut self.chat_history,
                        &mut self.loop_host,
                        &mut turn_recording,
                        turn_messages
                            .get(previous_message_count..)
                            .context("turn transcript is shorter than prior chat context")?,
                        None,
                    )?;
                    Ok(ChatTurnStatus::Answered { answer: content })
                }
                TurnOutput::Stopped { reason, detail } => {
                    let content = stopped_turn_context_message(
                        reason,
                        detail.as_ref(),
                        self.loop_host.session().turn_visible_tools(),
                    );
                    turn_messages.push(TurnMessage::Assistant { content });
                    record_completed_turn_messages(
                        &mut self.chat_history,
                        &mut self.loop_host,
                        &mut turn_recording,
                        turn_messages
                            .get(previous_message_count..)
                            .context("turn transcript is shorter than prior chat context")?,
                        Some(reason),
                    )?;
                    Ok(ChatTurnStatus::Stopped { reason })
                }
            })()
        };
        let status = match status_result {
            Ok(status) => status,
            Err(error) => {
                return self.finish_failed_turn(
                    run_id,
                    turn_id,
                    attempt_ids,
                    generated_requests,
                    format!("failed to record completed turn: {error:#}"),
                );
            }
        };
        if let Err(error) = self.loop_host.append_pending_terminal_event() {
            return self.finish_failed_turn(
                run_id,
                turn_id,
                attempt_ids,
                generated_requests,
                format!("failed to append turn terminal event: {error:#}"),
            );
        }
        self.messages = turn_messages;
        let runtime_events = self.loop_host.take_runtime_events()?;
        Ok(ChatTurnOutput {
            run_id,
            turn_id,
            attempt_ids,
            runtime_events,
            status,
            generated_requests,
        })
    }

    fn finish_failed_turn(
        &mut self,
        run_id: RunId,
        turn_id: TurnId,
        attempt_ids: Vec<AttemptId>,
        generated_requests: usize,
        message: String,
    ) -> Result<ChatTurnOutput> {
        for attempt_id in &attempt_ids {
            if self.loop_host.has_linked_attempt(attempt_id) {
                continue;
            }
            let envelope = self
                .loop_host
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
        self.session_finished = true;
        self.loop_host
            .append_failed_terminal_event()
            .with_context(|| {
                format!("failed to append terminal event after turn failure: {message}")
            })?;
        let runtime_events = self.loop_host.take_runtime_events()?;
        Ok(ChatTurnOutput {
            run_id,
            turn_id,
            attempt_ids,
            runtime_events,
            status: ChatTurnStatus::Failed { message },
            generated_requests,
        })
    }
}

pub(crate) fn build_turn_input(spec: TurnInputSpec<'_>) -> TurnInput {
    let mut input = TurnInput::user(
        spec.run_id.clone(),
        spec.turn_id.clone(),
        spec.user_input.to_string(),
    )
    .with_context_messages(spec.context_messages.to_vec())
    .with_request_index_start(spec.request_index)
    .with_hook_payload(spec.hook_payload)
    .with_max_hook_repair_attempts(spec.max_hook_repair_attempts);
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

fn ensure_final_assistant_message(messages: &mut Vec<TurnMessage>, content: String) {
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
    loop_host: &mut ChatLoopHost,
    recording: &mut CompletedTurnRecording<'_>,
    messages: &[TurnMessage],
    stop_reason: Option<StopReason>,
) -> Result<()> {
    let mut pending_stop_reason = stop_reason;
    for message in messages {
        match message {
            TurnMessage::System { .. } | TurnMessage::User { .. } => {}
            TurnMessage::Assistant { content } => {
                link_next_attempt(chat_history, loop_host, recording)?;
                let message_id = MessageId::generate();
                let is_stop_marker = pending_stop_reason.take().is_some();
                log_message_metadata(
                    "assistant",
                    recording.session_id,
                    &message_id,
                    content,
                    recording.runtime,
                );
                let envelope = loop_host.append_runtime_event(RuntimeEvent::AssistantMessage {
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
                link_next_attempt(chat_history, loop_host, recording)?;
                let message_id = MessageId::generate();
                log_message_metadata(
                    "assistant_tool_call",
                    recording.session_id,
                    &message_id,
                    &arguments.to_string(),
                    recording.runtime,
                );
                let envelope = loop_host.append_runtime_event(RuntimeEvent::AssistantToolCall {
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
                let envelope = loop_host.append_runtime_event(RuntimeEvent::ToolMessage {
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
        link_next_attempt(chat_history, loop_host, recording)?;
    }
    Ok(())
}

fn link_next_attempt(
    chat_history: &mut Option<ChatSessionStore>,
    loop_host: &mut ChatLoopHost,
    recording: &mut CompletedTurnRecording<'_>,
) -> Result<()> {
    let Some(attempt_id) = recording.remaining_attempt_ids.next() else {
        return Ok(());
    };
    let envelope = loop_host.append_attempt_linked_event(attempt_id)?;
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
    content: &str,
    runtime: &AgentLibreRuntimeConfig,
) {
    let fields = logged_message_fields(role, content, runtime.logging.include_message_text);
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
    use std::sync::atomic::{AtomicU64, Ordering};

    use agl_capabilities::{CapabilityId, DispatchDenialCode};
    use agl_events::{EVENT_SCHEMA, EventEnvelope, EventScope, SafeRuntimeEvent, TurnFinishStatus};
    use agl_ids::{EventId, MessageId, RequestId, RunId, SessionId, TurnId};
    use agl_loop::AgentLoopHost;
    use agl_turn::{TurnPhase, TurnTerminalStatus, TurnTransition, TurnTransitionRecord};

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
        let service = ChatService::open(options, &runtime).unwrap();
        TestChatService { service, root }
    }

    #[test]
    fn pending_terminal_envelope_is_appended_after_transcript_events() {
        let mut chat = test_chat_service("terminal-last");
        let run_id = run_id();
        let turn_id = turn_id();
        let session_id = chat.service.session_id().clone();
        chat.service
            .loop_host
            .begin_turn(&session_id, &run_id, &turn_id, Some(request_id()))
            .unwrap();
        chat.service
            .loop_host
            .append_runtime_event(RuntimeEvent::UserMessage {
                message_id: message_id('4'),
                content: "hello".to_string(),
            })
            .unwrap();
        let terminal = RuntimeEvent::TurnFinished {
            status: TurnFinishStatus::Answered,
        };
        AgentLoopHost::emit_transition(
            &mut chat.service.loop_host,
            &TurnTransitionRecord {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                sequence: 1,
                from: TurnPhase::AnswerReady,
                to: TurnPhase::Finished,
                transition: TurnTransition::Finish {
                    status: TurnTerminalStatus::Answered,
                },
            },
            &terminal,
        )
        .unwrap();
        chat.service
            .loop_host
            .append_runtime_event(RuntimeEvent::AssistantMessage {
                message_id: message_id('5'),
                content: "answer".to_string(),
            })
            .unwrap();
        chat.service
            .loop_host
            .append_pending_terminal_event()
            .unwrap();

        let events = chat.service.loop_host.take_runtime_events().unwrap();
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
            .loop_host
            .begin_turn(&session_id, &run_id, &turn_id, Some(request_id()))
            .unwrap();
        let policy_hash = chat.service.loop_host.active_policy_hash().unwrap().clone();

        let reload_error = chat.service.loop_host.reload_runtime_context().unwrap_err();
        let workspace_error = chat
            .service
            .loop_host
            .set_workspace_root(chat.root.join("other"))
            .unwrap_err();

        assert!(reload_error.to_string().contains("active turn"));
        assert!(workspace_error.to_string().contains("active turn"));
        assert_eq!(
            chat.service.loop_host.active_policy_hash(),
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
            .loop_host
            .begin_turn(&session_id, &run_id, &turn_id, Some(request_id()))
            .unwrap();
        let policy_hash = chat
            .service
            .loop_host
            .active_policy_hash()
            .unwrap()
            .as_str()
            .to_string();

        let raw_name = "model-controlled-secret\ninvalid-capability";
        AgentLoopHost::record_capability_denial(
            &mut chat.service.loop_host,
            CapabilityId::new(raw_name.to_string()).ok(),
            DispatchDenialCode::InvalidArguments,
        )
        .unwrap();

        let events = chat.service.loop_host.take_runtime_events().unwrap();
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
                        content: "hello".to_string(),
                    },
                ),
                runtime_event(
                    2,
                    RuntimeEvent::AssistantMessage {
                        message_id: message_id('5'),
                        content: "hi".to_string(),
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
                    content: "hello".to_string()
                },
                TurnMessage::Assistant {
                    content: "hi".to_string()
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
                        content: "old".to_string(),
                    },
                ),
                ChatSessionEvent::ContextCleared {
                    session_id: session_id.clone(),
                },
                runtime_event(
                    2,
                    RuntimeEvent::UserMessage {
                        message_id: message_id('5'),
                        content: "new".to_string(),
                    },
                ),
            ],
        };

        assert_eq!(
            replay_turn_messages(&replay),
            vec![TurnMessage::User {
                content: "new".to_string()
            }]
        );
    }

    #[test]
    fn build_turn_input_preserves_context_and_request_index() {
        let run_id = run_id();
        let turn_id = turn_id();
        let context = vec![
            TurnMessage::User {
                content: "old".to_string(),
            },
            TurnMessage::Assistant {
                content: "previous".to_string(),
            },
        ];

        let hook_batches = vec![
            TurnHookBatch::new(agl_loop::HookEvent::ArtifactWrite)
                .with_required_hook(agl_loop::HookId::new("task_spec.validate").unwrap()),
        ];

        let visible_tools = vec![visible_tool("fs.read")];

        let input = build_turn_input(TurnInputSpec {
            run_id: &run_id,
            turn_id: &turn_id,
            request_index: 7,
            context_messages: &context,
            hook_batches: &hook_batches,
            hook_payload: serde_json::json!({"runtime_identity": {"skills": []}}),
            max_hook_repair_attempts: 1,
            visible_tools: &visible_tools,
            user_input: "new",
        });

        assert_eq!(input.run_id, run_id);
        assert_eq!(input.turn_id, turn_id);
        assert_eq!(input.user_input, "new");
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
    }

    #[test]
    fn build_turn_input_keeps_tools_disabled_without_visible_tools() {
        let run_id = run_id();
        let turn_id = turn_id();
        let input = build_turn_input(TurnInputSpec {
            run_id: &run_id,
            turn_id: &turn_id,
            request_index: 1,
            context_messages: &[],
            hook_batches: &[],
            hook_payload: serde_json::json!({}),
            max_hook_repair_attempts: 0,
            visible_tools: &[],
            user_input: "new",
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
