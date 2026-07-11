use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

use agl_events::{SafeRuntimeEvent, TurnFinishStatus};
use agl_ids::{EventId, RequestId, RunId, SessionId, TurnId};
use agl_protocol::{
    DaemonEvent, DaemonEventKind, DaemonRequest, DaemonRequestKind, HelloEvent, HelloRequest,
    ProtocolError, ProtocolErrorCode, ProtocolRunState, RunCancelRequest, RunEventsEvent,
    RunEventsRequest, RunStatusEvent, RunStatusRequest, RunSubmitRequest, RunSubscribeRequest,
    SessionClearRequest, SessionFinishRequest, SessionFinishedEvent, SessionListEvent,
    SessionListRequest, SessionOpenRequest, SessionOpenedEvent, SessionStatusEvent,
    SessionStatusRequest, SessionTranscriptEvent, SessionTranscriptRequest, TurnTerminalStatus,
};

#[cfg(unix)]
use std::os::unix::net::UnixStream;

#[derive(Debug)]
pub enum ClientError {
    Io(io::Error),
    Json(serde_json::Error),
    Protocol(ProtocolError),
    SchemaMismatch {
        expected: &'static str,
        actual: String,
    },
    RequestMismatch {
        expected: RequestId,
        actual: Option<RequestId>,
    },
    TurnIdentityMismatch(String),
    UnexpectedEvent {
        expected: &'static str,
        actual: String,
    },
    EmptyResponse,
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "daemon client I/O failed: {error}"),
            Self::Json(error) => write!(f, "daemon protocol JSON failed: {error}"),
            Self::Protocol(error) => {
                write!(f, "daemon returned {:?}: {}", error.code, error.message)
            }
            Self::SchemaMismatch { expected, actual } => {
                write!(f, "daemon returned schema {actual}, expected {expected}")
            }
            Self::RequestMismatch { expected, actual } => {
                write!(
                    f,
                    "daemon returned request_id {actual:?}, expected {expected}"
                )
            }
            Self::TurnIdentityMismatch(message) => {
                write!(f, "daemon turn identity mismatch: {message}")
            }
            Self::UnexpectedEvent { expected, actual } => {
                write!(f, "daemon returned event {actual}, expected {expected}")
            }
            Self::EmptyResponse => write!(f, "daemon closed the connection without a response"),
        }
    }
}

impl Error for ClientError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for ClientError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub trait DaemonTransport {
    fn write_line(&mut self, line: &str) -> Result<(), ClientError>;
    fn read_line(&mut self) -> Result<String, ClientError>;
}

#[cfg(unix)]
pub struct UnixTransport {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

#[cfg(unix)]
impl UnixTransport {
    pub fn connect(socket_path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let writer = UnixStream::connect(socket_path)?;
        let reader = BufReader::new(writer.try_clone()?);
        Ok(Self { reader, writer })
    }
}

#[cfg(unix)]
impl DaemonTransport for UnixTransport {
    fn write_line(&mut self, line: &str) -> Result<(), ClientError> {
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
    }

