use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use agl_chat::{
    ChatOptions, ChatService, ChatTurnStatus, InferenceOptions, ToolAccessMode as ChatToolMode,
};
use agl_protocol::{
    AssistantMessageEvent, DaemonCapability, DaemonEvent, DaemonEventKind, DaemonRequest,
    DaemonRequestKind, HelloEvent, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
    ProtocolToolMode, REQUEST_SCHEMA, SessionFinishReason, SessionFinishedEvent, SessionListEvent,
    SessionOpenedEvent, SessionStatus, SessionStatusEvent, SessionSummary, SessionTranscriptEvent,
    TranscriptEvent, TurnFailedEvent, TurnFinishedEvent, TurnStartedEvent, TurnStoppedEvent,
    TurnTerminalStatus,
};
use agl_runtime::{AgentLibrePaths, AgentLibreRuntimeConfig};
use agl_session::{AgentLibreSessionId, ChatSessionEvent};
use anyhow::{Context, Result, bail};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

pub const DEFAULT_SOCKET_FILE: &str = "agl.sock";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonOptions {
    pub socket_path: PathBuf,
    pub inference: InferenceOptions,
}

impl DaemonOptions {
    pub fn new(paths: &AgentLibrePaths, inference: InferenceOptions) -> Self {
        Self {
            socket_path: default_socket_path(paths),
            inference,
        }
    }
}

pub fn default_socket_path(paths: &AgentLibrePaths) -> PathBuf {
    paths.state_dir.join("daemon").join(DEFAULT_SOCKET_FILE)
}

pub struct DaemonServer {
    runtime: AgentLibreRuntimeConfig,
    options: DaemonOptions,
}

impl DaemonServer {
    pub fn new(runtime: AgentLibreRuntimeConfig, options: DaemonOptions) -> Self {
        Self { runtime, options }
    }

    pub fn socket_path(&self) -> &Path {
        &self.options.socket_path
    }

    #[cfg(unix)]
    pub fn run_foreground(self) -> Result<()> {
        let listener = bind_listener(&self.options.socket_path)?;
        tracing::info!(
            target: "agentlibre::daemon",
            socket_path = %self.options.socket_path.display(),
            "daemon listening"
        );
        let mut state = DaemonState::new(self.runtime, self.options.inference);
        for incoming in listener.incoming() {
            let stream = incoming.context("failed to accept daemon client")?;
            if let Err(err) = handle_stream(stream, &mut state) {
                tracing::warn!(target: "agentlibre::daemon", error = %err, "daemon client failed");
            }
        }
        Ok(())
    }

    #[cfg(not(unix))]
    pub fn run_foreground(self) -> Result<()> {
        bail!("agl daemon is only available on Unix platforms in this alpha")
    }
}

#[cfg(unix)]
fn bind_listener(socket_path: &Path) -> Result<UnixListener> {
    let parent = socket_path
        .parent()
        .context("daemon socket path has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create daemon socket dir {}", parent.display()))?;

    if socket_path.exists() {
        match UnixStream::connect(socket_path) {
            Ok(_) => bail!(
                "daemon socket is already owned by a live process: {}",
                socket_path.display()
            ),
            Err(_) => std::fs::remove_file(socket_path).with_context(|| {
                format!(
                    "failed to remove stale daemon socket {}",
                    socket_path.display()
                )
            })?,
        }
    }

    let listener = UnixListener::bind(socket_path)
        .with_context(|| format!("failed to bind daemon socket {}", socket_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| {
                format!(
                    "failed to restrict daemon socket permissions {}",
                    socket_path.display()
                )
            })?;
    }
    Ok(listener)
}

#[cfg(unix)]
fn handle_stream(stream: UnixStream, state: &mut DaemonState) -> Result<()> {
    let mut writer = stream
        .try_clone()
        .context("failed to clone daemon client stream")?;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line.context("failed to read daemon request")?;
        if line.trim().is_empty() {
            continue;
        }
        let events = match serde_json::from_str::<DaemonRequest>(&line) {
            Ok(request) => state.handle_request(request),
            Err(err) => vec![DaemonEvent::new(
                None,
                DaemonEventKind::Error(ProtocolError::new(
                    ProtocolErrorCode::InvalidRequest,
                    format!("invalid daemon request JSON: {err}"),
                    false,
                )),
            )],
        };
        for event in events {
            write_event(&mut writer, &event)?;
        }
    }
    Ok(())
}

fn write_event(writer: &mut impl Write, event: &DaemonEvent) -> Result<()> {
    serde_json::to_writer(&mut *writer, event).context("failed to serialize daemon event")?;
    writer
        .write_all(b"\n")
        .context("failed to write daemon event newline")?;
    writer.flush().context("failed to flush daemon event")
}

