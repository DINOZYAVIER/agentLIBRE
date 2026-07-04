use std::collections::BTreeMap;
use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use agl_chat::{
    ChatOptions, ChatService, ChatSessionSummary, ChatTurnOutput, ChatTurnStatus, InferenceOptions,
    ToolAccessMode as ChatToolMode,
};
use agl_protocol::{
    AssistantMessageEvent, DaemonCapability, DaemonEvent, DaemonEventKind, DaemonRequest,
    DaemonRequestKind, HelloEvent, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
    ProtocolToolMode, REQUEST_SCHEMA, SessionFinishedEvent, SessionListEvent, SessionOpenedEvent,
    SessionStatus, SessionStatusEvent, SessionSummary, SessionTranscriptEvent, SessionTurnRequest,
    TurnFailedEvent, TurnFinishedEvent, TurnStartedEvent, TurnStoppedEvent, TurnTerminalStatus,
};
use agl_runtime::AgentLibreRuntimeConfig;
use agl_session::{AgentLibreSessionId, ChatSessionEvent};
use agl_store::{AglStore, IdempotencyOutcome, IdempotencyStatus, StoreError};
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{busy_error, invalid_request_error, not_found_error, runtime_error};
use crate::transcript::transcript_event;

const SESSION_TURN_IDEMPOTENCY_NAMESPACE: &str = "daemon.session_turn";
const TURN_REPLAY_PAYLOAD_VERSION: u32 = 1;

pub struct DaemonState {
    runtime: AgentLibreRuntimeConfig,
    inference_defaults: InferenceOptions,
    sessions: BTreeMap<String, SessionRuntime>,
    store: AglStore,
}

impl DaemonState {
    pub fn new(runtime: AgentLibreRuntimeConfig, inference_defaults: InferenceOptions) -> Self {
        Self::open(runtime, inference_defaults).expect("failed to open daemon state store")
    }

