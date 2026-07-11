use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use agl_chat::{
    ChatOptions, ChatService, ChatSessionSummary, ChatTurnOutput, ChatTurnStatus,
    InferenceClientHandle, InferenceOptions, ToolAccessMode as ChatToolMode,
};
use agl_events::{SafeRuntimeEvent, SafeRuntimeEventEnvelope, TurnFinishStatus};
#[cfg(test)]
use agl_ids::EventId;
use agl_ids::{RequestId, RunId, SessionId, TurnId};
use agl_inference::ModelManagerStatus;
use agl_protocol::{
    AssistantMessageEvent, DaemonCapability, DaemonEvent, DaemonEventKind, DaemonRequest,
    DaemonRequestKind, HelloEvent, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode,
    ProtocolToolMode, REQUEST_SCHEMA, SessionFinishedEvent, SessionListEvent, SessionOpenedEvent,
    SessionStatus, SessionStatusEvent, SessionSummary, SessionTranscriptEvent, SessionTurnRequest,
    TurnFailedEvent, TurnFinishedEvent, TurnStartedEvent, TurnStoppedEvent, TurnTerminalStatus,
};
use agl_runtime::AgentLibreRuntimeConfig;
use agl_session::ChatSessionStore;
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
    inference_client: InferenceClientHandle,
    sessions: BTreeMap<SessionId, SessionRuntime>,
    store: AglStore,
}

impl DaemonState {
    pub fn new(
        runtime: AgentLibreRuntimeConfig,
        inference_defaults: InferenceOptions,
        inference_client: InferenceClientHandle,
    ) -> Self {
        Self::open(runtime, inference_defaults, inference_client)
            .expect("failed to open daemon state store")
    }