pub struct DaemonState {
    runtime: AgentLibreRuntimeConfig,
    inference_defaults: InferenceOptions,
    sessions: BTreeMap<String, SessionRuntime>,
}

impl DaemonState {
    pub fn new(runtime: AgentLibreRuntimeConfig, inference_defaults: InferenceOptions) -> Self {
        Self {
            runtime,
            inference_defaults,
            sessions: BTreeMap::new(),
        }
    }

    pub fn handle_request(&mut self, request: DaemonRequest) -> Vec<DaemonEvent> {
        let request_id = request.request_id.clone();
        if request.schema != REQUEST_SCHEMA {
            return vec![self.error_event(
                Some(request_id),
                ProtocolErrorCode::UnsupportedProtocolVersion,
                format!("unsupported request schema {}", request.schema),
                false,
            )];
        }

        let result = match request.kind {
            DaemonRequestKind::Hello(request) => {
                let accepted = request.accepted_protocol_versions;
                if !accepted.is_empty()
                    && !accepted.iter().any(|version| version == PROTOCOL_VERSION)
                {
                    Err(ProtocolError::new(
                        ProtocolErrorCode::UnsupportedProtocolVersion,
                        "client does not accept daemon protocol version",
                        false,
                    ))
                } else {
                    Ok(vec![DaemonEvent::new(
                        Some(request_id.clone()),
                        DaemonEventKind::Hello(HelloEvent {
                            protocol_version: PROTOCOL_VERSION.to_string(),
                            product_version: env!("CARGO_PKG_VERSION").to_string(),
                            capabilities: vec![
                                DaemonCapability::SessionOpen,
                                DaemonCapability::SessionTurn,
                                DaemonCapability::SessionClear,
                                DaemonCapability::SessionFinish,
                                DaemonCapability::SessionStatus,
                                DaemonCapability::SessionTranscript,
                                DaemonCapability::FinalAssistantMessage,
                            ],
                        }),
                    )])
                }
            }
            DaemonRequestKind::SessionOpen(request) => {
                self.open_session(request_id.clone(), request)
            }
            DaemonRequestKind::SessionTurn(request) => self.run_turn(request_id.clone(), request),
            DaemonRequestKind::SessionClear(request) => {
                match self.sessions.get_mut(&request.session_id) {
                    Some(session) if session.status == SessionStatus::Busy => Err(busy_error()),
                    Some(session) => match session.service.clear_context().map_err(runtime_error) {
                        Ok(_) => {
                            session.status = SessionStatus::Open;
                            Ok(vec![status_event(
                                &request_id,
                                &request.session_id,
                                session.status,
                            )])
                        }
                        Err(error) => Err(error),
                    },
                    None => Err(not_found_error(&request.session_id)),
                }
            }
            DaemonRequestKind::SessionFinish(request) => {
                match self.sessions.get_mut(&request.session_id) {
                    Some(session) if session.status == SessionStatus::Busy => Err(busy_error()),
                    Some(session) => match session.service.request_exit().map_err(runtime_error) {
                        Ok(_) => {
                            session.status = SessionStatus::Finished;
                            Ok(vec![DaemonEvent::new(
                                Some(request_id.clone()),
                                DaemonEventKind::SessionFinished(SessionFinishedEvent {
                                    session_id: request.session_id,
                                    reason: request.reason,
                                }),
                            )])
                        }
                        Err(error) => Err(error),
                    },
                    None => Err(not_found_error(&request.session_id)),
                }
            }
            DaemonRequestKind::SessionCancel(_request) => Err(ProtocolError::new(
                ProtocolErrorCode::Unsupported,
                "turn cancellation is not implemented in this alpha",
                false,
            )),
            DaemonRequestKind::SessionStatus(request) => {
                let status = self
                    .sessions
                    .get(&request.session_id)
                    .map(|session| session.status)
                    .ok_or_else(|| not_found_error(&request.session_id));
                status.map(|status| vec![status_event(&request_id, &request.session_id, status)])
            }
            DaemonRequestKind::SessionList(_request) => Ok(vec![DaemonEvent::new(
                Some(request_id.clone()),
                DaemonEventKind::SessionList(SessionListEvent {
                    sessions: self
                        .sessions
                        .iter()
                        .map(|(session_id, session)| SessionSummary {
                            session_id: session_id.clone(),
                            title: None,
                            status: session.status,
                        })
                        .collect(),
                }),
            )]),
            DaemonRequestKind::SessionTranscript(request) => self.read_transcript(
                request_id.clone(),
                request.session_id,
                request.include_content,
            ),
        };

        match result {
            Ok(events) => events,
            Err(error) => vec![DaemonEvent::new(
                Some(request_id),
                DaemonEventKind::Error(error),
            )],
        }
    }

