use std::path::{Path, PathBuf};

use agl_loop::{TurnInput, TurnOutput, run_turn};
use agl_runtime::{AgentLibreRuntimeConfig, logged_message_fields};
use agl_session::{
    AgentLibreMessageId, AgentLibreSessionId, ChatSessionEvent, ChatSessionReplay, ChatSessionStore,
};
use agl_turn::{StopDetail, StopReason, TurnHookBatch, TurnMessage, VisibleTool};
use anyhow::{Context, Result, bail};

use crate::{
    ChatLoopHost, ChatOptions, InferenceSession, ToolAccessMode, assistant_text_for_terminal,
    default_run_id,
};

const MAX_TOOL_CALLS_PER_TURN: usize = 8;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatSessionSummary {
    pub session_id: AgentLibreSessionId,
    pub run_id: String,
    pub artifact_root: PathBuf,
    pub event_stream: PathBuf,
    pub workspace_root: PathBuf,
    pub tool_mode: &'static str,
    pub history_enabled: bool,
    pub resumed: bool,
    pub replayed_messages: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatTurnOutput {
    pub status: ChatTurnStatus,
    pub generated_requests: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChatTurnStatus {
    Answered { answer: String },
    Stopped { reason: StopReason },
}

pub struct ChatService {
    runtime: AgentLibreRuntimeConfig,
    session_id: AgentLibreSessionId,
    tool_mode: ToolAccessMode,
    history_enabled: bool,
    resumed_session: bool,
    chat_history: Option<ChatSessionStore>,
    loop_host: ChatLoopHost,
    messages: Vec<TurnMessage>,
    request_index: usize,
    message_index: usize,
    session_finished: bool,
}

impl ChatService {
    pub fn open(mut options: ChatOptions, runtime: &AgentLibreRuntimeConfig) -> Result<Self> {
        if options.new_session && options.session_id.is_some() {
            bail!("new session cannot be requested with a specific session id");
        }

        let history_enabled = runtime.history.enabled && !options.no_history;
        let run_id = options
            .inference
            .run_id
            .clone()
            .unwrap_or_else(default_run_id);
        options.inference.run_id = Some(run_id.clone());
        let session_id = if options.new_session {
            AgentLibreSessionId::generate()
        } else if let Some(session_id) = &options.session_id {
            AgentLibreSessionId::new(session_id.clone())?
        } else {
            AgentLibreSessionId::generate()
        };
        let resumed_session = history_enabled
            && !options.new_session
            && options.session_id.is_some()
            && ChatSessionStore::exists(runtime.paths.sessions_root(), &session_id);
        let explicit_artifact_root = InferenceSession::resolve_artifact_root(&options.inference);
        let workspace_root = runtime.resolve_workspace_root(options.workspace_root.as_deref())?;
        let tool_mode = options.inference.tool_mode;
        let artifact_root_override = if history_enabled {
            explicit_artifact_root.or_else(|| {
                Some(
                    runtime
                        .paths
                        .session_run_artifact_root(session_id.as_str(), &run_id),
                )
            })
        } else {
            None
        };
        let session = InferenceSession::new(options.inference, runtime, artifact_root_override)?;
        let (chat_history, replay) = if history_enabled {
            if resumed_session {
                let history = ChatSessionStore::open(
                    runtime.paths.sessions_root(),
                    session_id.clone(),
                    session.run_id().as_str().to_string(),
                )?;
                let replay = history.read_replay()?;
                (Some(history), Some(replay))
            } else {
                (
                    Some(ChatSessionStore::start(
                        runtime.paths.sessions_root(),
                        session_id.clone(),
                        session.run_id().as_str().to_string(),
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
        let request_index = replay
            .as_ref()
            .map(|replay| replay.next_attempt_index)
            .unwrap_or(1);
        let message_index = replay
            .as_ref()
            .map(|replay| replay.next_message_index)
            .unwrap_or(1);

        Ok(Self {
            runtime: runtime.clone(),
            session_id,
            tool_mode,
            history_enabled,
            resumed_session,
            chat_history,
            loop_host,
            messages,
            request_index,
            message_index,
            session_finished: false,
        })
    }

    pub fn summary(&self) -> ChatSessionSummary {
        ChatSessionSummary {
            session_id: self.session_id.clone(),
            run_id: self.loop_host.session().run_id().as_str().to_string(),
            artifact_root: self.loop_host.session().artifact_root().to_path_buf(),
            event_stream: self.loop_host.event_sink_path().to_path_buf(),
            workspace_root: self.loop_host.workspace_root().to_path_buf(),
            tool_mode: self.tool_mode.as_str(),
            history_enabled: self.history_enabled,
            resumed: self.resumed_session,
            replayed_messages: self.messages.len(),
        }
    }

    pub fn session_id(&self) -> &AgentLibreSessionId {
        &self.session_id
    }

    pub fn run_id(&self) -> &str {
        self.loop_host.session().run_id().as_str()
    }

    pub fn artifact_root(&self) -> &Path {
        self.loop_host.session().artifact_root()
    }

    pub fn event_sink_path(&self) -> &Path {
        self.loop_host.event_sink_path()
    }

    pub fn workspace_root(&self) -> &Path {
        self.loop_host.workspace_root()
    }

    pub fn set_workspace_root(&mut self, workspace_root: impl AsRef<Path>) -> Result<()> {
        self.loop_host.set_workspace_root(workspace_root)
    }

    pub fn clear_context(&mut self) -> Result<usize> {
        let cleared_messages = self.messages.len();
        self.messages.clear();
        self.loop_host.session_mut().clear_context();
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
        let user_message_id = AgentLibreMessageId::indexed(self.message_index);
        self.message_index += 1;
        log_message_metadata(
            "user",
            &self.session_id,
            &user_message_id,
            input,
            &self.runtime,
        );
        let attempt_id = format!("attempt-{:04}", self.request_index);
        if let Some(history) = &mut self.chat_history {
            history.append_user_message(user_message_id, input.to_string())?;
            history.link_attempt(attempt_id)?;
        }
        let turn_input = build_turn_input(
            self.loop_host.session().run_id().as_str(),
            self.request_index,
            &self.messages,
            self.loop_host.session().turn_hook_batches(),
            self.loop_host.session().turn_visible_tools(),
            input,
        );
        self.loop_host.reset_turn_counters();
        let output = match run_turn(&mut self.loop_host, turn_input) {
            Ok(output) => output,
            Err(err) => {
                if let Some(history) = &mut self.chat_history {
                    history.fail(format!("{err:#}"))?;
                }
                return Err(err);
            }
        };
        let generated_requests = self.loop_host.generated_requests();
        let mut next_attempt_to_link = self.request_index + 1;
        let linked_attempt_end = self.request_index + generated_requests;
        let mut turn_recording = CompletedTurnRecording {
            session_id: &self.session_id,
            message_index: &mut self.message_index,
            next_attempt_to_link: &mut next_attempt_to_link,
            linked_attempt_end,
            runtime: &self.runtime,
        };
        let previous_message_count = self.messages.len();
        let mut turn_messages = self.loop_host.take_turn_messages();
        if turn_messages.is_empty() {
            turn_messages = self.messages.clone();
            turn_messages.push(TurnMessage::User {
                content: input.to_string(),
            });
        }
        let status = match output {
            TurnOutput::Answered { answer } => {
                let content = assistant_text_for_terminal(&answer);
                ensure_final_assistant_message(&mut turn_messages, content.clone());
                record_completed_turn_messages(
                    &mut self.chat_history,
                    &mut turn_recording,
                    turn_messages
                        .get(previous_message_count..)
                        .context("turn transcript is shorter than prior chat context")?,
                    None,
                )?;
                ChatTurnStatus::Answered { answer: content }
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
                    &mut turn_recording,
                    turn_messages
                        .get(previous_message_count..)
                        .context("turn transcript is shorter than prior chat context")?,
                    Some(reason),
                )?;
                ChatTurnStatus::Stopped { reason }
            }
        };
        self.messages = turn_messages;
        self.request_index += generated_requests;
        Ok(ChatTurnOutput {
            status,
            generated_requests,
        })
    }
}

pub fn build_turn_input(
    run_id: &str,
    request_index: usize,
    context_messages: &[TurnMessage],
    hook_batches: &[TurnHookBatch],
    visible_tools: &[VisibleTool],
    user_input: &str,
) -> TurnInput {
    let mut input = TurnInput::user(user_input.to_string())
        .with_turn_id(run_id.to_string())
        .with_context_messages(context_messages.to_vec())
        .with_request_index_start(request_index);
    for hook_batch in hook_batches {
        input = input.with_hook_batch(hook_batch.clone());
    }
    for tool in visible_tools {
        input = input.with_visible_tool(tool.clone());
    }
    if !visible_tools.is_empty() {
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
    session_id: &'a AgentLibreSessionId,
    message_index: &'a mut usize,
    next_attempt_to_link: &'a mut usize,
    linked_attempt_end: usize,
    runtime: &'a AgentLibreRuntimeConfig,
}

fn record_completed_turn_messages(
    chat_history: &mut Option<ChatSessionStore>,
    recording: &mut CompletedTurnRecording<'_>,
    messages: &[TurnMessage],
    stop_reason: Option<StopReason>,
) -> Result<()> {
    let mut pending_stop_reason = stop_reason;
    for message in messages {
        match message {
            TurnMessage::System { .. } | TurnMessage::User { .. } => {}
            TurnMessage::Assistant { content } => {
                let message_id = AgentLibreMessageId::indexed(*recording.message_index);
                *recording.message_index += 1;
                if let Some(history) = chat_history.as_mut() {
                    if pending_stop_reason.take().is_some() {
                        history
                            .append_assistant_stop_marker(message_id.clone(), content.clone())?;
                    } else {
                        history.append_assistant_message(message_id.clone(), content.clone())?;
                    }
                }
                log_message_metadata(
                    "assistant",
                    recording.session_id,
                    &message_id,
                    content,
                    recording.runtime,
                );
            }
            TurnMessage::AssistantToolCall { name, arguments } => {
                let message_id = AgentLibreMessageId::indexed(*recording.message_index);
                *recording.message_index += 1;
                if let Some(history) = chat_history.as_mut() {
                    history.append_assistant_tool_call(
                        message_id.clone(),
                        name.clone(),
                        arguments.clone(),
                    )?;
                }
                log_message_metadata(
                    "assistant_tool_call",
                    recording.session_id,
                    &message_id,
                    &arguments.to_string(),
                    recording.runtime,
                );
            }
            TurnMessage::ToolObservation { name, content } => {
                let message_id = AgentLibreMessageId::indexed(*recording.message_index);
                *recording.message_index += 1;
                if let Some(history) = chat_history.as_mut() {
                    history.append_tool_message(
                        message_id.clone(),
                        name.clone(),
                        content.clone(),
                    )?;
                }
                log_message_metadata(
                    "tool",
                    recording.session_id,
                    &message_id,
                    content,
                    recording.runtime,
                );
                if let Some(history) = chat_history.as_mut()
                    && *recording.next_attempt_to_link < recording.linked_attempt_end
                {
                    history
                        .link_attempt(format!("attempt-{:04}", *recording.next_attempt_to_link))?;
                    *recording.next_attempt_to_link += 1;
                }
            }
        }
    }
    Ok(())
}

pub fn stopped_turn_context_message(
    reason: StopReason,
    detail: Option<&StopDetail>,
    available_tools: &[VisibleTool],
) -> String {
    let available = render_available_tool_names(available_tools);
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
            "The previous turn stopped because the model requested unavailable tool `{name}`. No tool was executed. Available tools in this session: {available}. Recovery: do not call `{name}` again unless it appears in `<agentlibre_tool_context>`; answer with the CLI/daemon path or ask for a write-capable/tool-enabled session instead."
        ),
        (StopReason::HiddenTool, _) => format!(
            "The previous turn stopped because the requested tool is not available in this CLI session. No tool was executed. Available tools in this session: {available}. Recovery: do not repeat hidden tool calls; answer with the CLI/daemon path or ask for a tool-enabled session instead."
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
        .map(|tool| format!("`{}`", tool.name))
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
            ChatSessionEvent::UserMessage { content, .. } => messages.push(TurnMessage::User {
                content: content.clone(),
            }),
            ChatSessionEvent::AssistantMessage { content, .. } => {
                messages.push(TurnMessage::Assistant {
                    content: content.clone(),
                });
            }
            ChatSessionEvent::AssistantToolCall {
                name, arguments, ..
            } => {
                messages.push(TurnMessage::AssistantToolCall {
                    name: name.clone(),
                    arguments: arguments.clone(),
                });
            }
            ChatSessionEvent::ToolMessage { name, content, .. } => {
                messages.push(TurnMessage::ToolObservation {
                    name: name.clone(),
                    content: content.clone(),
                });
            }
            ChatSessionEvent::ContextCleared { .. } => messages.clear(),
            _ => {}
        }
    }
    messages
}

fn log_message_metadata(
    role: &str,
    session_id: &AgentLibreSessionId,
    message_id: &AgentLibreMessageId,
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

#[cfg(test)]
mod tests {
    use agl_session::{AgentLibreMessageId, AgentLibreSessionId};

    use super::*;

    #[test]
    fn replay_turn_messages_keeps_transcript_order() {
        let session_id = AgentLibreSessionId::new("session-001").unwrap();
        let replay = ChatSessionReplay {
            events: vec![
                ChatSessionEvent::SessionStarted {
                    session_id: session_id.clone(),
                    run_id: "run-001".to_string(),
                },
                ChatSessionEvent::UserMessage {
                    session_id: session_id.clone(),
                    message_id: AgentLibreMessageId::indexed(1),
                    content: "hello".to_string(),
                },
                ChatSessionEvent::AssistantMessage {
                    session_id: session_id.clone(),
                    message_id: AgentLibreMessageId::indexed(2),
                    content: "hi".to_string(),
                },
                ChatSessionEvent::AssistantToolCall {
                    session_id: session_id.clone(),
                    message_id: AgentLibreMessageId::indexed(3),
                    name: "read_file".to_string(),
                    arguments: serde_json::json!({"path": "README.MD"}),
                },
                ChatSessionEvent::ToolMessage {
                    session_id,
                    message_id: AgentLibreMessageId::indexed(4),
                    name: "read_file".to_string(),
                    content: "content".to_string(),
                },
            ],
            next_message_index: 5,
            next_attempt_index: 1,
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
                    content: "content".to_string()
                }
            ]
        );
    }

    #[test]
    fn replay_turn_messages_honors_context_clear() {
        let session_id = AgentLibreSessionId::new("session-001").unwrap();
        let replay = ChatSessionReplay {
            events: vec![
                ChatSessionEvent::UserMessage {
                    session_id: session_id.clone(),
                    message_id: AgentLibreMessageId::indexed(1),
                    content: "old".to_string(),
                },
                ChatSessionEvent::ContextCleared {
                    session_id: session_id.clone(),
                },
                ChatSessionEvent::UserMessage {
                    session_id,
                    message_id: AgentLibreMessageId::indexed(2),
                    content: "new".to_string(),
                },
            ],
            next_message_index: 3,
            next_attempt_index: 1,
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

        let visible_tools = vec![
            VisibleTool::new("fs.read")
                .describe("Read a repository file")
                .require_argument("path"),
        ];

        let input = build_turn_input("run-001", 7, &context, &hook_batches, &visible_tools, "new");

        assert_eq!(input.turn_id, "run-001");
        assert_eq!(input.user_input, "new");
        assert_eq!(input.context_messages, context);
        assert_eq!(input.hook_batches, hook_batches);
        assert_eq!(input.request_index_start, 7);
        assert_eq!(input.visible_tools, visible_tools);
        assert_eq!(input.max_tool_calls, MAX_TOOL_CALLS_PER_TURN);
    }

    #[test]
    fn build_turn_input_keeps_tools_disabled_without_visible_tools() {
        let input = build_turn_input("run-001", 1, &[], &[], &[], "new");

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
            VisibleTool::new("fs.list"),
            VisibleTool::new("fs.read"),
            VisibleTool::new("fs.search"),
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
            VisibleTool::new("fs.list"),
            VisibleTool::new("fs.read"),
            VisibleTool::new("fs.search"),
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
}