    pub fn open(
        runtime: AgentLibreRuntimeConfig,
        inference_defaults: InferenceOptions,
        inference_client: InferenceClientHandle,
    ) -> Result<Self> {
        let store = AglStore::open_at(runtime.paths.store_root())
            .context("failed to open daemon state store")?;
        Ok(Self {
            runtime,
            inference_defaults,
            inference_client,
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
                                DaemonCapability::RuntimeEvents,
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
                    None => Err(not_found_error(request.session_id.as_str())),
                }
            }
            DaemonRequestKind::SessionFinish(request) => {
                match self.sessions.get_mut(&request.session_id) {
                    Some(session) if session.status == SessionStatus::Busy => Err(busy_error()),
                    Some(session) => {
                        let finish = if session.worker_finished {
                            Ok(())
                        } else {
                            session.endpoint.request_exit().map_err(runtime_error)
                        };
                        match finish {
                            Ok(_) => {
                                session.worker_finished = true;
                                if session.status != SessionStatus::Failed {
                                    session.status = SessionStatus::Finished;
                                }
                                Ok(vec![DaemonEvent::new(
                                    Some(request_id.clone()),
                                    DaemonEventKind::SessionFinished(SessionFinishedEvent {
                                        session_id: request.session_id,
                                        reason: request.reason,
                                    }),
                                )])
                            }
                            Err(error) => Err(error),
                        }
                    }
                    None => Err(not_found_error(request.session_id.as_str())),
                }
            }
            DaemonRequestKind::SessionStatus(request) => {
                let status = self
                    .sessions
                    .get(&request.session_id)
                    .map(|session| session.status)
                    .ok_or_else(|| not_found_error(request.session_id.as_str()));
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

    pub fn model_manager_status(&self) -> Result<ModelManagerStatus> {
        self.inference_client
            .status()
            .context("failed to inspect model manager status")
    }

    fn open_session(
        &mut self,
        request_id: RequestId,
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
            && self.sessions.contains_key(session_id)
        {
            return Ok(vec![DaemonEvent::new(
                Some(request_id),
                DaemonEventKind::SessionOpened(SessionOpenedEvent {
                    session_id: session_id.clone(),
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
        let (endpoint, summary) = SessionEndpoint::spawn_real(
            options,
            self.runtime.clone(),
            self.inference_client.clone(),
        )
        .map_err(runtime_error)?;
        let session_id = summary.session_id;
        let resumed = summary.resumed;
        self.sessions.insert(
            session_id.clone(),
            SessionRuntime {
                endpoint,
                status: SessionStatus::Open,
                worker_finished: false,
            },
        );

        Ok(vec![DaemonEvent::new(
            Some(request_id),
            DaemonEventKind::SessionOpened(SessionOpenedEvent {
                session_id,
                resumed,
            }),
        )])
    }

    fn run_turn(
        &mut self,
        request_id: RequestId,
        request: SessionTurnRequest,
    ) -> Result<Vec<DaemonEvent>, ProtocolError> {
        let turn = match self.prepare_turn(request_id, request)? {
            PreparedTurnAdmission::Run(turn) => turn,
            PreparedTurnAdmission::Replay(events) => return Ok(events),
        };
        let turn_result = turn.endpoint.run_user_turn(
            turn.run_id.clone(),
            turn.turn_id.clone(),
            Some(turn.request_id.clone()),
            &turn.text,
        );
        self.finish_turn(turn, turn_result)
    }

    fn prepare_turn(
        &mut self,
        request_id: RequestId,
        request: SessionTurnRequest,
    ) -> Result<PreparedTurnAdmission, ProtocolError> {
        let session_id = request.session_id.clone();
        let Some(status) = self.sessions.get(&session_id).map(|session| session.status) else {
            return Err(not_found_error(session_id.as_str()));
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
            return Err(not_found_error(session_id.as_str()));
        };
        let run_id = RunId::generate();
        let turn_id = TurnId::generate();
        session.status = SessionStatus::Busy;
        let endpoint = session.endpoint.clone();
        let events = vec![DaemonEvent::new(
            Some(request_id.clone()),
            DaemonEventKind::TurnStarted(TurnStartedEvent {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
            }),
        )];
        Ok(PreparedTurnAdmission::Run(PreparedTurn {
            request_id,
            session_id,
            run_id,
            turn_id,
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
        let turn_result = turn_result.and_then(|output| {
            if output.run_id != turn.run_id || output.turn_id != turn.turn_id {
                return Err(anyhow!(
                    "chat turn identity does not match daemon admission: expected {}/{}, got {}/{}",
                    turn.run_id,
                    turn.turn_id,
                    output.run_id,
                    output.turn_id
                ));
            }
            let terminal_status = match &output.status {
                ChatTurnStatus::Answered { .. } => TurnFinishStatus::Answered,
                ChatTurnStatus::Stopped { .. } => TurnFinishStatus::Stopped,
                ChatTurnStatus::Failed { .. } => TurnFinishStatus::Failed,
            };
            validate_runtime_envelopes(
                &output.runtime_events,
                &turn.request_id,
                &turn.session_id,
                &turn.run_id,
                &turn.turn_id,
                terminal_status,
            )?;
            Ok(output)
        });
        match turn_result {
            Ok(output) => {
                turn.events
                    .extend(output.runtime_events.into_iter().map(|event| {
                        DaemonEvent::new(
                            Some(turn.request_id.clone()),
                            DaemonEventKind::RuntimeEvent(Box::new(event)),
                        )
                    }));
                let terminal = match output.status {
                    ChatTurnStatus::Answered { answer } => {
                        let terminal = TurnReplayTerminal::Answered {
                            answer: answer.clone(),
                        };
                        turn.events.push(DaemonEvent::new(
                            Some(turn.request_id.clone()),
                            DaemonEventKind::AssistantMessage(AssistantMessageEvent {
                                session_id: turn.session_id.clone(),
                                run_id: turn.run_id.clone(),
                                turn_id: turn.turn_id.clone(),
                                content: answer,
                            }),
                        ));
                        turn.events.push(DaemonEvent::new(
                            Some(turn.request_id.clone()),
                            DaemonEventKind::TurnFinished(TurnFinishedEvent {
                                session_id: turn.session_id.clone(),
                                run_id: turn.run_id.clone(),
                                turn_id: turn.turn_id.clone(),
                                status: TurnTerminalStatus::Answered,
                            }),
                        ));
                        (terminal, SessionStatus::Open)
                    }
                    ChatTurnStatus::Stopped { reason } => {
                        let terminal = TurnReplayTerminal::Stopped {
                            reason: reason.as_str().to_string(),
                        };
                        turn.events.push(DaemonEvent::new(
                            Some(turn.request_id.clone()),
                            DaemonEventKind::TurnStopped(TurnStoppedEvent {
                                session_id: turn.session_id.clone(),
                                run_id: turn.run_id.clone(),
                                turn_id: turn.turn_id.clone(),
                                reason: reason.as_str().to_string(),
                            }),
                        ));
                        turn.events.push(DaemonEvent::new(
                            Some(turn.request_id.clone()),
                            DaemonEventKind::TurnFinished(TurnFinishedEvent {
                                session_id: turn.session_id.clone(),
                                run_id: turn.run_id.clone(),
                                turn_id: turn.turn_id.clone(),
                                status: TurnTerminalStatus::Stopped,
                            }),
                        ));
                        (terminal, SessionStatus::Open)
                    }
                    ChatTurnStatus::Failed { message } => {
                        let terminal = TurnReplayTerminal::Failed {
                            message: message.clone(),
                        };
                        turn.events.push(DaemonEvent::new(
                            Some(turn.request_id.clone()),
                            DaemonEventKind::TurnFailed(TurnFailedEvent {
                                session_id: turn.session_id.clone(),
                                run_id: turn.run_id.clone(),
                                turn_id: turn.turn_id.clone(),
                                message,
                            }),
                        ));
                        turn.events.push(DaemonEvent::new(
                            Some(turn.request_id.clone()),
                            DaemonEventKind::TurnFinished(TurnFinishedEvent {
                                session_id: turn.session_id.clone(),
                                run_id: turn.run_id.clone(),
                                turn_id: turn.turn_id.clone(),
                                status: TurnTerminalStatus::Failed,
                            }),
                        ));
                        (terminal, SessionStatus::Failed)
                    }
                };
                let idempotency_result = self.finish_turn_idempotency(
                    turn.turn_idempotency.as_ref(),
                    &turn.session_id,
                    &turn.run_id,
                    &turn.turn_id,
                    &terminal.0,
                );
                let events = turn.events.clone();
                self.restore_finished_turn(turn, terminal.1)?;
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
                    &turn.run_id,
                    &turn.turn_id,
                    &terminal,
                );
                turn.events.push(DaemonEvent::new(
                    Some(turn.request_id.clone()),
                    DaemonEventKind::TurnFailed(TurnFailedEvent {
                        session_id: turn.session_id.clone(),
                        run_id: turn.run_id.clone(),
                        turn_id: turn.turn_id.clone(),
                        message,
                    }),
                ));
                turn.events.push(DaemonEvent::new(
                    Some(turn.request_id.clone()),
                    DaemonEventKind::TurnFinished(TurnFinishedEvent {
                        session_id: turn.session_id.clone(),
                        run_id: turn.run_id.clone(),
                        turn_id: turn.turn_id.clone(),
                        status: TurnTerminalStatus::Failed,
                    }),
                ));
                let events = turn.events.clone();
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
            return Err(not_found_error(turn.session_id.as_str()));
        };
        session.status = status;
        Ok(())
    }

    fn read_transcript(
        &self,
        request_id: RequestId,
        session_id: SessionId,
        include_content: bool,
    ) -> Result<Vec<DaemonEvent>, ProtocolError> {
        let sessions_root = self.runtime.paths.sessions_root();
        if !ChatSessionStore::exists(&sessions_root, &session_id) {
            return Err(not_found_error(session_id.as_str()));
        }
        let replay = ChatSessionStore::open(&sessions_root, session_id.clone())
            .and_then(|store| store.read_replay())
            .map_err(runtime_error)?;
        let events = replay
            .events
            .into_iter()
            .filter_map(|event| transcript_event(event, include_content))
            .collect();
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
        request_id: &RequestId,
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
        session_id: &SessionId,
        run_id: &RunId,
        turn_id: &TurnId,
        terminal: &TurnReplayTerminal,
    ) -> Result<(), ProtocolError> {
        let Some(key) = key else {
            return Ok(());
        };
        let payload = TurnReplayPayload {
            version: TURN_REPLAY_PAYLOAD_VERSION,
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
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
    pub(crate) fn insert_test_session(
        &mut self,
        session_id: SessionId,
        outputs: Vec<ChatTurnStatus>,
    ) {
        self.sessions.insert(
            session_id.clone(),
            SessionRuntime {
                endpoint: SessionEndpoint::spawn_test(session_id, outputs, None, false),
                status: SessionStatus::Open,
                worker_finished: false,
            },
        );
    }

    #[cfg(test)]
    pub(crate) fn insert_test_session_with_runtime_events(
        &mut self,
        session_id: SessionId,
        outputs: Vec<ChatTurnStatus>,
    ) {
        self.sessions.insert(
            session_id.clone(),
            SessionRuntime {
                endpoint: SessionEndpoint::spawn_test(session_id, outputs, None, true),
                status: SessionStatus::Open,
                worker_finished: false,
            },
        );
    }

    #[cfg(test)]
    pub(crate) fn insert_slow_test_session(
        &mut self,
        session_id: SessionId,
        outputs: Vec<ChatTurnStatus>,
        delay: std::time::Duration,
    ) {
        self.sessions.insert(
            session_id.clone(),
            SessionRuntime {
                endpoint: SessionEndpoint::spawn_test(session_id, outputs, Some(delay), false),
                status: SessionStatus::Open,
                worker_finished: false,
            },
        );
    }

    #[cfg(test)]
    pub(crate) fn test_session_turns(&self, session_id: &SessionId) -> usize {
        self.sessions
            .get(session_id)
            .and_then(|session| session.endpoint.test_turns())
            .unwrap_or(0)
    }

    #[cfg(test)]
    pub(crate) fn begin_test_turn_idempotency(
        &self,
        session_id: &SessionId,
        text: &str,
        key: &str,
    ) {
        let fingerprint = turn_idempotency_fingerprint(session_id, text);
        self.store
            .begin_idempotency(SESSION_TURN_IDEMPOTENCY_NAMESPACE, key, &fingerprint)
            .unwrap();
    }

    fn error_event(
        &self,
        request_id: Option<RequestId>,
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
    pub fn new(
        runtime: AgentLibreRuntimeConfig,
        inference_defaults: InferenceOptions,
        inference_client: InferenceClientHandle,
    ) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DaemonState::new(
                runtime,
                inference_defaults,
                inference_client,
            ))),
        }
    }

    pub fn open(
        runtime: AgentLibreRuntimeConfig,
        inference_defaults: InferenceOptions,
        inference_client: InferenceClientHandle,
    ) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Mutex::new(DaemonState::open(
                runtime,
                inference_defaults,
                inference_client,
            )?)),
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

    pub fn model_manager_status(&self) -> Result<ModelManagerStatus> {
        self.inner
            .lock()
            .map_err(|error| anyhow!("daemon state lock is poisoned: {error}"))?
            .model_manager_status()
    }

    fn handle_turn(&self, request_id: RequestId, request: SessionTurnRequest) -> Vec<DaemonEvent> {
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

        let turn_result = turn.endpoint.run_user_turn(
            turn.run_id.clone(),
            turn.turn_id.clone(),
            Some(turn.request_id.clone()),
            &turn.text,
        );
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
        session_id: SessionId,
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
    status: SessionStatus,
    worker_finished: bool,
}

#[derive(Clone)]
struct SessionEndpoint {
    commands: mpsc::Sender<SessionCommand>,
    #[cfg(test)]
    turns: Option<Arc<AtomicUsize>>,
}

enum SessionCommand {
    RunTurn {
        run_id: RunId,
        turn_id: TurnId,
        request_id: Option<RequestId>,
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
        inference_client: InferenceClientHandle,
    ) -> Result<(Self, ChatSessionSummary)> {
        let (commands, receiver) = mpsc::channel();
        let (init_sender, init_receiver) = mpsc::channel();
        std::thread::Builder::new()
            .name("agl-daemon-session".to_string())
            .spawn(move || {
                let service = ChatService::open(options, &runtime, inference_client);
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

    fn run_user_turn(
        &self,
        run_id: RunId,
        turn_id: TurnId,
        request_id: Option<RequestId>,
        input: &str,
    ) -> Result<ChatTurnOutput> {
        let (reply, receiver) = mpsc::channel();
        self.commands
            .send(SessionCommand::RunTurn {
                run_id,
                turn_id,
                request_id,
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
    fn spawn_test(
        session_id: SessionId,
        outputs: Vec<ChatTurnStatus>,
        delay: Option<std::time::Duration>,
        emit_runtime_events: bool,
    ) -> Self {
        let (commands, receiver) = mpsc::channel();
        let turns = Arc::new(AtomicUsize::new(0));
        let worker_turns = Arc::clone(&turns);
        std::thread::spawn(move || {
            run_test_session_worker(
                receiver,
                session_id,
                outputs,
                delay,
                emit_runtime_events,
                worker_turns,
            )
        });
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
            SessionCommand::RunTurn {
                run_id,
                turn_id,
                request_id,
                text,
                reply,
            } => {
                let _ = reply.send(
                    service
                        .run_user_turn_with_ids(run_id, turn_id, request_id, &text)
                        .map_err(|err| format!("{err:#}")),
                );
            }
            SessionCommand::ClearContext { reply } => {
                let _ = reply.send(service.clear_context().map_err(|err| format!("{err:#}")));
            }
            SessionCommand::RequestExit { reply } => {
                let result = service.request_exit().map_err(|err| format!("{err:#}"));
                let finished = result.is_ok();
                let _ = reply.send(result);
                if finished {
                    break;
                }
            }
        }
    }
    if let Err(err) = service.finish_eof_if_needed() {
        tracing::warn!(
            target: "agentlibre::daemon",
            session_id = %service.session_id(),
            error = %err,
            "failed to release daemon session inference context"
        );
    }
}

#[cfg(test)]
fn run_test_session_worker(
    receiver: mpsc::Receiver<SessionCommand>,
    session_id: SessionId,
    mut outputs: Vec<ChatTurnStatus>,
    delay: Option<std::time::Duration>,
    emit_runtime_events: bool,
    turns: Arc<AtomicUsize>,
) {
    while let Ok(command) = receiver.recv() {
        match command {
            SessionCommand::RunTurn {
                run_id,
                turn_id,
                request_id,
                text,
                reply,
            } => {
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
                let terminal_status = match &status {
                    ChatTurnStatus::Answered { .. } => TurnFinishStatus::Answered,
                    ChatTurnStatus::Stopped { .. } => TurnFinishStatus::Stopped,
                    ChatTurnStatus::Failed { .. } => TurnFinishStatus::Failed,
                };
                let mut runtime_events = Vec::new();
                if emit_runtime_events {
                    runtime_events.push(
                        serde_json::from_value(serde_json::json!({
                            "schema": "agentlibre.event.v1alpha",
                            "event_id": EventId::generate(),
                            "sequence": 1,
                            "occurred_at_unix_ms": 1,
                            "scope": {
                                "run_id": run_id,
                                "session_id": session_id,
                                "turn_id": turn_id,
                            },
                            "request_id": request_id,
                            "payload": {
                                "kind": "turn.started",
                                "user_input_bytes": text.len(),
                            },
                        }))
                        .expect("test runtime event must deserialize"),
                    );
                }
                runtime_events.push(
                    serde_json::from_value(serde_json::json!({
                        "schema": "agentlibre.event.v1alpha",
                        "event_id": EventId::generate(),
                        "sequence": runtime_events.len() + 1,
                        "occurred_at_unix_ms": 2,
                        "scope": {
                            "run_id": run_id,
                            "session_id": session_id,
                            "turn_id": turn_id,
                        },
                        "request_id": request_id,
                        "payload": {
                            "kind": "turn.finished",
                            "status": terminal_status,
                        },
                    }))
                    .expect("test terminal runtime event must deserialize"),
                );
                let _ = reply.send(Ok(ChatTurnOutput {
                    run_id,
                    turn_id,
                    attempt_ids: Vec::new(),
                    runtime_events,
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
    request_id: RequestId,
    session_id: SessionId,
    run_id: RunId,
    turn_id: TurnId,
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
#[serde(deny_unknown_fields)]
struct TurnReplayPayload {
    version: u32,
    session_id: SessionId,
    run_id: RunId,
    turn_id: TurnId,
    terminal: TurnReplayTerminal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
enum TurnReplayTerminal {
    Answered { answer: String },
    Stopped { reason: String },
    Failed { message: String },
}

pub(crate) fn validate_runtime_envelopes(
    events: &[SafeRuntimeEventEnvelope],
    request_id: &RequestId,
    session_id: &SessionId,
    run_id: &RunId,
    turn_id: &TurnId,
    expected_terminal_status: TurnFinishStatus,
) -> Result<()> {
    if events.is_empty() {
        return Err(anyhow!("chat turn did not produce runtime events"));
    }

    let mut event_ids = BTreeSet::new();
    for (index, event) in events.iter().enumerate() {
        event
            .validate()
            .with_context(|| format!("runtime event {} has an invalid envelope", index + 1))?;
        let expected_sequence = u64::try_from(index + 1)
            .context("runtime event stream length exceeds the sequence range")?;
        if event.sequence != expected_sequence {
            return Err(anyhow!(
                "runtime event sequence is not contiguous: expected {}, got {}",
                expected_sequence,
                event.sequence
            ));
        }
        if event.request_id.as_ref() != Some(request_id) {
            return Err(anyhow!(
                "runtime event {} request ID does not match daemon admission",
                event.event_id
            ));
        }
        if event.scope.session_id() != Some(session_id)
            || event.scope.run_id() != run_id
            || event.scope.turn_id() != Some(turn_id)
        {
            return Err(anyhow!(
                "runtime event {} scope does not match daemon admission",
                event.event_id
            ));
        }
        if !event_ids.insert(event.event_id.clone()) {
            return Err(anyhow!(
                "runtime event stream contains duplicate event ID {}",
                event.event_id
            ));
        }
        if matches!(event.payload, SafeRuntimeEvent::TurnFinished { .. })
            && index + 1 != events.len()
        {
            return Err(anyhow!(
                "terminal runtime event {} is not last",
                event.event_id
            ));
        }
    }

    match &events
        .last()
        .expect("non-empty runtime event stream")
        .payload
    {
        SafeRuntimeEvent::TurnFinished { status } if status == &expected_terminal_status => Ok(()),
        SafeRuntimeEvent::TurnFinished { status } => Err(anyhow!(
            "runtime terminal status {status:?} does not match chat turn status {expected_terminal_status:?}"
        )),
        payload => Err(anyhow!("last runtime event is not terminal: {payload:?}")),
    }
}

fn replay_turn_idempotency(
    request_id: &RequestId,
    session_id: &SessionId,
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
    if &payload.session_id != session_id {
        return Err(runtime_error("idempotent turn replay session mismatch"));
    }

    Ok(TurnIdempotencyAdmission::Replay(replay_events(
        request_id,
        session_id,
        &payload.run_id,
        &payload.turn_id,
        payload.terminal,
    )))
}

fn replay_events(
    request_id: &RequestId,
    session_id: &SessionId,
    run_id: &RunId,
    turn_id: &TurnId,
    terminal: TurnReplayTerminal,
) -> Vec<DaemonEvent> {
    match terminal {
        TurnReplayTerminal::Answered { answer } => vec![
            DaemonEvent::new(
                Some(request_id.clone()),
                DaemonEventKind::TurnStarted(TurnStartedEvent {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    turn_id: turn_id.clone(),
                }),
            ),
            DaemonEvent::new(
                Some(request_id.clone()),
                DaemonEventKind::AssistantMessage(AssistantMessageEvent {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    turn_id: turn_id.clone(),
                    content: answer,
                }),
            ),
            DaemonEvent::new(
                Some(request_id.clone()),
                DaemonEventKind::TurnFinished(TurnFinishedEvent {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    turn_id: turn_id.clone(),
                    status: TurnTerminalStatus::Answered,
                }),
            ),
        ],
        TurnReplayTerminal::Stopped { reason } => vec![
            DaemonEvent::new(
                Some(request_id.clone()),
                DaemonEventKind::TurnStarted(TurnStartedEvent {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    turn_id: turn_id.clone(),
                }),
            ),
            DaemonEvent::new(
                Some(request_id.clone()),
                DaemonEventKind::TurnStopped(TurnStoppedEvent {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    turn_id: turn_id.clone(),
                    reason,
                }),
            ),
            DaemonEvent::new(
                Some(request_id.clone()),
                DaemonEventKind::TurnFinished(TurnFinishedEvent {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    turn_id: turn_id.clone(),
                    status: TurnTerminalStatus::Stopped,
                }),
            ),
        ],
        TurnReplayTerminal::Failed { message } => vec![
            DaemonEvent::new(
                Some(request_id.clone()),
                DaemonEventKind::TurnStarted(TurnStartedEvent {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    turn_id: turn_id.clone(),
                }),
            ),
            DaemonEvent::new(
                Some(request_id.clone()),
                DaemonEventKind::TurnFailed(TurnFailedEvent {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    turn_id: turn_id.clone(),
                    message,
                }),
            ),
            DaemonEvent::new(
                Some(request_id.clone()),
                DaemonEventKind::TurnFinished(TurnFinishedEvent {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    turn_id: turn_id.clone(),
                    status: TurnTerminalStatus::Failed,
                }),
            ),
        ],
    }
}

fn turn_idempotency_fingerprint(session_id: &SessionId, text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agentlibre.daemon.session_turn.v1\0");
    hasher.update(session_id.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(text.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn status_event(
    request_id: &RequestId,
    session_id: &SessionId,
    status: SessionStatus,
) -> DaemonEvent {
    DaemonEvent::new(
        Some(request_id.clone()),
        DaemonEventKind::SessionStatus(SessionStatusEvent {
            session_id: session_id.clone(),
            status,
        }),
    )
}

fn protocol_error_event(
    request_id: Option<RequestId>,
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