    fn open_session(
        &mut self,
        request_id: String,
        request: agl_protocol::SessionOpenRequest,
    ) -> Result<Vec<DaemonEvent>, ProtocolError> {
        if request.new_session && request.session_id.is_some() {
            return Err(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                "new_session cannot be used with session_id",
                false,
            ));
        }

        if let Some(session_id) = &request.session_id
            && let Some(session) = self.sessions.get(session_id)
        {
            return Ok(vec![DaemonEvent::new(
                Some(request_id),
                DaemonEventKind::SessionOpened(SessionOpenedEvent {
                    session_id: session_id.clone(),
                    run_id: session.service.run_id().to_string(),
                    resumed: true,
                }),
            )]);
        }

        let mut inference = self.inference_defaults.clone();
        inference.skills.extend(request.skills);
        inference.tool_mode = chat_tool_mode(request.tool_mode);
        let options = ChatOptions {
            inference,
            workspace_root: request.workspace_root.map(PathBuf::from),
            session_id: request.session_id,
            no_history: false,
            new_session: request.new_session,
        };
        let service = ChatService::open(options, &self.runtime).map_err(runtime_error)?;
        let summary = service.summary();
        let session_id = summary.session_id.to_string();
        let run_id = summary.run_id;
        let resumed = summary.resumed;
        self.sessions.insert(
            session_id.clone(),
            SessionRuntime {
                service,
                status: SessionStatus::Open,
            },
        );

        Ok(vec![DaemonEvent::new(
            Some(request_id),
            DaemonEventKind::SessionOpened(SessionOpenedEvent {
                session_id,
                run_id,
                resumed,
            }),
        )])
    }

    fn run_turn(
        &mut self,
        request_id: String,
        request: agl_protocol::SessionTurnRequest,
    ) -> Result<Vec<DaemonEvent>, ProtocolError> {
        let Some(session) = self.sessions.get_mut(&request.session_id) else {
            return Err(not_found_error(&request.session_id));
        };
        if session.status == SessionStatus::Busy {
            return Err(busy_error());
        }
        if matches!(
            session.status,
            SessionStatus::Finished | SessionStatus::Failed
        ) {
            return Err(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                format!("session {} is {:?}", request.session_id, session.status),
                false,
            ));
        }

        session.status = SessionStatus::Busy;
        let mut events = vec![DaemonEvent::new(
            Some(request_id.clone()),
            DaemonEventKind::TurnStarted(TurnStartedEvent {
                session_id: request.session_id.clone(),
                turn_id: session.service.run_id().to_string(),
            }),
        )];
        let turn_result = session.service.run_user_turn(&request.text);
        match turn_result {
            Ok(output) => {
                session.status = SessionStatus::Open;
                match output.status {
                    ChatTurnStatus::Answered { answer } => {
                        events.push(DaemonEvent::new(
                            Some(request_id.clone()),
                            DaemonEventKind::AssistantMessage(AssistantMessageEvent {
                                session_id: request.session_id.clone(),
                                content: answer,
                            }),
                        ));
                        events.push(DaemonEvent::new(
                            Some(request_id),
                            DaemonEventKind::TurnFinished(TurnFinishedEvent {
                                session_id: request.session_id,
                                status: TurnTerminalStatus::Answered,
                            }),
                        ));
                    }
                    ChatTurnStatus::Stopped { reason } => {
                        events.push(DaemonEvent::new(
                            Some(request_id.clone()),
                            DaemonEventKind::TurnStopped(TurnStoppedEvent {
                                session_id: request.session_id.clone(),
                                reason: reason.as_str().to_string(),
                            }),
                        ));
                        events.push(DaemonEvent::new(
                            Some(request_id),
                            DaemonEventKind::TurnFinished(TurnFinishedEvent {
                                session_id: request.session_id,
                                status: TurnTerminalStatus::Stopped,
                            }),
                        ));
                    }
                }
                Ok(events)
            }
            Err(err) => {
                session.status = SessionStatus::Failed;
                Ok(vec![
                    DaemonEvent::new(
                        Some(request_id.clone()),
                        DaemonEventKind::TurnFailed(TurnFailedEvent {
                            session_id: request.session_id.clone(),
                            message: format!("{err:#}"),
                        }),
                    ),
                    DaemonEvent::new(
                        Some(request_id),
                        DaemonEventKind::TurnFinished(TurnFinishedEvent {
                            session_id: request.session_id,
                            status: TurnTerminalStatus::Failed,
                        }),
                    ),
                ])
            }
        }
    }

    fn read_transcript(
        &self,
        request_id: String,
        session_id: String,
        include_content: bool,
    ) -> Result<Vec<DaemonEvent>, ProtocolError> {
        let session_id_value =
            AgentLibreSessionId::new(session_id.clone()).map_err(invalid_request_error)?;
        let transcript_path = self
            .runtime
            .paths
            .session_dir(session_id_value.as_str())
            .join("transcript.jsonl");
        let content = match std::fs::read_to_string(&transcript_path) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(not_found_error(&session_id));
            }
            Err(err) => return Err(runtime_error(err)),
        };
        let mut events = Vec::new();
        for (line_index, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let event: ChatSessionEvent = serde_json::from_str(line).map_err(|err| {
                ProtocolError::new(
                    ProtocolErrorCode::RuntimeFailure,
                    format!(
                        "failed to parse transcript {} line {}: {err}",
                        transcript_path.display(),
                        line_index + 1
                    ),
                    false,
                )
            })?;
            if let Some(event) = transcript_event(event, include_content) {
                events.push(event);
            }
        }
        Ok(vec![DaemonEvent::new(
            Some(request_id),
            DaemonEventKind::SessionTranscript(SessionTranscriptEvent {
                session_id,
                events,
                content_included: include_content,
            }),
        )])
    }

    fn error_event(
        &self,
        request_id: Option<String>,
        code: ProtocolErrorCode,
        message: impl Into<String>,
        retryable: bool,
    ) -> DaemonEvent {
        DaemonEvent::new(
            request_id,
            DaemonEventKind::Error(ProtocolError::new(code, message, retryable)),
        )
    }
}

