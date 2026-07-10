use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

use agl_events::{SafeRuntimeEvent, TurnFinishStatus};
use agl_ids::{EventId, RequestId, RunId, SessionId, TurnId};
use agl_protocol::{
    DaemonEvent, DaemonEventKind, DaemonRequest, DaemonRequestKind, HelloEvent, HelloRequest,
    ProtocolError, ProtocolErrorCode, SessionClearRequest, SessionFinishRequest,
    SessionFinishedEvent, SessionListEvent, SessionListRequest, SessionOpenRequest,
    SessionOpenedEvent, SessionStatusEvent, SessionStatusRequest, SessionTranscriptEvent,
    SessionTranscriptRequest, SessionTurnRequest, TurnTerminalStatus,
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

    pub fn send_turn(&mut self, request: SessionTurnRequest) -> Result<TurnResponse, ClientError> {
        let session_id = request.session_id.clone();
        let request_id = self.send(DaemonRequestKind::SessionTurn(request))?;
        let mut events = Vec::new();
        let mut assistant_text = String::new();
        let mut admitted: Option<(RunId, TurnId)> = None;
        let mut pending_failure: Option<ProtocolError> = None;
        let mut runtime_terminal: Option<TurnTerminalStatus> = None;
        let mut stopped = false;
        let mut runtime_sequence = 0_u64;
        let mut runtime_event_ids = HashSet::<EventId>::new();
        loop {
            let event = self.read_correlated_event(&request_id)?;
            match &event.kind {
                DaemonEventKind::TurnStarted(started) => {
                    if started.session_id != session_id || admitted.is_some() {
                        return Err(ClientError::TurnIdentityMismatch(
                            "invalid or duplicate turn admission".to_string(),
                        ));
                    }
                    admitted = Some((started.run_id.clone(), started.turn_id.clone()));
                    events.push(event);
                }
                DaemonEventKind::RuntimeEvent(runtime) => {
                    if runtime_terminal.is_some() {
                        return Err(ClientError::TurnIdentityMismatch(
                            "runtime event arrived after the runtime terminal".to_string(),
                        ));
                    }
                    let Some((run_id, turn_id)) = admitted.as_ref() else {
                        return Err(ClientError::TurnIdentityMismatch(
                            "runtime event arrived before admission".to_string(),
                        ));
                    };
                    if runtime.scope.run_id() != run_id
                        || runtime.scope.turn_id() != Some(turn_id)
                        || runtime.scope.session_id() != Some(&session_id)
                    {
                        return Err(ClientError::TurnIdentityMismatch(
                            "runtime envelope does not match admitted session/run/turn".to_string(),
                        ));
                    }
                    if runtime.request_id.as_ref() != Some(&request_id) {
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
                DaemonEventKind::AssistantMessage(message) => {
                    validate_control_identity(
                        &session_id,
                        admitted.as_ref(),
                        &message.session_id,
                        &message.run_id,
                        &message.turn_id,
                    )?;
                    assistant_text.push_str(&message.content);
                    events.push(event);
                }
                DaemonEventKind::TurnStopped(stopped_event) => {
                    validate_control_identity(
                        &session_id,
                        admitted.as_ref(),
                        &stopped_event.session_id,
                        &stopped_event.run_id,
                        &stopped_event.turn_id,
                    )?;
                    if stopped {
                        return Err(ClientError::TurnIdentityMismatch(
                            "duplicate turn_stopped event".to_string(),
                        ));
                    }
                    stopped = true;
                    events.push(event);
                }
                DaemonEventKind::TurnFinished(finished) => {
                    validate_control_identity(
                        &session_id,
                        admitted.as_ref(),
                        &finished.session_id,
                        &finished.run_id,
                        &finished.turn_id,
                    )?;
                    let status = finished.status;
                    let (run_id, turn_id) = admitted.clone().ok_or_else(|| {
                        ClientError::TurnIdentityMismatch(
                            "terminal event arrived before turn admission".to_string(),
                        )
                    })?;
                    if runtime_sequence != 0 && runtime_terminal != Some(status) {
                        return Err(ClientError::TurnIdentityMismatch(
                            "runtime stream is incomplete or its terminal status does not match the control terminal"
                                .to_string(),
                        ));
                    }
                    if pending_failure.is_some() != (status == TurnTerminalStatus::Failed) {
                        return Err(ClientError::TurnIdentityMismatch(
                            "turn_failed detail does not match the control terminal status"
                                .to_string(),
                        ));
                    }
                    if stopped != (status == TurnTerminalStatus::Stopped) {
                        return Err(ClientError::TurnIdentityMismatch(
                            "turn_stopped detail does not match the control terminal status"
                                .to_string(),
                        ));
                    }
                    events.push(event);
                    if status == TurnTerminalStatus::Failed {
                        return Err(ClientError::Protocol(pending_failure.unwrap_or_else(
                            || {
                                ProtocolError::new(
                                    ProtocolErrorCode::RuntimeFailure,
                                    "turn failed without a preceding failure detail",
                                    false,
                                )
                            },
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
                DaemonEventKind::TurnFailed(failed) => {
                    validate_control_identity(
                        &session_id,
                        admitted.as_ref(),
                        &failed.session_id,
                        &failed.run_id,
                        &failed.turn_id,
                    )?;
                    let message = failed.message.clone();
                    events.push(event);
                    pending_failure = Some(ProtocolError::new(
                        ProtocolErrorCode::RuntimeFailure,
                        message,
                        false,
                    ));
                }
                DaemonEventKind::Error(error) => return Err(ClientError::Protocol(error.clone())),
                _ => events.push(event),
            }
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

fn validate_control_identity(
    requested_session_id: &SessionId,
    admitted: Option<&(RunId, TurnId)>,
    actual_session_id: &SessionId,
    actual_run_id: &RunId,
    actual_turn_id: &TurnId,
) -> Result<(), ClientError> {
    let Some((run_id, turn_id)) = admitted else {
        return Err(ClientError::TurnIdentityMismatch(
            "turn event arrived before admission".to_string(),
        ));
    };
    if actual_session_id != requested_session_id
        || actual_run_id != run_id
        || actual_turn_id != turn_id
    {
        return Err(ClientError::TurnIdentityMismatch(
            "control event does not match admitted session/run/turn".to_string(),
        ));
    }
    Ok(())
}

fn protocol_terminal_status(status: &TurnFinishStatus) -> TurnTerminalStatus {
    match status {
        TurnFinishStatus::Answered => TurnTerminalStatus::Answered,
        TurnFinishStatus::Stopped => TurnTerminalStatus::Stopped,
        TurnFinishStatus::Failed => TurnTerminalStatus::Failed,
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
        DaemonEventKind::TurnStarted(_) => "turn_started",
        DaemonEventKind::RuntimeEvent(_) => "runtime_event",
        DaemonEventKind::AssistantMessage(_) => "assistant_message",
        DaemonEventKind::TurnStopped(_) => "turn_stopped",
        DaemonEventKind::TurnFinished(_) => "turn_finished",
        DaemonEventKind::TurnFailed(_) => "turn_failed",
        DaemonEventKind::SessionFinished(_) => "session_finished",
        DaemonEventKind::SessionStatus(_) => "session_status",
        DaemonEventKind::SessionList(_) => "session_list",
        DaemonEventKind::SessionTranscript(_) => "session_transcript",
        DaemonEventKind::Error(_) => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    use agl_events::{EVENT_SCHEMA as RUNTIME_EVENT_SCHEMA, EventEnvelope, EventScope};
    use agl_ids::{EventId, RunId, SessionId, TurnId};
    use agl_protocol::{
        AssistantMessageEvent, DaemonCapability, EVENT_SCHEMA, PROTOCOL_VERSION, REQUEST_SCHEMA,
        SessionStatus, TurnFailedEvent, TurnFinishedEvent, TurnStartedEvent, TurnStoppedEvent,
    };

    const SESSION_ID: &str = "ses_01890f17-4a00-7000-8000-000000000001";
    const RUN_ID: &str = "run_01890f17-4a00-7000-8000-000000000002";
    const TURN_ID: &str = "turn_01890f17-4a00-7000-8000-000000000003";
    const OTHER_REQUEST_ID: &str = "req_01890f17-4a00-7000-8000-000000000004";

    #[derive(Default)]
    struct ScriptedTransport {
        writes: Vec<String>,
        reads: VecDeque<String>,
        correlate_reads: bool,
    }

    impl ScriptedTransport {
        fn with_events(events: Vec<DaemonEvent>) -> Self {
            Self {
                writes: Vec::new(),
                reads: events
                    .into_iter()
                    .map(|event| serde_json::to_string(&event).unwrap())
                    .collect(),
                correlate_reads: true,
            }
        }

        fn with_uncorrelated_events(events: Vec<DaemonEvent>) -> Self {
            Self {
                correlate_reads: false,
                ..Self::with_events(events)
            }
        }
    }

    impl DaemonTransport for ScriptedTransport {
        fn write_line(&mut self, line: &str) -> Result<(), ClientError> {
            self.writes.push(line.to_string());
            if self.correlate_reads {
                let request: DaemonRequest = serde_json::from_str(line)?;
                for encoded in &mut self.reads {
                    let mut event: DaemonEvent = serde_json::from_str(encoded)?;
                    event.request_id = Some(request.request_id.clone());
                    if let DaemonEventKind::RuntimeEvent(runtime) = &mut event.kind
                        && runtime.request_id.is_none()
                    {
                        runtime.request_id = Some(request.request_id.clone());
                    }
                    *encoded = serde_json::to_string(&event)?;
                }
            }
            Ok(())
        }

        fn read_line(&mut self) -> Result<String, ClientError> {
            self.reads.pop_front().ok_or(ClientError::EmptyResponse)
        }
    }

    #[test]
    fn hello_writes_alpha_request_and_decodes_response() {
        let event = DaemonEvent::new(
            None,
            DaemonEventKind::Hello(HelloEvent {
                protocol_version: PROTOCOL_VERSION.to_string(),
                product_version: "1.0.0-alpha.6".to_string(),
                capabilities: vec![DaemonCapability::SessionOpen],
            }),
        );
        let transport = ScriptedTransport::with_events(vec![event]);
        let mut client = AgentLibreClient::new(transport);

        let response = client.hello(HelloRequest {
            client_name: Some("matrix".to_string()),
            accepted_protocol_versions: vec![PROTOCOL_VERSION.to_string()],
        });

        assert_eq!(response.unwrap().protocol_version, PROTOCOL_VERSION);
        let request: DaemonRequest =
            serde_json::from_str(&client.transport.writes[0]).expect("request JSON");
        assert_eq!(request.schema, REQUEST_SCHEMA);
        assert!(request.request_id.as_str().starts_with("req_"));
        assert!(matches!(request.kind, DaemonRequestKind::Hello(_)));
    }

    #[test]
    fn turn_response_collects_stream_until_terminal_event() {
        let events = vec![
            DaemonEvent::new(
                None,
                DaemonEventKind::TurnStarted(TurnStartedEvent {
                    session_id: SessionId::parse(SESSION_ID).unwrap(),
                    run_id: RunId::parse(RUN_ID).unwrap(),
                    turn_id: TurnId::parse(TURN_ID).unwrap(),
                }),
            ),
            DaemonEvent::new(
                None,
                DaemonEventKind::AssistantMessage(AssistantMessageEvent {
                    session_id: SessionId::parse(SESSION_ID).unwrap(),
                    run_id: RunId::parse(RUN_ID).unwrap(),
                    turn_id: TurnId::parse(TURN_ID).unwrap(),
                    content: "hello ".to_string(),
                }),
            ),
            DaemonEvent::new(
                None,
                DaemonEventKind::AssistantMessage(AssistantMessageEvent {
                    session_id: SessionId::parse(SESSION_ID).unwrap(),
                    run_id: RunId::parse(RUN_ID).unwrap(),
                    turn_id: TurnId::parse(TURN_ID).unwrap(),
                    content: "there".to_string(),
                }),
            ),
            DaemonEvent::new(
                None,
                DaemonEventKind::TurnFinished(TurnFinishedEvent {
                    session_id: SessionId::parse(SESSION_ID).unwrap(),
                    run_id: RunId::parse(RUN_ID).unwrap(),
                    turn_id: TurnId::parse(TURN_ID).unwrap(),
                    status: TurnTerminalStatus::Answered,
                }),
            ),
        ];
        let mut client = AgentLibreClient::new(ScriptedTransport::with_events(events));

        let response = client
            .send_turn(SessionTurnRequest {
                session_id: SessionId::parse(SESSION_ID).unwrap(),
                text: "say hi".to_string(),
                idempotency_key: None,
            })
            .unwrap();

        assert_eq!(response.assistant_text, "hello there");
        assert_eq!(response.status, TurnTerminalStatus::Answered);
        assert_eq!(response.session_id.as_str(), SESSION_ID);
        assert_eq!(response.run_id.as_str(), RUN_ID);
        assert_eq!(response.turn_id.as_str(), TURN_ID);
        assert_eq!(response.events.len(), 4);
    }

    #[test]
    fn runtime_stream_accepts_exact_identity_and_contiguous_unique_events() {
        let events = vec![
            turn_started_event(),
            runtime_event(1, EventId::generate(), Some(session_id()), None),
            runtime_event_with_payload(
                2,
                SafeRuntimeEvent::TurnFinished {
                    status: TurnFinishStatus::Answered,
                },
            ),
            turn_finished_event(TurnTerminalStatus::Answered),
        ];
        let mut client = AgentLibreClient::new(ScriptedTransport::with_events(events));

        let response = client.send_turn(turn_request()).unwrap();

        let runtime_events = response
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                DaemonEventKind::RuntimeEvent(runtime) => Some(runtime.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            runtime_events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(
            runtime_events
                .iter()
                .all(|event| event.scope.session_id() == Some(&response.session_id))
        );
    }

    #[test]
    fn runtime_stream_rejects_missing_session_scope() {
        let events = vec![
            turn_started_event(),
            runtime_event(1, EventId::generate(), None, None),
        ];
        let mut client = AgentLibreClient::new(ScriptedTransport::with_events(events));

        let error = client.send_turn(turn_request()).unwrap_err();

        assert!(matches!(error, ClientError::TurnIdentityMismatch(_)));
        assert!(error.to_string().contains("session/run/turn"));
    }

    #[test]
    fn runtime_stream_rejects_inner_request_mismatch() {
        let events = vec![
            turn_started_event(),
            runtime_event(
                1,
                EventId::generate(),
                Some(session_id()),
                Some(RequestId::parse(OTHER_REQUEST_ID).unwrap()),
            ),
        ];
        let mut client = AgentLibreClient::new(ScriptedTransport::with_events(events));

        let error = client.send_turn(turn_request()).unwrap_err();

        assert!(matches!(error, ClientError::TurnIdentityMismatch(_)));
        assert!(error.to_string().contains("outer request"));
    }

    #[test]
    fn runtime_stream_rejects_sequence_gaps() {
        let events = vec![
            turn_started_event(),
            runtime_event(1, EventId::generate(), Some(session_id()), None),
            runtime_event(3, EventId::generate(), Some(session_id()), None),
        ];
        let mut client = AgentLibreClient::new(ScriptedTransport::with_events(events));

        let error = client.send_turn(turn_request()).unwrap_err();

        assert!(matches!(error, ClientError::TurnIdentityMismatch(_)));
        assert!(error.to_string().contains("expected 2"));
    }

    #[test]
    fn runtime_stream_sequence_must_start_at_one() {
        let events = vec![
            turn_started_event(),
            runtime_event(2, EventId::generate(), Some(session_id()), None),
        ];
        let mut client = AgentLibreClient::new(ScriptedTransport::with_events(events));

        let error = client.send_turn(turn_request()).unwrap_err();

        assert!(matches!(error, ClientError::TurnIdentityMismatch(_)));
        assert!(error.to_string().contains("expected 1"));
    }

    #[test]
    fn runtime_stream_rejects_duplicate_event_ids() {
        let event_id = EventId::generate();
        let events = vec![
            turn_started_event(),
            runtime_event(1, event_id.clone(), Some(session_id()), None),
            runtime_event(2, event_id, Some(session_id()), None),
        ];
        let mut client = AgentLibreClient::new(ScriptedTransport::with_events(events));

        let error = client.send_turn(turn_request()).unwrap_err();

        assert!(matches!(error, ClientError::TurnIdentityMismatch(_)));
        assert!(error.to_string().contains("duplicate runtime event_id"));
    }

    #[test]
    fn turn_stopped_requires_the_admitted_identity() {
        let events = vec![
            turn_started_event(),
            DaemonEvent::new(
                None,
                DaemonEventKind::TurnStopped(TurnStoppedEvent {
                    session_id: SessionId::generate(),
                    run_id: run_id(),
                    turn_id: turn_id(),
                    reason: "hidden_tool".to_string(),
                }),
            ),
        ];
        let mut client = AgentLibreClient::new(ScriptedTransport::with_events(events));

        let error = client.send_turn(turn_request()).unwrap_err();

        assert!(matches!(error, ClientError::TurnIdentityMismatch(_)));
        assert!(error.to_string().contains("control event"));
    }

    #[test]
    fn runtime_and_control_terminal_statuses_must_match() {
        let events = vec![
            turn_started_event(),
            runtime_event(1, EventId::generate(), Some(session_id()), None),
            runtime_event_with_payload(
                2,
                SafeRuntimeEvent::TurnFinished {
                    status: TurnFinishStatus::Answered,
                },
            ),
            turn_finished_event(TurnTerminalStatus::Stopped),
        ];
        let mut client = AgentLibreClient::new(ScriptedTransport::with_events(events));

        let error = client.send_turn(turn_request()).unwrap_err();

        assert!(matches!(error, ClientError::TurnIdentityMismatch(_)));
        assert!(error.to_string().contains("terminal status"));
    }

    #[test]
    fn partial_runtime_stream_requires_its_terminal_event() {
        let events = vec![
            turn_started_event(),
            runtime_event(1, EventId::generate(), Some(session_id()), None),
            turn_finished_event(TurnTerminalStatus::Answered),
        ];
        let mut client = AgentLibreClient::new(ScriptedTransport::with_events(events));

        let error = client.send_turn(turn_request()).unwrap_err();

        assert!(matches!(error, ClientError::TurnIdentityMismatch(_)));
        assert!(error.to_string().contains("runtime stream is incomplete"));
    }

    #[test]
    fn turn_failure_is_returned_only_after_matching_terminal_event() {
        let events = vec![
            turn_started_event(),
            runtime_event(1, EventId::generate(), Some(session_id()), None),
            runtime_event_with_payload(
                2,
                SafeRuntimeEvent::TurnFinished {
                    status: TurnFinishStatus::Failed,
                },
            ),
            DaemonEvent::new(
                None,
                DaemonEventKind::TurnFailed(TurnFailedEvent {
                    session_id: session_id(),
                    run_id: run_id(),
                    turn_id: turn_id(),
                    message: "model backend failed".to_string(),
                }),
            ),
            turn_finished_event(TurnTerminalStatus::Failed),
        ];
        let mut client = AgentLibreClient::new(ScriptedTransport::with_events(events));

        let error = client.send_turn(turn_request()).unwrap_err();

        match error {
            ClientError::Protocol(error) => {
                assert_eq!(error.code, ProtocolErrorCode::RuntimeFailure);
                assert_eq!(error.message, "model backend failed");
            }
            other => panic!("unexpected error: {other}"),
        }
        assert!(client.transport.reads.is_empty());
    }

    #[test]
    fn protocol_error_is_returned_without_untyped_string_matching() {
        let error = ProtocolError::new(ProtocolErrorCode::Busy, "session is busy", true);
        let event = DaemonEvent::new(None, DaemonEventKind::Error(error));
        let mut client = AgentLibreClient::new(ScriptedTransport::with_events(vec![event]));

        let result = client.session_status(SessionStatusRequest {
            session_id: SessionId::parse(SESSION_ID).unwrap(),
        });

        match result.unwrap_err() {
            ClientError::Protocol(error) => {
                assert_eq!(error.code, ProtocolErrorCode::Busy);
                assert!(error.retryable);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn list_sessions_uses_protocol_session_list_request() {
        let event = DaemonEvent::new(
            None,
            DaemonEventKind::SessionList(SessionListEvent {
                sessions: Vec::new(),
            }),
        );
        let mut client = AgentLibreClient::new(ScriptedTransport::with_events(vec![event]));

        let response = client.list_sessions(SessionListRequest::default()).unwrap();

        assert!(response.sessions.is_empty());
        let request: DaemonRequest =
            serde_json::from_str(&client.transport.writes[0]).expect("request JSON");
        assert!(matches!(request.kind, DaemonRequestKind::SessionList(_)));
    }

    #[test]
    fn request_id_mismatch_fails_closed() {
        let event = DaemonEvent {
            schema: EVENT_SCHEMA.to_string(),
            request_id: Some(RequestId::parse(OTHER_REQUEST_ID).unwrap()),
            safe_metadata: Default::default(),
            kind: DaemonEventKind::SessionStatus(SessionStatusEvent {
                session_id: SessionId::parse(SESSION_ID).unwrap(),
                status: SessionStatus::Open,
            }),
        };
        let mut client =
            AgentLibreClient::new(ScriptedTransport::with_uncorrelated_events(vec![event]));

        let result = client.session_status(SessionStatusRequest {
            session_id: SessionId::parse(SESSION_ID).unwrap(),
        });

        assert!(matches!(result, Err(ClientError::RequestMismatch { .. })));
    }

    #[test]
    fn client_manifest_stays_on_protocol_boundary() {
        let manifest = include_str!("../Cargo.toml");

        assert!(manifest.contains("agl-protocol.workspace = true"));
        for forbidden in ["agl-chat", "agl-loop", "agl-inference", "agl-cli"] {
            assert!(
                !has_dependency(manifest, forbidden),
                "agl-client must not depend on {forbidden}"
            );
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

    fn turn_request() -> SessionTurnRequest {
        SessionTurnRequest {
            session_id: session_id(),
            text: "test input".to_string(),
            idempotency_key: None,
        }
    }

    fn turn_started_event() -> DaemonEvent {
        DaemonEvent::new(
            None,
            DaemonEventKind::TurnStarted(TurnStartedEvent {
                session_id: session_id(),
                run_id: run_id(),
                turn_id: turn_id(),
            }),
        )
    }

    fn turn_finished_event(status: TurnTerminalStatus) -> DaemonEvent {
        DaemonEvent::new(
            None,
            DaemonEventKind::TurnFinished(TurnFinishedEvent {
                session_id: session_id(),
                run_id: run_id(),
                turn_id: turn_id(),
                status,
            }),
        )
    }

    fn runtime_event(
        sequence: u64,
        event_id: EventId,
        session_id: Option<SessionId>,
        request_id: Option<RequestId>,
    ) -> DaemonEvent {
        let mut scope = EventScope::builder(run_id()).turn_id(turn_id());
        if let Some(session_id) = session_id {
            scope = scope.session_id(session_id);
        }
        DaemonEvent::new(
            None,
            DaemonEventKind::RuntimeEvent(Box::new(EventEnvelope {
                schema: RUNTIME_EVENT_SCHEMA.to_string(),
                event_id,
                sequence,
                occurred_at_unix_ms: 1,
                scope: scope.build().unwrap(),
                request_id,
                caused_by: None,
                payload: SafeRuntimeEvent::ModelRequested {
                    request_index: sequence as usize,
                },
            })),
        )
    }

    fn runtime_event_with_payload(sequence: u64, payload: SafeRuntimeEvent) -> DaemonEvent {
        DaemonEvent::new(
            None,
            DaemonEventKind::RuntimeEvent(Box::new(EventEnvelope {
                schema: RUNTIME_EVENT_SCHEMA.to_string(),
                event_id: EventId::generate(),
                sequence,
                occurred_at_unix_ms: 1,
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

    fn has_dependency(manifest: &str, crate_name: &str) -> bool {
        manifest.lines().any(|line| {
            let line = line.trim_start();
            line.starts_with(&format!("{crate_name}."))
                || line.starts_with(&format!("{crate_name} ="))
        })
    }
}