    fn read_line(&mut self) -> Result<String, ClientError> {
        let mut line = String::new();
        let bytes = self.reader.read_line(&mut line)?;
        if bytes == 0 {
            return Err(ClientError::EmptyResponse);
        }
        while line.ends_with('\n') || line.ends_with('\r') {
            line.pop();
        }
        Ok(line)
    }
}

pub struct AgentLibreClient<T> {
    transport: T,
}

#[cfg(unix)]
impl AgentLibreClient<UnixTransport> {
    pub fn connect(socket_path: impl AsRef<Path>) -> Result<Self, ClientError> {
        Ok(Self::new(UnixTransport::connect(socket_path)?))
    }
}

impl<T> AgentLibreClient<T>
where
    T: DaemonTransport,
{
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    pub fn hello(&mut self, request: HelloRequest) -> Result<HelloEvent, ClientError> {
        match self.single_response(DaemonRequestKind::Hello(request))? {
            DaemonEventKind::Hello(event) => Ok(event),
            other => Err(unexpected("hello", &other)),
        }
    }

    pub fn open_session(
        &mut self,
        request: SessionOpenRequest,
    ) -> Result<SessionOpenedEvent, ClientError> {
        match self.single_response(DaemonRequestKind::SessionOpen(request))? {
            DaemonEventKind::SessionOpened(event) => Ok(event),
            other => Err(unexpected("session_opened", &other)),
        }
    }

    pub fn send_turn(&mut self, request: RunSubmitRequest) -> Result<TurnResponse, ClientError> {
        let session_id = request.session_id.clone();
        let admission_request_id = self.send(DaemonRequestKind::RunSubmit(request))?;
        let accepted_event = self.read_correlated_event(&admission_request_id)?;
        let accepted = match &accepted_event.kind {
            DaemonEventKind::RunAccepted(accepted) if accepted.session_id == session_id => {
                accepted.clone()
            }
            DaemonEventKind::RunAccepted(_) => {
                return Err(ClientError::TurnIdentityMismatch(
                    "run admission returned a different session".to_string(),
                ));
            }
            DaemonEventKind::Error(error) => return Err(ClientError::Protocol(error.clone())),
            other => return Err(unexpected("run_accepted", other)),
        };
        let run_id = accepted.run_id.clone();
        let turn_id = accepted.turn_id.clone();
        let subscription_request_id =
            self.send(DaemonRequestKind::RunSubscribe(RunSubscribeRequest {
                run_id: run_id.clone(),
                after_sequence: 0,
            }))?;
        let mut events = vec![accepted_event];
        let mut runtime_terminal: Option<TurnTerminalStatus> = None;
        let mut runtime_sequence = 0_u64;
        let mut runtime_event_ids = HashSet::<EventId>::new();
        let mut subscription_started = false;
        loop {
            let event = self.read_correlated_event(&subscription_request_id)?;
            match &event.kind {
                DaemonEventKind::RunSubscriptionStarted(started) => {
                    if subscription_started
                        || started.run_id != run_id
                        || started.after_sequence != 0
                    {
                        return Err(ClientError::TurnIdentityMismatch(
                            "invalid or duplicate run subscription admission".to_string(),
                        ));
                    }
                    subscription_started = true;
                    events.push(event);
                }
                DaemonEventKind::RunEvent(runtime) => {
                    if !subscription_started {
                        return Err(ClientError::TurnIdentityMismatch(
                            "run event arrived before subscription admission".to_string(),
                        ));
                    }
                    if runtime_terminal.is_some() {
                        return Err(ClientError::TurnIdentityMismatch(
                            "runtime event arrived after the runtime terminal".to_string(),
                        ));
                    }
                    if runtime.scope.run_id() != &run_id
                        || runtime.scope.turn_id() != Some(&turn_id)
                        || runtime.scope.session_id() != Some(&session_id)
                    {
                        return Err(ClientError::TurnIdentityMismatch(
                            "runtime envelope does not match admitted session/run/turn".to_string(),
                        ));
                    }
                    if runtime.request_id.as_ref() != Some(&admission_request_id) {
                        return Err(ClientError::TurnIdentityMismatch(
                            "runtime envelope request_id does not match the outer request"
                                .to_string(),
                        ));
                    }
                    let expected_sequence = runtime_sequence.checked_add(1).ok_or_else(|| {
                        ClientError::TurnIdentityMismatch(
                            "runtime envelope sequence overflowed".to_string(),
                        )
                    })?;
                    if runtime.sequence != expected_sequence {
                        return Err(ClientError::TurnIdentityMismatch(format!(
                            "runtime envelope sequence {} is not the expected {}",
                            runtime.sequence, expected_sequence
                        )));
                    }
                    if !runtime_event_ids.insert(runtime.event_id.clone()) {
                        return Err(ClientError::TurnIdentityMismatch(format!(
                            "duplicate runtime event_id {}",
                            runtime.event_id
                        )));
                    }
                    runtime_sequence = runtime.sequence;
                    if let SafeRuntimeEvent::TurnFinished { status } = &runtime.payload {
                        runtime_terminal = Some(protocol_terminal_status(status));
                    }
                    events.push(event);
                }
                DaemonEventKind::RunSubscriptionFinished(finished) => {
                    if !subscription_started || finished.run_id != run_id {
                        return Err(ClientError::TurnIdentityMismatch(
                            "run subscription terminal does not match admission".to_string(),
                        ));
                    }
                    if finished.last_sequence != runtime_sequence {
                        return Err(ClientError::TurnIdentityMismatch(
                            "subscription terminal sequence does not match the runtime stream"
                                .to_string(),
                        ));
                    }
                    let status = terminal_status(finished.state, finished.terminal_result.as_ref());
                    if runtime_terminal.is_some() && runtime_terminal != Some(status) {
                        return Err(ClientError::TurnIdentityMismatch(
                            "runtime and subscription terminal statuses differ".to_string(),
                        ));
                    }
                    let assistant_text = finished
                        .terminal_result
                        .as_ref()
                        .and_then(|result| result.get("answer"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let failure_message = finished.error_message.clone();
                    events.push(event);
                    if status == TurnTerminalStatus::Failed {
                        return Err(ClientError::Protocol(ProtocolError::new(
                            ProtocolErrorCode::RuntimeFailure,
                            failure_message.unwrap_or_else(|| {
                                "run failed without terminal diagnostics".to_string()
                            }),
                            false,
                        )));
                    }
                    return Ok(TurnResponse {
                        session_id,
                        run_id,
                        turn_id,
                        events,
                        assistant_text,
                        status,
                    });
                }
                DaemonEventKind::Error(error) => return Err(ClientError::Protocol(error.clone())),
                other => return Err(unexpected("run subscription event", other)),
            }
        }
    }

    pub fn run_status(&mut self, run_id: RunId) -> Result<RunStatusEvent, ClientError> {
        match self.single_response(DaemonRequestKind::RunStatus(RunStatusRequest { run_id }))? {
            DaemonEventKind::RunStatus(status) => Ok(status),
            other => Err(unexpected("run_status", &other)),
        }
    }

    pub fn cancel_run(&mut self, run_id: RunId) -> Result<RunStatusEvent, ClientError> {
        match self.single_response(DaemonRequestKind::RunCancel(RunCancelRequest { run_id }))? {
            DaemonEventKind::RunStatus(status) => Ok(status),
            other => Err(unexpected("run_status", &other)),
        }
    }

    pub fn run_events(&mut self, request: RunEventsRequest) -> Result<RunEventsEvent, ClientError> {
        match self.single_response(DaemonRequestKind::RunEvents(request))? {
            DaemonEventKind::RunEvents(events) => Ok(events),
            other => Err(unexpected("run_events", &other)),
        }
    }

    pub fn clear_session(
        &mut self,
        request: SessionClearRequest,
    ) -> Result<SessionStatusEvent, ClientError> {
        match self.single_response(DaemonRequestKind::SessionClear(request))? {
            DaemonEventKind::SessionStatus(event) => Ok(event),
            other => Err(unexpected("session_status", &other)),
        }
    }

    pub fn finish_session(
        &mut self,
        request: SessionFinishRequest,
    ) -> Result<SessionFinishedEvent, ClientError> {
        match self.single_response(DaemonRequestKind::SessionFinish(request))? {
            DaemonEventKind::SessionFinished(event) => Ok(event),
            other => Err(unexpected("session_finished", &other)),
        }
    }

    pub fn session_status(
        &mut self,
        request: SessionStatusRequest,
    ) -> Result<SessionStatusEvent, ClientError> {
        match self.single_response(DaemonRequestKind::SessionStatus(request))? {
            DaemonEventKind::SessionStatus(event) => Ok(event),
            other => Err(unexpected("session_status", &other)),
        }
    }

    pub fn list_sessions(
        &mut self,
        request: SessionListRequest,
    ) -> Result<SessionListEvent, ClientError> {
        match self.single_response(DaemonRequestKind::SessionList(request))? {
            DaemonEventKind::SessionList(event) => Ok(event),
            other => Err(unexpected("session_list", &other)),
        }
    }

    pub fn read_transcript(
        &mut self,
        request: SessionTranscriptRequest,
    ) -> Result<SessionTranscriptEvent, ClientError> {
        match self.single_response(DaemonRequestKind::SessionTranscript(request))? {
            DaemonEventKind::SessionTranscript(event) => Ok(event),
            other => Err(unexpected("session_transcript", &other)),
        }
    }

    fn single_response(&mut self, kind: DaemonRequestKind) -> Result<DaemonEventKind, ClientError> {
        let request_id = self.send(kind)?;
        let event = self.read_correlated_event(&request_id)?;
        match event.kind {
            DaemonEventKind::Error(error) => Err(ClientError::Protocol(error)),
            other => Ok(other),
        }
    }

    fn send(&mut self, kind: DaemonRequestKind) -> Result<RequestId, ClientError> {
        let request_id = RequestId::generate();
        let request = DaemonRequest::new(request_id.clone(), kind);
        let line = serde_json::to_string(&request)?;
        self.transport.write_line(&line)?;
        Ok(request_id)
    }

    fn read_correlated_event(
        &mut self,
        request_id: &RequestId,
    ) -> Result<DaemonEvent, ClientError> {
        let line = self.transport.read_line()?;
        let event: DaemonEvent = serde_json::from_str(&line)?;
        if event.schema != agl_protocol::EVENT_SCHEMA {
            return Err(ClientError::SchemaMismatch {
                expected: agl_protocol::EVENT_SCHEMA,
                actual: event.schema,
            });
        }
        if event.request_id.as_ref() != Some(request_id) {
            return Err(ClientError::RequestMismatch {
                expected: request_id.clone(),
                actual: event.request_id,
            });
        }
        Ok(event)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TurnResponse {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub events: Vec<DaemonEvent>,
    pub assistant_text: String,
    pub status: TurnTerminalStatus,
}

fn protocol_terminal_status(status: &TurnFinishStatus) -> TurnTerminalStatus {
    match status {
        TurnFinishStatus::Answered => TurnTerminalStatus::Answered,
        TurnFinishStatus::Stopped => TurnTerminalStatus::Stopped,
        TurnFinishStatus::Failed => TurnTerminalStatus::Failed,
        TurnFinishStatus::Cancelled => TurnTerminalStatus::Cancelled,
    }
}

fn terminal_status(
    state: ProtocolRunState,
    result: Option<&serde_json::Value>,
) -> TurnTerminalStatus {
    match state {
        ProtocolRunState::Cancelled => TurnTerminalStatus::Cancelled,
        ProtocolRunState::Failed => TurnTerminalStatus::Failed,
        ProtocolRunState::Succeeded => match result
            .and_then(|result| result.get("status"))
            .and_then(serde_json::Value::as_str)
        {
            Some("stopped") => TurnTerminalStatus::Stopped,
            _ => TurnTerminalStatus::Answered,
        },
        ProtocolRunState::Queued | ProtocolRunState::Running | ProtocolRunState::Waiting => {
            TurnTerminalStatus::Failed
        }
    }
}

fn unexpected(expected: &'static str, actual: &DaemonEventKind) -> ClientError {
    ClientError::UnexpectedEvent {
        expected,
        actual: event_name(actual).to_string(),
    }
}

fn event_name(event: &DaemonEventKind) -> &'static str {
    match event {
        DaemonEventKind::Hello(_) => "hello",
        DaemonEventKind::SessionOpened(_) => "session_opened",
        DaemonEventKind::SessionFinished(_) => "session_finished",
        DaemonEventKind::SessionStatus(_) => "session_status",
        DaemonEventKind::SessionList(_) => "session_list",
        DaemonEventKind::SessionTranscript(_) => "session_transcript",
        DaemonEventKind::RunAccepted(_) => "run_accepted",
        DaemonEventKind::RunStatus(_) => "run_status",
        DaemonEventKind::RunEvents(_) => "run_events",
        DaemonEventKind::RunSubscriptionStarted(_) => "run_subscription_started",
        DaemonEventKind::RunEvent(_) => "run_event",
        DaemonEventKind::RunSubscriptionFinished(_) => "run_subscription_finished",
        DaemonEventKind::Error(_) => "error",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use agl_events::{EVENT_SCHEMA as RUNTIME_EVENT_SCHEMA, EventEnvelope, EventScope};
    use agl_protocol::{
        DaemonCapability, EVENT_SCHEMA, PROTOCOL_VERSION, REQUEST_SCHEMA, RunAcceptedEvent,
        RunBudgetRequest, RunSubscriptionFinishedEvent, RunSubscriptionStartedEvent, RunUsageEvent,
    };

    use super::*;

    const SESSION_ID: &str = "ses_01890f17-4a00-7000-8000-000000000001";
    const RUN_ID: &str = "run_01890f17-4a00-7000-8000-000000000002";
    const TURN_ID: &str = "turn_01890f17-4a00-7000-8000-000000000003";

    #[derive(Default)]
    struct ScriptedTransport {
        writes: Vec<String>,
        reads: VecDeque<String>,
    }

    impl ScriptedTransport {
        fn with_events(events: Vec<DaemonEvent>) -> Self {
            Self {
                writes: Vec::new(),
                reads: events
                    .into_iter()
                    .map(|event| serde_json::to_string(&event).unwrap())
                    .collect(),
            }
        }
    }

    impl DaemonTransport for ScriptedTransport {
        fn write_line(&mut self, line: &str) -> Result<(), ClientError> {
            self.writes.push(line.to_string());
            let request: DaemonRequest = serde_json::from_str(line)?;
            for encoded in &mut self.reads {
                let mut event: DaemonEvent = serde_json::from_str(encoded)?;
                event.request_id = Some(request.request_id.clone());
                if let DaemonEventKind::RunEvent(runtime) = &mut event.kind
                    && runtime.request_id.is_none()
                {
                    runtime.request_id = Some(request.request_id.clone());
                }
                *encoded = serde_json::to_string(&event)?;
            }
            Ok(())
        }

        fn read_line(&mut self) -> Result<String, ClientError> {
            self.reads.pop_front().ok_or(ClientError::EmptyResponse)
        }
    }

    #[test]
    fn hello_writes_current_strict_request() {
        let transport = ScriptedTransport::with_events(vec![DaemonEvent::new(
            None,
            DaemonEventKind::Hello(HelloEvent {
                protocol_version: PROTOCOL_VERSION.to_string(),
                product_version: "test".to_string(),
                capabilities: vec![DaemonCapability::RunSubmit],
            }),
        )]);
        let mut client = AgentLibreClient::new(transport);
        let response = client
            .hello(HelloRequest {
                client_name: Some("test".to_string()),
                accepted_protocol_versions: vec![PROTOCOL_VERSION.to_string()],
            })
            .unwrap();
        assert_eq!(response.protocol_version, PROTOCOL_VERSION);
        let request: DaemonRequest = serde_json::from_str(&client.transport.writes[0]).unwrap();
        assert_eq!(request.schema, REQUEST_SCHEMA);
        assert_eq!(EVENT_SCHEMA, agl_protocol::EVENT_SCHEMA);
    }

    #[test]
    fn turn_submission_collects_replay_and_live_events_until_terminal() {
        let events = successful_run_events();
        let transport = ScriptedTransport::with_events(events);
        let mut client = AgentLibreClient::new(transport);

        let response = client.send_turn(run_request()).unwrap();

        assert_eq!(response.session_id, session_id());
        assert_eq!(response.run_id, run_id());
        assert_eq!(response.turn_id, turn_id());
        assert_eq!(response.status, TurnTerminalStatus::Answered);
        assert_eq!(response.assistant_text, "done");
        assert_eq!(response.events.len(), 5);
        let requests = client
            .transport
            .writes
            .iter()
            .map(|line| serde_json::from_str::<DaemonRequest>(line).unwrap().kind)
            .collect::<Vec<_>>();
        assert!(matches!(requests[0], DaemonRequestKind::RunSubmit(_)));
        assert!(matches!(requests[1], DaemonRequestKind::RunSubscribe(_)));
    }

    #[test]
    fn runtime_sequence_gap_fails_closed() {
        let mut events = successful_run_events();
        if let DaemonEventKind::RunEvent(event) = &mut events[2].kind {
            event.sequence = 2;
        }
        let mut client = AgentLibreClient::new(ScriptedTransport::with_events(events));
        assert!(matches!(
            client.send_turn(run_request()),
            Err(ClientError::TurnIdentityMismatch(_))
        ));
    }

    #[test]
    fn failed_subscription_terminal_returns_protocol_error() {
        let events = vec![
            accepted_event(),
            subscription_started_event(),
            DaemonEvent::new(
                None,
                DaemonEventKind::RunSubscriptionFinished(RunSubscriptionFinishedEvent {
                    run_id: run_id(),
                    state: ProtocolRunState::Failed,
                    last_sequence: 0,
                    terminal_result: None,
                    error_code: Some("test.failure".to_string()),
                    error_message: Some("failed safely".to_string()),
                }),
            ),
        ];
        let mut client = AgentLibreClient::new(ScriptedTransport::with_events(events));
        assert!(matches!(
            client.send_turn(run_request()),
            Err(ClientError::Protocol(ProtocolError { ref message, .. })) if message == "failed safely"
        ));
    }

    #[test]
    fn status_cancel_and_replay_use_run_requests() {
        let status = RunStatusEvent {
            session_id: Some(session_id()),
            run_id: run_id(),
            turn_id: Some(turn_id()),
            state: ProtocolRunState::Running,
            usage: RunUsageEvent::default(),
            cancellation_requested: false,
            attempts: 1,
            created_at_ms: 1,
            updated_at_ms: 2,
            started_at_ms: Some(2),
            finished_at_ms: None,
            error_code: None,
            terminal_result: None,
            error_message: None,
        };
        let transport = ScriptedTransport::with_events(vec![DaemonEvent::new(
            None,
            DaemonEventKind::RunStatus(status.clone()),
        )]);
        let mut client = AgentLibreClient::new(transport);
        assert_eq!(client.run_status(run_id()).unwrap(), status);
    }

    #[test]
    fn client_manifest_stays_on_protocol_boundary() {
        let manifest = include_str!("../Cargo.toml");
        assert!(manifest.contains("agl-protocol.workspace = true"));
        assert!(!manifest.contains("agl-daemon.workspace = true"));
        assert!(!manifest.contains("agl-chat.workspace = true"));
    }

    fn successful_run_events() -> Vec<DaemonEvent> {
        vec![
            accepted_event(),
            subscription_started_event(),
            runtime_event(
                1,
                SafeRuntimeEvent::TurnStarted {
                    user_input_bytes: 4,
                },
            ),
            runtime_event(
                2,
                SafeRuntimeEvent::TurnFinished {
                    status: TurnFinishStatus::Answered,
                },
            ),
            DaemonEvent::new(
                None,
                DaemonEventKind::RunSubscriptionFinished(RunSubscriptionFinishedEvent {
                    run_id: run_id(),
                    state: ProtocolRunState::Succeeded,
                    last_sequence: 2,
                    terminal_result: Some(serde_json::json!({
                        "status": "answered",
                        "answer": "done"
                    })),
                    error_code: None,
                    error_message: None,
                }),
            ),
        ]
    }

    fn accepted_event() -> DaemonEvent {
        DaemonEvent::new(
            None,
            DaemonEventKind::RunAccepted(RunAcceptedEvent {
                session_id: session_id(),
                run_id: run_id(),
                turn_id: turn_id(),
                state: ProtocolRunState::Queued,
                replayed: false,
            }),
        )
    }

    fn subscription_started_event() -> DaemonEvent {
        DaemonEvent::new(
            None,
            DaemonEventKind::RunSubscriptionStarted(RunSubscriptionStartedEvent {
                run_id: run_id(),
                after_sequence: 0,
                replay_boundary: 0,
            }),
        )
    }

    fn runtime_event(sequence: u64, payload: SafeRuntimeEvent) -> DaemonEvent {
        DaemonEvent::new(
            None,
            DaemonEventKind::RunEvent(Box::new(EventEnvelope {
                schema: RUNTIME_EVENT_SCHEMA.to_string(),
                event_id: EventId::generate(),
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
            })),
        )
    }

    fn run_request() -> RunSubmitRequest {
        RunSubmitRequest {
            session_id: session_id(),
            content: agl_content::Content::text("test").unwrap(),
            idempotency_key: Some("key".to_string()),
            budget: RunBudgetRequest::default(),
        }
    }

    fn session_id() -> SessionId {
        SessionId::parse(SESSION_ID).unwrap()
    }

    fn run_id() -> RunId {
        RunId::parse(RUN_ID).unwrap()
    }

    fn turn_id() -> TurnId {
        TurnId::parse(TURN_ID).unwrap()
    }
}