struct SessionRuntime {
    service: ChatService,
    status: SessionStatus,
}

fn status_event(request_id: &str, session_id: &str, status: SessionStatus) -> DaemonEvent {
    DaemonEvent::new(
        Some(request_id.to_string()),
        DaemonEventKind::SessionStatus(SessionStatusEvent {
            session_id: session_id.to_string(),
            status,
        }),
    )
}

fn chat_tool_mode(mode: ProtocolToolMode) -> ChatToolMode {
    match mode {
        ProtocolToolMode::ReadOnly => ChatToolMode::ReadOnly,
        ProtocolToolMode::Write => ChatToolMode::Write,
    }
}

fn transcript_event(event: ChatSessionEvent, include_content: bool) -> Option<TranscriptEvent> {
    match event {
        ChatSessionEvent::SessionStarted { .. } => None,
        ChatSessionEvent::UserMessage {
            message_id,
            content,
            ..
        } => Some(TranscriptEvent::UserMessage {
            message_id: message_id.to_string(),
            content: include_content.then_some(content),
        }),
        ChatSessionEvent::AssistantMessage {
            message_id,
            content,
            ..
        } => Some(TranscriptEvent::AssistantMessage {
            message_id: message_id.to_string(),
            content: include_content.then_some(content),
        }),
        ChatSessionEvent::AssistantToolCall {
            message_id,
            name,
            arguments,
            ..
        } => Some(TranscriptEvent::AssistantToolCall {
            message_id: message_id.to_string(),
            name,
            arguments: include_content.then_some(arguments),
        }),
        ChatSessionEvent::ToolMessage {
            message_id,
            name,
            content,
            ..
        } => Some(TranscriptEvent::ToolMessage {
            message_id: message_id.to_string(),
            name,
            content: include_content.then_some(content),
        }),
        ChatSessionEvent::ModelAttemptLinked {
            run_id, attempt_id, ..
        } => Some(TranscriptEvent::ModelAttemptLinked { run_id, attempt_id }),
        ChatSessionEvent::ContextCleared { .. } => Some(TranscriptEvent::ContextCleared),
        ChatSessionEvent::SessionFinished { reason, .. } => {
            Some(TranscriptEvent::SessionFinished {
                reason: match reason {
                    agl_session::AgentLibreSessionFinishReason::Eof => SessionFinishReason::Eof,
                    agl_session::AgentLibreSessionFinishReason::ExitCommand => {
                        SessionFinishReason::ExitCommand
                    }
                    agl_session::AgentLibreSessionFinishReason::HostShutdown => {
                        SessionFinishReason::HostShutdown
                    }
                },
            })
        }
        ChatSessionEvent::SessionFailed { message, .. } => {
            Some(TranscriptEvent::SessionFailed { message })
        }
    }
}

fn busy_error() -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::Busy, "session is busy", true)
}

