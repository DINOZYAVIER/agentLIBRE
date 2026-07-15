use std::collections::BTreeMap;
use std::path::PathBuf;

use agl_chat::{
    ChatOptions, ChatService, ChatTurnStatus, InferenceOptions, ToolAccessMode as ChatToolMode,
};
use agl_protocol::{
    AssistantMessageEvent, DaemonCapability, DaemonEvent, DaemonEventKind, DaemonRequest,
    DaemonRequestKind, HelloEvent, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
    ProtocolToolMode, REQUEST_SCHEMA, SessionFinishedEvent, SessionListEvent, SessionOpenedEvent,
    SessionStatus, SessionStatusEvent, SessionSummary, SessionTranscriptEvent, TurnFailedEvent,
    TurnFinishedEvent, TurnStartedEvent, TurnStoppedEvent, TurnTerminalStatus,
};
use agl_runtime::AgentLibreRuntimeConfig;
use agl_session::{AgentLibreSessionId, ChatSessionEvent};
use anyhow::Result;

use crate::error::{busy_error, invalid_request_error, not_found_error, runtime_error};
use crate::transcript::transcript_event;

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
                                DaemonCapability::SessionList,
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
        ProtocolToolMode::Execute => ChatToolMode::Execute,
        ProtocolToolMode::Approve => ChatToolMode::Approve,
        ProtocolToolMode::Admin => ChatToolMode::Admin,
    }
}