    pub fn open(
        runtime: AgentLibreRuntimeConfig,
        inference_defaults: InferenceOptions,
    ) -> Result<Self> {
        let store = AglStore::open_at(runtime.paths.store_root())
            .context("failed to open daemon state store")?;
        Ok(Self {
            runtime,
            inference_defaults,
            sessions: BTreeMap::new(),
            store,
        })
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
                    Some(session) => {
                        match session.endpoint.clear_context().map_err(runtime_error) {
                            Ok(_) => {
                                session.status = SessionStatus::Open;
                                Ok(vec![status_event(
                                    &request_id,
                                    &request.session_id,
                                    session.status,
                                )])
                            }
                            Err(error) => Err(error),
                        }
                    }
                    None => Err(not_found_error(&request.session_id)),
                }
            }
            DaemonRequestKind::SessionFinish(request) => {
                match self.sessions.get_mut(&request.session_id) {
                    Some(session) if session.status == SessionStatus::Busy => Err(busy_error()),
                    Some(session) => match session.endpoint.request_exit().map_err(runtime_error) {
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
                    run_id: session.run_id.clone(),
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
        let (endpoint, summary) =
            SessionEndpoint::spawn_real(options, self.runtime.clone()).map_err(runtime_error)?;
        let session_id = summary.session_id.to_string();
        let run_id = summary.run_id;
        let resumed = summary.resumed;
        self.sessions.insert(
            session_id.clone(),
            SessionRuntime {
                endpoint,
                run_id: run_id.clone(),
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
        request: SessionTurnRequest,
    ) -> Result<Vec<DaemonEvent>, ProtocolError> {
        let turn = match self.prepare_turn(request_id, request)? {
            PreparedTurnAdmission::Run(turn) => turn,
            PreparedTurnAdmission::Replay(events) => return Ok(events),
        };
        let turn_result = turn.endpoint.run_user_turn(&turn.text);
        self.finish_turn(turn, turn_result)
    }

    fn prepare_turn(
        &mut self,
        request_id: String,
        request: SessionTurnRequest,
    ) -> Result<PreparedTurnAdmission, ProtocolError> {
        let session_id = request.session_id.clone();
        let Some(status) = self.sessions.get(&session_id).map(|session| session.status) else {
            return Err(not_found_error(&session_id));
        };
        if status == SessionStatus::Busy {
            return Err(busy_error());
        }
        if matches!(status, SessionStatus::Finished | SessionStatus::Failed) {
            return Err(ProtocolError::new(
                ProtocolErrorCode::InvalidRequest,
                format!("session {} is {:?}", session_id, status),
                false,
            ));
        }

        let turn_idempotency = match self.begin_turn_idempotency(&request_id, &request)? {
            TurnIdempotencyAdmission::Fresh(key) => key,
            TurnIdempotencyAdmission::Replay(events) => {
                return Ok(PreparedTurnAdmission::Replay(events));
            }
        };
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return Err(not_found_error(&session_id));
        };
        session.status = SessionStatus::Busy;
        let endpoint = session.endpoint.clone();
        let events = vec![DaemonEvent::new(
            Some(request_id.clone()),
            DaemonEventKind::TurnStarted(TurnStartedEvent {
                session_id: session_id.clone(),
                turn_id: session.run_id.clone(),
            }),
        )];
        Ok(PreparedTurnAdmission::Run(PreparedTurn {
            request_id,
            session_id,
            text: request.text,
            endpoint,
            events,
            turn_idempotency,
        }))
    }

    fn finish_turn(
        &mut self,
        mut turn: PreparedTurn,
        turn_result: Result<ChatTurnOutput>,
    ) -> Result<Vec<DaemonEvent>, ProtocolError> {
        match turn_result {
            Ok(output) => {
                let terminal = match output.status {
                    ChatTurnStatus::Answered { answer } => {
                        let terminal = TurnReplayTerminal::Answered {
                            answer: answer.clone(),
                        };
                        turn.events.push(DaemonEvent::new(
                            Some(turn.request_id.clone()),
                            DaemonEventKind::AssistantMessage(AssistantMessageEvent {
                                session_id: turn.session_id.clone(),
                                content: answer,
                            }),
                        ));
                        turn.events.push(DaemonEvent::new(
                            Some(turn.request_id.clone()),
                            DaemonEventKind::TurnFinished(TurnFinishedEvent {
                                session_id: turn.session_id.clone(),
                                status: TurnTerminalStatus::Answered,
                            }),
                        ));
                        terminal
                    }
                    ChatTurnStatus::Stopped { reason } => {
                        let terminal = TurnReplayTerminal::Stopped {
                            reason: reason.as_str().to_string(),
                        };
                        turn.events.push(DaemonEvent::new(
                            Some(turn.request_id.clone()),
                            DaemonEventKind::TurnStopped(TurnStoppedEvent {
                                session_id: turn.session_id.clone(),
                                reason: reason.as_str().to_string(),
                            }),
                        ));
                        turn.events.push(DaemonEvent::new(
                            Some(turn.request_id.clone()),
                            DaemonEventKind::TurnFinished(TurnFinishedEvent {
                                session_id: turn.session_id.clone(),
                                status: TurnTerminalStatus::Stopped,
                            }),
                        ));
                        terminal
                    }
                };
                let idempotency_result = self.finish_turn_idempotency(
                    turn.turn_idempotency.as_ref(),
                    &turn.session_id,
                    &terminal,
                );
                let events = turn.events.clone();
                self.restore_finished_turn(turn, SessionStatus::Open)?;
                idempotency_result?;
                Ok(events)
            }
            Err(err) => {
                let message = format!("{err:#}");
                let terminal = TurnReplayTerminal::Failed {
                    message: message.clone(),
                };
                let idempotency_result = self.finish_turn_idempotency(
                    turn.turn_idempotency.as_ref(),
                    &turn.session_id,
                    &terminal,
                );
                let events = vec![
                    DaemonEvent::new(
                        Some(turn.request_id.clone()),
                        DaemonEventKind::TurnFailed(TurnFailedEvent {
                            session_id: turn.session_id.clone(),
                            message,
                        }),
                    ),
                    DaemonEvent::new(
                        Some(turn.request_id.clone()),
                        DaemonEventKind::TurnFinished(TurnFinishedEvent {
                            session_id: turn.session_id.clone(),
                            status: TurnTerminalStatus::Failed,
                        }),
                    ),
                ];
                self.restore_finished_turn(turn, SessionStatus::Failed)?;
                idempotency_result?;
                Ok(events)
            }
        }
    }

    fn restore_finished_turn(
        &mut self,
        turn: PreparedTurn,
        status: SessionStatus,
    ) -> Result<(), ProtocolError> {
        let Some(session) = self.sessions.get_mut(&turn.session_id) else {
            return Err(not_found_error(&turn.session_id));
        };
        session.status = status;
        Ok(())
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

    fn begin_turn_idempotency(
        &self,
        request_id: &str,
        request: &SessionTurnRequest,
    ) -> Result<TurnIdempotencyAdmission, ProtocolError> {
        let Some(key) = request.idempotency_key.as_deref() else {
            return Ok(TurnIdempotencyAdmission::Fresh(None));
        };
        let fingerprint = turn_idempotency_fingerprint(&request.session_id, &request.text);
        match self
            .store
            .begin_idempotency(SESSION_TURN_IDEMPOTENCY_NAMESPACE, key, &fingerprint)
        {
            Ok(IdempotencyOutcome::Inserted(_)) => {
                Ok(TurnIdempotencyAdmission::Fresh(Some(TurnIdempotencyKey {
                    key: key.to_string(),
                })))
            }
            Ok(IdempotencyOutcome::Replayed(record)) => match record.status {
                IdempotencyStatus::InProgress => Err(busy_error()),
                IdempotencyStatus::Completed
                | IdempotencyStatus::Failed
                | IdempotencyStatus::Skipped => replay_turn_idempotency(
                    request_id,
                    &request.session_id,
                    record.result_ref.as_deref(),
                ),
            },
            Err(StoreError::IdempotencyConflict { .. }) => Err(invalid_request_error(
                "idempotency key was reused with a different turn",
            )),
            Err(err) => Err(runtime_error(err)),
        }
    }

    fn finish_turn_idempotency(
        &self,
        key: Option<&TurnIdempotencyKey>,
        session_id: &str,
        terminal: &TurnReplayTerminal,
    ) -> Result<(), ProtocolError> {
        let Some(key) = key else {
            return Ok(());
        };
        let payload = TurnReplayPayload {
            version: TURN_REPLAY_PAYLOAD_VERSION,
            session_id: session_id.to_string(),
            terminal: terminal.clone(),
        };
        let payload = serde_json::to_string(&payload).map_err(runtime_error)?;
        let result = match terminal {
            TurnReplayTerminal::Failed { .. } => self.store.fail_idempotency(
                SESSION_TURN_IDEMPOTENCY_NAMESPACE,
                &key.key,
                Some(&payload),
            ),
            TurnReplayTerminal::Answered { .. } | TurnReplayTerminal::Stopped { .. } => self
                .store
                .complete_idempotency(SESSION_TURN_IDEMPOTENCY_NAMESPACE, &key.key, Some(&payload)),
        };
        result.map(|_| ()).map_err(runtime_error)
    }

    #[cfg(test)]
    pub(crate) fn insert_test_session(&mut self, session_id: &str, outputs: Vec<ChatTurnStatus>) {
        let run_id = format!("run-{session_id}");
        self.sessions.insert(
            session_id.to_string(),
            SessionRuntime {
                endpoint: SessionEndpoint::spawn_test(outputs, None),
                run_id,
                status: SessionStatus::Open,
            },
        );
    }

    #[cfg(test)]
    pub(crate) fn insert_slow_test_session(
        &mut self,
        session_id: &str,
        outputs: Vec<ChatTurnStatus>,
        delay: std::time::Duration,
    ) {
        let run_id = format!("run-{session_id}");
        self.sessions.insert(
            session_id.to_string(),
            SessionRuntime {
                endpoint: SessionEndpoint::spawn_test(outputs, Some(delay)),
                run_id,
                status: SessionStatus::Open,
            },
        );
    }

    #[cfg(test)]
    pub(crate) fn test_session_turns(&self, session_id: &str) -> usize {
        self.sessions
            .get(session_id)
            .and_then(|session| session.endpoint.test_turns())
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub(crate) fn begin_test_turn_idempotency(&self, session_id: &str, text: &str, key: &str) {
        let fingerprint = turn_idempotency_fingerprint(session_id, text);
        self.store
            .begin_idempotency(SESSION_TURN_IDEMPOTENCY_NAMESPACE, key, &fingerprint)
            .unwrap();
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

#[derive(Clone)]
pub struct SharedDaemonState {
    inner: Arc<Mutex<DaemonState>>,
}

impl SharedDaemonState {
    pub fn new(runtime: AgentLibreRuntimeConfig, inference_defaults: InferenceOptions) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DaemonState::new(runtime, inference_defaults))),
        }
    }

    pub fn open(
        runtime: AgentLibreRuntimeConfig,
        inference_defaults: InferenceOptions,
    ) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Mutex::new(DaemonState::open(runtime, inference_defaults)?)),
        })
    }

    pub fn handle_request(&self, request: DaemonRequest) -> Vec<DaemonEvent> {
        let request_id = request.request_id.clone();
        if request.schema != REQUEST_SCHEMA {
            return vec![protocol_error_event(
                Some(request_id),
                ProtocolErrorCode::UnsupportedProtocolVersion,
                format!("unsupported request schema {}", request.schema),
                false,
            )];
        }

        match request.kind {
            DaemonRequestKind::SessionTurn(request) => self.handle_turn(request_id, request),
            kind => match self.inner.lock() {
                Ok(mut state) => state.handle_request(DaemonRequest {
                    schema: REQUEST_SCHEMA.to_string(),
                    request_id,
                    kind,
                }),
                Err(err) => vec![protocol_error_event(
                    Some(request_id),
                    ProtocolErrorCode::RuntimeFailure,
                    format!("daemon state lock is poisoned: {err}"),
                    false,
                )],
            },
        }
    }

    fn handle_turn(&self, request_id: String, request: SessionTurnRequest) -> Vec<DaemonEvent> {
        let turn = match self.inner.lock() {
            Ok(mut state) => match state.prepare_turn(request_id.clone(), request) {
                Ok(PreparedTurnAdmission::Run(turn)) => turn,
                Ok(PreparedTurnAdmission::Replay(events)) => return events,
                Err(error) => {
                    return vec![DaemonEvent::new(
                        Some(request_id),
                        DaemonEventKind::Error(error),
                    )];
                }
            },
            Err(err) => {
                return vec![protocol_error_event(
                    Some(request_id),
                    ProtocolErrorCode::RuntimeFailure,
                    format!("daemon state lock is poisoned: {err}"),
                    false,
                )];
            }
        };

        let turn_result = turn.endpoint.run_user_turn(&turn.text);
        match self.inner.lock() {
            Ok(mut state) => match state.finish_turn(turn, turn_result) {
                Ok(events) => events,
                Err(error) => vec![DaemonEvent::new(
                    Some(request_id),
                    DaemonEventKind::Error(error),
                )],
            },
            Err(err) => vec![protocol_error_event(
                Some(request_id),
                ProtocolErrorCode::RuntimeFailure,
                format!("daemon state lock is poisoned after turn execution: {err}"),
                false,
            )],
        }
    }

    #[cfg(test)]
    pub(crate) fn insert_slow_test_session(
        &self,
        session_id: &str,
        outputs: Vec<ChatTurnStatus>,
        delay: std::time::Duration,
    ) {
        self.inner
            .lock()
            .unwrap()
            .insert_slow_test_session(session_id, outputs, delay);
    }
}