fn not_found_error(session_id: &str) -> ProtocolError {
    ProtocolError::new(
        ProtocolErrorCode::NotFound,
        format!("session {session_id} was not found"),
        false,
    )
}

fn runtime_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::RuntimeFailure, error.to_string(), false)
}

fn invalid_request_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, error.to_string(), false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agl_protocol::{
        EVENT_SCHEMA, HelloRequest, SessionListRequest, SessionStatusRequest,
        SessionTranscriptRequest,
    };
    use agl_runtime::{
        AgentLibreHistoryConfig, AgentLibreLoggingConfig, AgentLibreWorkspaceConfig,
    };

    fn runtime() -> AgentLibreRuntimeConfig {
        let root = std::env::temp_dir().join(format!(
            "agl-daemon-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("main")
        ));
        AgentLibreRuntimeConfig {
            paths: AgentLibrePaths::from_agl_home(root),
            logging: AgentLibreLoggingConfig::from_env(),
            history: AgentLibreHistoryConfig::default(),
            workspace: AgentLibreWorkspaceConfig::default(),
        }
    }

    fn request(kind: DaemonRequestKind) -> DaemonRequest {
        DaemonRequest::new("req-1", kind)
    }

    #[test]
    fn hello_reports_alpha_capabilities_without_loading_model() {
        let mut state = DaemonState::new(runtime(), InferenceOptions::default());

        let events = state.handle_request(request(DaemonRequestKind::Hello(HelloRequest {
            client_name: Some("test".to_string()),
            accepted_protocol_versions: vec![PROTOCOL_VERSION.to_string()],
        })));

        assert_eq!(events.len(), 1);
        match &events[0].kind {
            DaemonEventKind::Hello(event) => {
                assert_eq!(event.protocol_version, PROTOCOL_VERSION);
                assert!(event.capabilities.contains(&DaemonCapability::SessionOpen));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn status_for_unknown_session_returns_not_found() {
        let mut state = DaemonState::new(runtime(), InferenceOptions::default());

        let events = state.handle_request(request(DaemonRequestKind::SessionStatus(
            SessionStatusRequest {
                session_id: "missing".to_string(),
            },
        )));

        match &events[0].kind {
            DaemonEventKind::Error(error) => assert_eq!(error.code, ProtocolErrorCode::NotFound),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn session_list_starts_empty() {
        let mut state = DaemonState::new(runtime(), InferenceOptions::default());

        let events = state.handle_request(request(DaemonRequestKind::SessionList(
            SessionListRequest::default(),
        )));

        match &events[0].kind {
            DaemonEventKind::SessionList(event) => assert!(event.sessions.is_empty()),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn transcript_read_omits_content_by_default() {
        let runtime = runtime();
        let session_id = "session-transcript";
        let session_dir = runtime.paths.session_dir(session_id);
        std::fs::create_dir_all(&session_dir).unwrap();
        let transcript = ChatSessionEvent::UserMessage {
            session_id: AgentLibreSessionId::new(session_id).unwrap(),
            message_id: agl_session::AgentLibreMessageId::indexed(1),
            content: "secret".to_string(),
        };
        std::fs::write(
            session_dir.join("transcript.jsonl"),
            format!("{}\n", serde_json::to_string(&transcript).unwrap()),
        )
        .unwrap();
        let mut state = DaemonState::new(runtime.clone(), InferenceOptions::default());

        let events = state.handle_request(request(DaemonRequestKind::SessionTranscript(
            SessionTranscriptRequest {
                session_id: session_id.to_string(),
                include_content: false,
            },
        )));

        match &events[0].kind {
            DaemonEventKind::SessionTranscript(event) => {
                assert!(!event.content_included);
                assert_eq!(
                    event.events,
                    vec![TranscriptEvent::UserMessage {
                        message_id: "message-0001".to_string(),
                        content: None,
                    }]
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(runtime.paths.config_dir.parent().unwrap());
    }

    #[test]
    fn wrong_schema_returns_protocol_version_error() {
        let mut state = DaemonState::new(runtime(), InferenceOptions::default());
        let mut req = request(DaemonRequestKind::SessionList(SessionListRequest::default()));
        req.schema = "agentlibre.daemon.request.v2".to_string();

        let events = state.handle_request(req);

        match &events[0].kind {
            DaemonEventKind::Error(error) => {
                assert_eq!(error.code, ProtocolErrorCode::UnsupportedProtocolVersion);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn daemon_events_keep_current_schema() {
        let event = DaemonEvent::new(
            None,
            DaemonEventKind::SessionList(SessionListEvent {
                sessions: Vec::new(),
            }),
        );

        assert_eq!(event.schema, EVENT_SCHEMA);
    }
}