struct SessionRuntime {
    endpoint: SessionEndpoint,
    run_id: String,
    status: SessionStatus,
}

#[derive(Clone)]
struct SessionEndpoint {
    commands: mpsc::Sender<SessionCommand>,
    #[cfg(test)]
    turns: Option<Arc<AtomicUsize>>,
}

enum SessionCommand {
    RunTurn {
        text: String,
        reply: mpsc::Sender<WorkerResult<ChatTurnOutput>>,
    },
    ClearContext {
        reply: mpsc::Sender<WorkerResult<usize>>,
    },
    RequestExit {
        reply: mpsc::Sender<WorkerResult<()>>,
    },
}

type WorkerResult<T> = std::result::Result<T, String>;

impl SessionEndpoint {
    fn spawn_real(
        options: ChatOptions,
        runtime: AgentLibreRuntimeConfig,
    ) -> Result<(Self, ChatSessionSummary)> {
        let (commands, receiver) = mpsc::channel();
        let (init_sender, init_receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("agl-daemon-session".to_string())
            .spawn(move || {
                let service = ChatService::open(options, &runtime);
                match service {
                    Ok(service) => {
                        let summary = service.summary();
                        if init_sender.send(Ok(summary)).is_ok() {
                            run_session_worker(service, receiver);
                        }
                    }
                    Err(err) => {
                        let _ = init_sender.send(Err(format!("{err:#}")));
                    }
                }
            })
            .context("failed to spawn daemon session worker")?;
        let summary = init_receiver
            .recv()
            .context("daemon session worker exited before initialization")?
            .map_err(|message| anyhow!(message))?;
        Ok((
            Self {
                commands,
                #[cfg(test)]
                turns: None,
            },
            summary,
        ))
    }

    fn run_user_turn(&self, input: &str) -> Result<ChatTurnOutput> {
        let (reply, receiver) = mpsc::channel();
        self.commands
            .send(SessionCommand::RunTurn {
                text: input.to_string(),
                reply,
            })
            .context("failed to send daemon session turn command")?;
        receive_worker_result(receiver, "turn")
    }

    fn clear_context(&mut self) -> Result<usize> {
        let (reply, receiver) = mpsc::channel();
        self.commands
            .send(SessionCommand::ClearContext { reply })
            .context("failed to send daemon session clear command")?;
        receive_worker_result(receiver, "clear")
    }

    fn request_exit(&mut self) -> Result<()> {
        let (reply, receiver) = mpsc::channel();
        self.commands
            .send(SessionCommand::RequestExit { reply })
            .context("failed to send daemon session finish command")?;
        receive_worker_result(receiver, "finish")
    }

    #[cfg(test)]
    fn spawn_test(outputs: Vec<ChatTurnStatus>, delay: Option<std::time::Duration>) -> Self {
        let (commands, receiver) = mpsc::channel();
        let turns = Arc::new(AtomicUsize::new(0));
        let worker_turns = Arc::clone(&turns);
        std::thread::spawn(move || run_test_session_worker(receiver, outputs, delay, worker_turns));
        Self {
            commands,
            turns: Some(turns),
        }
    }

    #[cfg(test)]
    fn test_turns(&self) -> Option<usize> {
        self.turns
            .as_ref()
            .map(|turns| turns.load(Ordering::SeqCst))
    }
}

fn run_session_worker(mut service: ChatService, receiver: mpsc::Receiver<SessionCommand>) {
    while let Ok(command) = receiver.recv() {
        match command {
            SessionCommand::RunTurn { text, reply } => {
                let _ = reply.send(
                    service
                        .run_user_turn(&text)
                        .map_err(|err| format!("{err:#}")),
                );
            }
            SessionCommand::ClearContext { reply } => {
                let _ = reply.send(service.clear_context().map_err(|err| format!("{err:#}")));
            }
            SessionCommand::RequestExit { reply } => {
                let result = service.request_exit().map_err(|err| format!("{err:#}"));
                let _ = reply.send(result);
                break;
            }
        }
    }
}

#[cfg(test)]
fn run_test_session_worker(
    receiver: mpsc::Receiver<SessionCommand>,
    mut outputs: Vec<ChatTurnStatus>,
    delay: Option<std::time::Duration>,
    turns: Arc<AtomicUsize>,
) {
    while let Ok(command) = receiver.recv() {
        match command {
            SessionCommand::RunTurn { reply, .. } => {
                turns.fetch_add(1, Ordering::SeqCst);
                if let Some(delay) = delay {
                    std::thread::sleep(delay);
                }
                let status = if outputs.is_empty() {
                    ChatTurnStatus::Answered {
                        answer: "test answer".to_string(),
                    }
                } else {
                    outputs.remove(0)
                };
                let _ = reply.send(Ok(ChatTurnOutput {
                    status,
                    generated_requests: 1,
                }));
            }
            SessionCommand::ClearContext { reply } => {
                let _ = reply.send(Ok(0));
            }
            SessionCommand::RequestExit { reply } => {
                let _ = reply.send(Ok(()));
                break;
            }
        }
    }
}

fn receive_worker_result<T>(
    receiver: mpsc::Receiver<WorkerResult<T>>,
    operation: &str,
) -> Result<T> {
    receiver
        .recv()
        .with_context(|| format!("daemon session worker exited during {operation}"))?
        .map_err(|message| anyhow!(message))
}

struct TurnIdempotencyKey {
    key: String,
}

enum PreparedTurnAdmission {
    Run(PreparedTurn),
    Replay(Vec<DaemonEvent>),
}

struct PreparedTurn {
    request_id: String,
    session_id: String,
    text: String,
    endpoint: SessionEndpoint,
    events: Vec<DaemonEvent>,
    turn_idempotency: Option<TurnIdempotencyKey>,
}

enum TurnIdempotencyAdmission {
    Fresh(Option<TurnIdempotencyKey>),
    Replay(Vec<DaemonEvent>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct TurnReplayPayload {
    version: u32,
    session_id: String,
    terminal: TurnReplayTerminal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum TurnReplayTerminal {
    Answered { answer: String },
    Stopped { reason: String },
    Failed { message: String },
}

fn replay_turn_idempotency(
    request_id: &str,
    session_id: &str,
    result_ref: Option<&str>,
) -> Result<TurnIdempotencyAdmission, ProtocolError> {
    let Some(result_ref) = result_ref else {
        return Err(runtime_error(
            "idempotent turn replay is missing result payload",
        ));
    };
    let payload: TurnReplayPayload = serde_json::from_str(result_ref).map_err(runtime_error)?;
    if payload.version != TURN_REPLAY_PAYLOAD_VERSION {
        return Err(runtime_error(format!(
            "unsupported idempotent turn replay payload version {}",
            payload.version
        )));
    }
    if payload.session_id != session_id {
        return Err(runtime_error("idempotent turn replay session mismatch"));
    }

    Ok(TurnIdempotencyAdmission::Replay(replay_events(
        request_id,
        session_id,
        payload.terminal,
    )))
}

fn replay_events(
    request_id: &str,
    session_id: &str,
    terminal: TurnReplayTerminal,
) -> Vec<DaemonEvent> {
    match terminal {
        TurnReplayTerminal::Answered { answer } => vec![
            DaemonEvent::new(
                Some(request_id.to_string()),
                DaemonEventKind::AssistantMessage(AssistantMessageEvent {
                    session_id: session_id.to_string(),
                    content: answer,
                }),
            ),
            DaemonEvent::new(
                Some(request_id.to_string()),
                DaemonEventKind::TurnFinished(TurnFinishedEvent {
                    session_id: session_id.to_string(),
                    status: TurnTerminalStatus::Answered,
                }),
            ),
        ],
        TurnReplayTerminal::Stopped { reason } => vec![
            DaemonEvent::new(
                Some(request_id.to_string()),
                DaemonEventKind::TurnStopped(TurnStoppedEvent {
                    session_id: session_id.to_string(),
                    reason,
                }),
            ),
            DaemonEvent::new(
                Some(request_id.to_string()),
                DaemonEventKind::TurnFinished(TurnFinishedEvent {
                    session_id: session_id.to_string(),
                    status: TurnTerminalStatus::Stopped,
                }),
            ),
        ],
        TurnReplayTerminal::Failed { message } => vec![
            DaemonEvent::new(
                Some(request_id.to_string()),
                DaemonEventKind::TurnFailed(TurnFailedEvent {
                    session_id: session_id.to_string(),
                    message,
                }),
            ),
            DaemonEvent::new(
                Some(request_id.to_string()),
                DaemonEventKind::TurnFinished(TurnFinishedEvent {
                    session_id: session_id.to_string(),
                    status: TurnTerminalStatus::Failed,
                }),
            ),
        ],
    }
}

fn turn_idempotency_fingerprint(session_id: &str, text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agentlibre.daemon.session_turn.v1\0");
    hasher.update(session_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(text.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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

fn protocol_error_event(
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

fn chat_tool_mode(mode: ProtocolToolMode) -> ChatToolMode {
    match mode {
        ProtocolToolMode::ReadOnly => ChatToolMode::ReadOnly,
        ProtocolToolMode::Write => ChatToolMode::Write,
        ProtocolToolMode::Execute => ChatToolMode::Execute,
        ProtocolToolMode::Approve => ChatToolMode::Approve,
        ProtocolToolMode::Admin => ChatToolMode::Admin,
    }
}
