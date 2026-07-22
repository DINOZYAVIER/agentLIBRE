use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::net::UnixStream;
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tokio_util::codec::{Framed, LinesCodec};

use agl_ids::{RequestId, RunId};
use agl_protocol::*;

const OUTBOUND_CAPACITY: usize = 128;
const ONE_SHOT_CAPACITY: usize = 32;
const SUBSCRIPTION_CAPACITY: usize = 256;
const EXECUTION_ATTACHMENT_CAPACITY: usize = 256;
const ABANDONED_STREAM_CAPACITY: usize = 256;
const CONNECTION_ROUTE_CAPACITY: usize = 256;
const IGNORED_TERMINAL_CAPACITY: usize = 256;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientError {
    DaemonUnavailable(String),
    HandshakeTimeout,
    Io(String),
    InvalidProtocolFrame,
    Protocol {
        code: ProtocolErrorCode,
        retryable: bool,
    },
    SchemaMismatch {
        expected: &'static str,
    },
    RequestMismatch {
        expected: RequestId,
        actual: Option<RequestId>,
    },
    UnexpectedEvent {
        expected: &'static str,
        actual: &'static str,
    },
    IdentityMismatch(&'static str),
    SubscriptionLagged {
        request_id: RequestId,
        last_sequence: u64,
    },
    SequenceGap {
        expected: u64,
        actual: u64,
    },
    SnapshotChunkOutOfOrder {
        expected: u16,
        actual: u16,
    },
    SnapshotTransferInvalid(&'static str),
    SnapshotDigestMismatch,
    DaemonInstanceChanged,
    ConnectionClosed,
    InputBackpressure,
    FrameTooLarge,
    InvalidRequest(String),
}

impl Display for ClientError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::DaemonUnavailable(message) => {
                write!(formatter, "daemon is not available: {message}")
            }
            Self::HandshakeTimeout => formatter.write_str("daemon protocol handshake timed out"),
            Self::Io(message) => write!(formatter, "daemon connection I/O failed: {message}"),
            Self::InvalidProtocolFrame => formatter.write_str("daemon protocol frame was invalid"),
            Self::Protocol { code, retryable } => {
                write!(
                    formatter,
                    "daemon request failed with {code:?} (retryable={retryable})"
                )
            }
            Self::SchemaMismatch { expected } => {
                write!(formatter, "daemon schema does not match {expected}")
            }
            Self::RequestMismatch { expected, actual } => {
                write!(
                    formatter,
                    "daemon response request ID {actual:?} does not match {expected}"
                )
            }
            Self::UnexpectedEvent { expected, actual } => {
                write!(formatter, "daemon returned {actual}, expected {expected}")
            }
            Self::IdentityMismatch(message) => formatter.write_str(message),
            Self::SubscriptionLagged {
                request_id,
                last_sequence,
            } => write!(
                formatter,
                "subscription {request_id} lagged after sequence {last_sequence}"
            ),
            Self::SequenceGap { expected, actual } => {
                write!(
                    formatter,
                    "stream sequence {actual} is not expected sequence {expected}"
                )
            }
            Self::SnapshotChunkOutOfOrder { expected, actual } => write!(
                formatter,
                "presentation snapshot chunk {actual} arrived, expected {expected}"
            ),
            Self::SnapshotTransferInvalid(message) => formatter.write_str(message),
            Self::SnapshotDigestMismatch => {
                formatter.write_str("presentation snapshot transfer digest does not match")
            }
            Self::DaemonInstanceChanged => {
                formatter.write_str("daemon instance changed; request a fresh snapshot")
            }
            Self::ConnectionClosed => formatter.write_str("daemon connection closed"),
            Self::InputBackpressure => formatter.write_str("client request queue is full"),
            Self::FrameTooLarge => formatter.write_str("daemon protocol frame exceeds 1 MiB"),
            Self::InvalidRequest(message) => {
                write!(formatter, "daemon request is invalid: {message}")
            }
        }
    }
}

impl std::error::Error for ClientError {}

impl From<std::io::Error> for ClientError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

fn verify_peer(stream: &UnixStream) -> Result<(), ClientError> {
    let peer = stream.peer_cred()?;
    // SAFETY: geteuid has no preconditions and does not modify process state.
    let expected_uid = unsafe { libc::geteuid() };
    verify_peer_identity(peer.uid(), expected_uid)
}

fn verify_peer_identity(peer_uid: u32, expected_uid: u32) -> Result<(), ClientError> {
    if peer_uid != expected_uid {
        return Err(ClientError::IdentityMismatch(
            "daemon socket peer UID does not match the current user",
        ));
    }
    Ok(())
}

#[derive(Clone)]
pub struct AgentLibreClient {
    sender: mpsc::Sender<ConnectionCommand>,
    hello: Arc<RwLock<Option<HelloEvent>>>,
    one_shot_slots: Arc<Semaphore>,
}

impl AgentLibreClient {
    pub async fn connect(socket_path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(socket_path).await.map_err(|error| {
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) {
                ClientError::DaemonUnavailable(error.to_string())
            } else {
                ClientError::Io(error.to_string())
            }
        })?;
        Self::from_stream(stream).await
    }

    pub async fn from_stream(stream: UnixStream) -> Result<Self, ClientError> {
        verify_peer(&stream)?;
        Self::from_verified_stream(stream).await
    }

    async fn from_verified_stream(stream: UnixStream) -> Result<Self, ClientError> {
        Self::from_verified_stream_with_timeout(stream, HANDSHAKE_TIMEOUT).await
    }

    async fn from_verified_stream_with_timeout(
        stream: UnixStream,
        handshake_timeout: Duration,
    ) -> Result<Self, ClientError> {
        let (sender, receiver) = mpsc::channel(OUTBOUND_CAPACITY);
        tokio::spawn(connection_task(stream, receiver, sender.downgrade()));
        let client = Self {
            sender,
            hello: Arc::new(RwLock::new(None)),
            one_shot_slots: Arc::new(Semaphore::new(ONE_SHOT_CAPACITY)),
        };
        let hello_response = tokio::time::timeout(
            handshake_timeout,
            client.request(DaemonRequestKind::Hello(HelloRequest {
                client_name: Some("agl-client".to_owned()),
                accepted_protocol_versions: vec![PROTOCOL_VERSION.to_owned()],
            })),
        )
        .await
        .map_err(|_| ClientError::HandshakeTimeout)??;
        let hello = match hello_response {
            DaemonEventKind::Hello(event) => event,
            other => return Err(unexpected("hello", &other)),
        };
        if hello.protocol_version != PROTOCOL_VERSION {
            return Err(ClientError::SchemaMismatch {
                expected: PROTOCOL_VERSION,
            });
        }
        *client
            .hello
            .write()
            .map_err(|_| ClientError::ConnectionClosed)? = Some(hello);
        Ok(client)
    }

    #[cfg(test)]
    async fn from_test_stream(stream: UnixStream) -> Result<Self, ClientError> {
        Self::from_verified_stream(stream).await
    }

    #[cfg(test)]
    async fn from_test_stream_with_handshake_timeout(
        stream: UnixStream,
        handshake_timeout: Duration,
    ) -> Result<Self, ClientError> {
        Self::from_verified_stream_with_timeout(stream, handshake_timeout).await
    }

    pub fn hello(&self) -> Result<HelloEvent, ClientError> {
        self.hello
            .read()
            .map_err(|_| ClientError::ConnectionClosed)?
            .clone()
            .ok_or(ClientError::ConnectionClosed)
    }

    pub async fn open_session(
        &self,
        request: SessionOpenRequest,
    ) -> Result<SessionOpenedEvent, ClientError> {
        match self
            .request(DaemonRequestKind::SessionOpen(request))
            .await?
        {
            DaemonEventKind::SessionOpened(event) => Ok(event),
            other => Err(unexpected("session_opened", &other)),
        }
    }

    pub async fn open_setup_smoke_session(
        &self,
        request: SetupSmokeSessionOpenRequest,
    ) -> Result<SessionOpenedEvent, ClientError> {
        match self
            .request(DaemonRequestKind::SetupSmokeSessionOpen(request))
            .await?
        {
            DaemonEventKind::SessionOpened(event) => Ok(event),
            other => Err(unexpected("session_opened", &other)),
        }
    }

    pub async fn clear_session(
        &self,
        request: SessionClearRequest,
    ) -> Result<SessionStatusEvent, ClientError> {
        match self
            .request(DaemonRequestKind::SessionClear(request))
            .await?
        {
            DaemonEventKind::SessionStatus(event) => Ok(event),
            other => Err(unexpected("session_status", &other)),
        }
    }

    pub async fn finish_session(
        &self,
        request: SessionFinishRequest,
    ) -> Result<SessionFinishedEvent, ClientError> {
        match self
            .request(DaemonRequestKind::SessionFinish(request))
            .await?
        {
            DaemonEventKind::SessionFinished(event) => Ok(event),
            other => Err(unexpected("session_finished", &other)),
        }
    }

    pub async fn session_status(
        &self,
        request: SessionStatusRequest,
    ) -> Result<SessionStatusEvent, ClientError> {
        match self
            .request(DaemonRequestKind::SessionStatus(request))
            .await?
        {
            DaemonEventKind::SessionStatus(event) => Ok(event),
            other => Err(unexpected("session_status", &other)),
        }
    }

    pub async fn list_sessions(
        &self,
        request: SessionListRequest,
    ) -> Result<SessionListEvent, ClientError> {
        match self
            .request(DaemonRequestKind::SessionList(request))
            .await?
        {
            DaemonEventKind::SessionList(event) => Ok(event),
            other => Err(unexpected("session_list", &other)),
        }
    }

    pub async fn read_transcript(
        &self,
        request: SessionTranscriptRequest,
    ) -> Result<SessionTranscriptEvent, ClientError> {
        match self
            .request(DaemonRequestKind::SessionTranscript(request))
            .await?
        {
            DaemonEventKind::SessionTranscript(event) => Ok(event),
            other => Err(unexpected("session_transcript", &other)),
        }
    }

    pub async fn submit_run(
        &self,
        request: RunSubmitRequest,
    ) -> Result<RunAcceptedEvent, ClientError> {
        match self.request(DaemonRequestKind::RunSubmit(request)).await? {
            DaemonEventKind::RunAccepted(event) => Ok(event),
            other => Err(unexpected("run_accepted", &other)),
        }
    }

    pub async fn inference_inventory(&self) -> Result<InferenceInventoryEvent, ClientError> {
        match self
            .request(DaemonRequestKind::InferenceInventory(
                InferenceInventoryRequest::default(),
            ))
            .await?
        {
            DaemonEventKind::InferenceInventory(event) => Ok(event),
            other => Err(unexpected("inference_inventory", &other)),
        }
    }

    pub async fn inference_status(&self) -> Result<InferenceStatusEvent, ClientError> {
        match self
            .request(DaemonRequestKind::InferenceStatus(
                InferenceStatusRequest::default(),
            ))
            .await?
        {
            DaemonEventKind::InferenceStatus(event) => Ok(event),
            other => Err(unexpected("inference_status", &other)),
        }
    }

    /// Admits an interactive prompt through the daemon's shared application
    /// surface. The wire family stays `RunSubmit` so the accepted run can use
    /// the existing run-status and run-subscription streams.
    pub async fn submit_prompt(
        &self,
        request: RunSubmitRequest,
    ) -> Result<RunAcceptedEvent, ClientError> {
        match self.request(DaemonRequestKind::RunSubmit(request)).await? {
            DaemonEventKind::RunAccepted(event) => Ok(event),
            other => Err(unexpected("run_accepted", &other)),
        }
    }

    pub async fn run_status(&self, run_id: RunId) -> Result<RunStatusEvent, ClientError> {
        match self
            .request(DaemonRequestKind::RunStatus(RunStatusRequest { run_id }))
            .await?
        {
            DaemonEventKind::RunStatus(event) => Ok(*event),
            other => Err(unexpected("run_status", &other)),
        }
    }

    pub async fn cancel_run(&self, run_id: RunId) -> Result<RunStatusEvent, ClientError> {
        match self
            .request(DaemonRequestKind::RunCancel(RunCancelRequest { run_id }))
            .await?
        {
            DaemonEventKind::RunStatus(event) => Ok(*event),
            other => Err(unexpected("run_status", &other)),
        }
    }

    pub async fn run_tree(&self, run_id: RunId) -> Result<RunTreeEvent, ClientError> {
        match self
            .request(DaemonRequestKind::RunTree(RunTreeRequest { run_id }))
            .await?
        {
            DaemonEventKind::RunTree(event) => Ok(event),
            other => Err(unexpected("run_tree", &other)),
        }
    }

    pub async fn run_events(
        &self,
        request: RunEventsRequest,
    ) -> Result<RunEventsEvent, ClientError> {
        match self.request(DaemonRequestKind::RunEvents(request)).await? {
            DaemonEventKind::RunEvents(event) => Ok(event),
            other => Err(unexpected("run_events", &other)),
        }
    }

    pub async fn subscribe_run(
        &self,
        request: RunSubscribeRequest,
    ) -> Result<RunSubscription, ClientError> {
        let run_id = request.run_id.clone();
        let mut raw = self
            .stream(DaemonRequestKind::RunSubscribe(request))
            .await?;
        let started = match raw.recv().await? {
            DaemonEventKind::RunSubscriptionStarted(started) if started.run_id == run_id => started,
            other => return Err(unexpected("run_subscription_started", &other)),
        };
        Ok(RunSubscription {
            raw,
            run_id,
            last_sequence: started.after_sequence,
            started,
        })
    }

    pub async fn execution_list(
        &self,
        request: ExecutionListRequest,
    ) -> Result<ExecutionListEvent, ClientError> {
        match self
            .request(DaemonRequestKind::ExecutionList(request))
            .await?
        {
            DaemonEventKind::ExecutionList(event) => Ok(event),
            other => Err(unexpected("execution_list", &other)),
        }
    }

    pub async fn execution_status(
        &self,
        request: ExecutionStatusRequest,
    ) -> Result<ExecutionStatusEvent, ClientError> {
        match self
            .request(DaemonRequestKind::ExecutionStatus(request))
            .await?
        {
            DaemonEventKind::ExecutionStatus(event) => Ok(event),
            other => Err(unexpected("execution_status", &other)),
        }
    }

    pub async fn execution_read(
        &self,
        request: ExecutionReadRequest,
    ) -> Result<ExecutionReadEvent, ClientError> {
        match self
            .request(DaemonRequestKind::ExecutionRead(request))
            .await?
        {
            DaemonEventKind::ExecutionRead(event) => Ok(event),
            other => Err(unexpected("execution_read", &other)),
        }
    }

    pub async fn execution_kill(
        &self,
        request: ExecutionKillRequest,
    ) -> Result<ExecutionKillAcceptedEvent, ClientError> {
        match self
            .request(DaemonRequestKind::ExecutionKill(request))
            .await?
        {
            DaemonEventKind::ExecutionKillAccepted(event) => Ok(event),
            other => Err(unexpected("execution_kill_accepted", &other)),
        }
    }

    pub async fn attach_execution(
        &self,
        execution_id: agl_ids::ExecutionId,
        after_sequence: u64,
        writable: bool,
    ) -> Result<ExecutionAttachment, ClientError> {
        let request = ExecutionAttachRequest {
            execution_id: execution_id.clone(),
            after_sequence,
            writable,
        };
        let mut raw = self
            .stream(DaemonRequestKind::ExecutionAttach(request))
            .await?;
        let started = match raw.recv().await? {
            DaemonEventKind::ExecutionAttachmentStarted(started)
                if started.status.execution_id == execution_id
                    && started.writable == writable
                    && started.writable == started.writer_lease_id.is_some() =>
            {
                started
            }
            other => return Err(unexpected("execution_attachment_started", &other)),
        };
        if started.next_sequence != after_sequence {
            return Err(ClientError::SequenceGap {
                expected: after_sequence,
                actual: started.next_sequence,
            });
        }
        let heartbeat = started.heartbeat_interval_ms.map(|milliseconds| {
            let period = Duration::from_millis(milliseconds);
            let mut interval = tokio::time::interval_at(Instant::now() + period, period);
            interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
            interval
        });
        Ok(ExecutionAttachment {
            client: self.clone(),
            attachment_id: started.attachment_id.clone(),
            execution_id,
            last_sequence: started.next_sequence,
            started,
            raw,
            heartbeat,
            finished: false,
        })
    }

    pub async fn command_catalog(
        &self,
        request: CommandCatalogRequest,
    ) -> Result<CommandCatalogEvent, ClientError> {
        match self
            .request(DaemonRequestKind::CommandCatalog(request))
            .await?
        {
            DaemonEventKind::CommandCatalog(event) => Ok(event),
            other => Err(unexpected("command_catalog", &other)),
        }
    }

    pub async fn command_suggestions(
        &self,
        request: CommandSuggestionsRequest,
    ) -> Result<CommandSuggestionsEvent, ClientError> {
        match self
            .request(DaemonRequestKind::CommandSuggestions(request))
            .await?
        {
            DaemonEventKind::CommandSuggestions(event) => Ok(event),
            other => Err(unexpected("command_suggestions", &other)),
        }
    }

    pub async fn application_action(
        &self,
        request: ApplicationActionRequest,
    ) -> Result<ApplicationActionResultEvent, ClientError> {
        match self
            .request(DaemonRequestKind::ApplicationAction(request))
            .await?
        {
            DaemonEventKind::ApplicationActionResult(event) => Ok(event),
            other => Err(unexpected("application_action_result", &other)),
        }
    }

    pub async fn session_presentation(
        &self,
        request: SessionPresentationRequest,
    ) -> Result<SessionPresentationSnapshot, ClientError> {
        let session_id = request.session_id.clone();
        let daemon_instance_id = self.hello()?.daemon_instance_id;
        let mut raw = self
            .stream(DaemonRequestKind::SessionPresentation(request))
            .await?;
        let assembled = receive_snapshot_transfer(
            &mut raw,
            &session_id,
            &daemon_instance_id,
            ExpectedSnapshotPurpose::Requested,
            None,
        )
        .await?;
        raw.terminal = true;
        Ok(assembled.snapshot)
    }

    pub async fn subscribe_presentation(
        &self,
        request: SessionPresentationSubscribeRequest,
    ) -> Result<PresentationSubscription, ClientError> {
        let session_id = request.session_id.clone();
        let mut raw = self
            .stream(DaemonRequestKind::SessionPresentationSubscribe(request))
            .await?;
        let daemon_instance_id = self.hello()?.daemon_instance_id;
        let assembled = receive_snapshot_transfer(
            &mut raw,
            &session_id,
            &daemon_instance_id,
            ExpectedSnapshotPurpose::SubscriptionInitial,
            None,
        )
        .await?;
        let next_revision = assembled.snapshot.cursor.revision.saturating_add(1);
        Ok(PresentationSubscription {
            snapshot: assembled.snapshot,
            raw,
            next_revision,
            daemon_instance_id,
            finished: false,
        })
    }

    pub async fn ensure_human_terminal(
        &self,
        request: HumanTerminalEnsureRequest,
    ) -> Result<HumanTerminalEnsuredEvent, ClientError> {
        match self
            .request(DaemonRequestKind::HumanTerminalEnsure(request))
            .await?
        {
            DaemonEventKind::HumanTerminalEnsured(event) => Ok(event),
            other => Err(unexpected("human_terminal_ensured", &other)),
        }
    }

    pub async fn ensure_human_host_terminal(
        &self,
        request: HumanHostTerminalEnsureRequest,
    ) -> Result<HumanTerminalEnsuredEvent, ClientError> {
        match self
            .request(DaemonRequestKind::HumanHostTerminalEnsure(request))
            .await?
        {
            DaemonEventKind::HumanTerminalEnsured(event) => Ok(event),
            other => Err(unexpected("human_terminal_ensured", &other)),
        }
    }

    pub async fn submit_human_terminal_command(
        &self,
        request: HumanTerminalCommandSubmitRequest,
    ) -> Result<HumanTerminalCommandAcceptedEvent, ClientError> {
        match self
            .request(DaemonRequestKind::HumanTerminalCommandSubmit(request))
            .await?
        {
            DaemonEventKind::HumanTerminalCommandAccepted(event) => Ok(event),
            other => Err(unexpected("human_terminal_command_accepted", &other)),
        }
    }

    async fn request(&self, kind: DaemonRequestKind) -> Result<DaemonEventKind, ClientError> {
        let _permit = Arc::clone(&self.one_shot_slots)
            .acquire_owned()
            .await
            .map_err(|_| ClientError::ConnectionClosed)?;
        let request_id = RequestId::generate();
        let expected = Expected::for_request(&kind, false).ok_or(ClientError::InvalidRequest(
            "request family requires a stream handle".to_owned(),
        ))?;
        let (reply, response) = oneshot::channel();
        let request = DaemonRequest::new(request_id, kind);
        request
            .validate()
            .map_err(|error| ClientError::InvalidRequest(error.to_string()))?;
        self.sender
            .try_send(ConnectionCommand::Send {
                request,
                route: Some(Route::OneShot { expected, reply }),
            })
            .map_err(map_send_error)?;
        response.await.map_err(|_| ClientError::ConnectionClosed)?
    }

    async fn stream(&self, kind: DaemonRequestKind) -> Result<RawSubscription, ClientError> {
        let request_id = RequestId::generate();
        let expected = Expected::for_request(&kind, true).ok_or(ClientError::InvalidRequest(
            "request family does not produce a stream".to_owned(),
        ))?;
        let cancellation = expected
            .stream_cancellation()
            .expect("a stream response family must define cancellation");
        let capacity = if matches!(expected, Expected::ExecutionStream) {
            EXECUTION_ATTACHMENT_CAPACITY
        } else {
            SUBSCRIPTION_CAPACITY
        };
        let (events, receiver) = mpsc::channel(capacity);
        let (failure, failure_receiver) = watch::channel(None);
        let request = DaemonRequest::new(request_id.clone(), kind);
        request
            .validate()
            .map_err(|error| ClientError::InvalidRequest(error.to_string()))?;
        self.sender
            .try_send(ConnectionCommand::Send {
                request,
                route: Some(Route::Stream {
                    expected,
                    events,
                    failure,
                    last_sequence: 0,
                }),
            })
            .map_err(map_send_error)?;
        Ok(RawSubscription {
            request_id,
            events: receiver,
            failure: failure_receiver,
            sender: self.sender.clone(),
            cancellation,
            terminal: false,
            last_sequence: 0,
        })
    }
}

pub struct RunSubscription {
    raw: RawSubscription,
    run_id: RunId,
    last_sequence: u64,
    pub started: RunSubscriptionStartedEvent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunSubscriptionEvent {
    Event(Box<agl_events::SafeRuntimeEventEnvelope>),
    Finished(RunSubscriptionFinishedEvent),
}

impl RunSubscription {
    pub fn request_id(&self) -> &RequestId {
        &self.raw.request_id
    }

    pub async fn next(&mut self) -> Result<Option<RunSubscriptionEvent>, ClientError> {
        if self.raw.terminal {
            return Ok(None);
        }
        match self.raw.recv().await? {
            DaemonEventKind::RunEvent(event) if event.scope.run_id() == &self.run_id => {
                let expected = self.last_sequence.saturating_add(1);
                if event.sequence != expected {
                    return Err(ClientError::SequenceGap {
                        expected,
                        actual: event.sequence,
                    });
                }
                self.last_sequence = event.sequence;
                self.raw.last_sequence = event.sequence;
                Ok(Some(RunSubscriptionEvent::Event(event)))
            }
            DaemonEventKind::RunSubscriptionFinished(event) if event.run_id == self.run_id => {
                if event.last_sequence != self.last_sequence {
                    return Err(ClientError::SequenceGap {
                        expected: self.last_sequence,
                        actual: event.last_sequence,
                    });
                }
                self.raw.terminal = true;
                Ok(Some(RunSubscriptionEvent::Finished(event)))
            }
            other => Err(unexpected("run stream event", &other)),
        }
    }
}

pub struct PresentationSubscription {
    pub snapshot: SessionPresentationSnapshot,
    raw: RawSubscription,
    next_revision: u64,
    daemon_instance_id: agl_ids::DaemonInstanceId,
    finished: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PresentationSubscriptionEvent {
    SnapshotReplaced {
        event_id: agl_ids::EventId,
        snapshot: Box<SessionPresentationSnapshot>,
    },
    Event(Box<SessionPresentationEventEnvelope>),
    Finished(SessionPresentationSubscriptionFinishedEvent),
}

#[derive(Clone, Copy)]
enum ExpectedSnapshotPurpose {
    Requested,
    SubscriptionInitial,
    Replacement,
}

#[derive(Debug)]
struct AssembledPresentationSnapshot {
    snapshot: SessionPresentationSnapshot,
    purpose: SessionPresentationSnapshotTransferPurpose,
}

struct PresentationSnapshotAssembly {
    manifest: SessionPresentationSnapshotManifestEvent,
    next_chunk: u16,
    bytes: Vec<u8>,
}

struct PresentationSnapshotAssembler {
    expected_session_id: agl_ids::SessionId,
    expected_daemon_instance_id: agl_ids::DaemonInstanceId,
    expected_purpose: ExpectedSnapshotPurpose,
    assembly: Option<PresentationSnapshotAssembly>,
    complete: bool,
}

impl PresentationSnapshotAssembler {
    fn new(
        expected_session_id: &agl_ids::SessionId,
        expected_daemon_instance_id: &agl_ids::DaemonInstanceId,
        expected_purpose: ExpectedSnapshotPurpose,
    ) -> Self {
        Self {
            expected_session_id: expected_session_id.clone(),
            expected_daemon_instance_id: expected_daemon_instance_id.clone(),
            expected_purpose,
            assembly: None,
            complete: false,
        }
    }

    fn manifest(
        &mut self,
        manifest: SessionPresentationSnapshotManifestEvent,
    ) -> Result<(), ClientError> {
        if self.complete || self.assembly.is_some() {
            return Err(ClientError::SnapshotTransferInvalid(
                "presentation snapshot transfer sent more than one manifest",
            ));
        }
        manifest.validate().map_err(|_| {
            ClientError::SnapshotTransferInvalid(
                "presentation snapshot manifest failed bounded validation",
            )
        })?;
        self.validate_identity_and_purpose(&manifest.transfer)?;
        let capacity = usize::try_from(manifest.decoded_bytes).map_err(|_| {
            ClientError::SnapshotTransferInvalid(
                "presentation snapshot decoded byte count does not fit this client",
            )
        })?;
        self.assembly = Some(PresentationSnapshotAssembly {
            manifest,
            next_chunk: 0,
            bytes: Vec::with_capacity(capacity),
        });
        Ok(())
    }

    fn chunk(&mut self, chunk: SessionPresentationSnapshotChunkEvent) -> Result<(), ClientError> {
        chunk.validate().map_err(|_| {
            ClientError::SnapshotTransferInvalid(
                "presentation snapshot chunk failed bounded validation",
            )
        })?;
        let assembly = self
            .assembly
            .as_mut()
            .ok_or(ClientError::SnapshotTransferInvalid(
                "presentation snapshot chunk arrived before its manifest",
            ))?;
        if chunk.transfer != assembly.manifest.transfer
            || chunk.chunk_count != assembly.manifest.chunk_count
        {
            return Err(ClientError::IdentityMismatch(
                "presentation snapshot chunk belongs to another transfer",
            ));
        }
        if chunk.chunk_index != assembly.next_chunk {
            return Err(ClientError::SnapshotChunkOutOfOrder {
                expected: assembly.next_chunk,
                actual: chunk.chunk_index,
            });
        }
        let bytes = chunk
            .bytes
            .decode(MAX_PRESENTATION_SNAPSHOT_CHUNK_BYTES)
            .map_err(|_| {
                ClientError::SnapshotTransferInvalid(
                    "presentation snapshot chunk could not be decoded",
                )
            })?;
        let decoded_bytes = usize::try_from(assembly.manifest.decoded_bytes).map_err(|_| {
            ClientError::SnapshotTransferInvalid(
                "presentation snapshot decoded byte count does not fit this client",
            )
        })?;
        if assembly.bytes.len().saturating_add(bytes.len()) > decoded_bytes {
            return Err(ClientError::SnapshotTransferInvalid(
                "presentation snapshot chunks exceed the manifest byte count",
            ));
        }
        assembly.bytes.extend_from_slice(&bytes);
        assembly.next_chunk = assembly.next_chunk.saturating_add(1);
        Ok(())
    }

    fn finished(
        &mut self,
        finished: SessionPresentationSnapshotFinishedEvent,
    ) -> Result<AssembledPresentationSnapshot, ClientError> {
        if self.complete {
            return Err(ClientError::SnapshotTransferInvalid(
                "presentation snapshot transfer finished more than once",
            ));
        }
        finished.validate().map_err(|_| {
            ClientError::SnapshotTransferInvalid(
                "presentation snapshot finish marker failed bounded validation",
            )
        })?;
        let assembly = self
            .assembly
            .take()
            .ok_or(ClientError::SnapshotTransferInvalid(
                "presentation snapshot finish marker arrived before its manifest",
            ))?;
        if finished.transfer != assembly.manifest.transfer
            || finished.item_count != assembly.manifest.item_count
            || finished.decoded_bytes != assembly.manifest.decoded_bytes
            || finished.chunk_count != assembly.manifest.chunk_count
            || finished.digest != assembly.manifest.digest
        {
            return Err(ClientError::IdentityMismatch(
                "presentation snapshot finish marker belongs to another transfer",
            ));
        }
        if assembly.next_chunk != assembly.manifest.chunk_count {
            return Err(ClientError::SnapshotTransferInvalid(
                "presentation snapshot transfer finished before every chunk arrived",
            ));
        }
        if assembly.bytes.len()
            != usize::try_from(assembly.manifest.decoded_bytes).map_err(|_| {
                ClientError::SnapshotTransferInvalid(
                    "presentation snapshot decoded byte count does not fit this client",
                )
            })?
        {
            return Err(ClientError::SnapshotTransferInvalid(
                "presentation snapshot decoded byte count does not match its manifest",
            ));
        }
        if PresentationSnapshotDigest::from_bytes(&assembly.bytes) != assembly.manifest.digest {
            return Err(ClientError::SnapshotDigestMismatch);
        }
        let snapshot: SessionPresentationSnapshot = serde_json::from_slice(&assembly.bytes)
            .map_err(|_| {
                ClientError::SnapshotTransferInvalid(
                    "presentation snapshot transfer is not a typed snapshot",
                )
            })?;
        let canonical = snapshot.canonical_json_bytes().map_err(|_| {
            ClientError::SnapshotTransferInvalid(
                "presentation snapshot transfer failed snapshot validation",
            )
        })?;
        if canonical != assembly.bytes {
            return Err(ClientError::SnapshotTransferInvalid(
                "presentation snapshot transfer is not canonical JSON",
            ));
        }
        if snapshot.session_id != assembly.manifest.transfer.session_id
            || snapshot.cursor != assembly.manifest.transfer.cursor
            || snapshot.items.len()
                != usize::try_from(assembly.manifest.item_count).unwrap_or(usize::MAX)
        {
            return Err(ClientError::IdentityMismatch(
                "presentation snapshot contents do not match the transfer manifest",
            ));
        }
        self.complete = true;
        Ok(AssembledPresentationSnapshot {
            snapshot,
            purpose: assembly.manifest.transfer.purpose,
        })
    }

    fn validate_identity_and_purpose(
        &self,
        transfer: &SessionPresentationSnapshotTransferIdentity,
    ) -> Result<(), ClientError> {
        if transfer.session_id != self.expected_session_id {
            return Err(ClientError::IdentityMismatch(
                "presentation snapshot transfer belongs to another session",
            ));
        }
        if transfer.cursor.daemon_instance_id != self.expected_daemon_instance_id {
            return Err(ClientError::DaemonInstanceChanged);
        }
        let purpose_matches = matches!(
            (self.expected_purpose, &transfer.purpose),
            (
                ExpectedSnapshotPurpose::Requested,
                SessionPresentationSnapshotTransferPurpose::Requested
            ) | (
                ExpectedSnapshotPurpose::SubscriptionInitial,
                SessionPresentationSnapshotTransferPurpose::SubscriptionInitial
            ) | (
                ExpectedSnapshotPurpose::Replacement,
                SessionPresentationSnapshotTransferPurpose::Replacement { .. }
            )
        );
        if !purpose_matches {
            return Err(ClientError::IdentityMismatch(
                "presentation snapshot transfer has the wrong purpose",
            ));
        }
        Ok(())
    }
}

async fn receive_snapshot_transfer(
    raw: &mut RawSubscription,
    expected_session_id: &agl_ids::SessionId,
    expected_daemon_instance_id: &agl_ids::DaemonInstanceId,
    expected_purpose: ExpectedSnapshotPurpose,
    first_manifest: Option<SessionPresentationSnapshotManifestEvent>,
) -> Result<AssembledPresentationSnapshot, ClientError> {
    let mut assembler = PresentationSnapshotAssembler::new(
        expected_session_id,
        expected_daemon_instance_id,
        expected_purpose,
    );
    if let Some(manifest) = first_manifest {
        assembler.manifest(manifest)?;
    }
    loop {
        match raw.recv().await? {
            DaemonEventKind::SessionPresentationSnapshotManifest(manifest) => {
                assembler.manifest(manifest)?;
            }
            DaemonEventKind::SessionPresentationSnapshotChunk(chunk) => {
                assembler.chunk(chunk)?;
            }
            DaemonEventKind::SessionPresentationSnapshotFinished(finished) => {
                return assembler.finished(finished);
            }
            other => return Err(unexpected("presentation snapshot transfer frame", &other)),
        }
    }
}

impl PresentationSubscription {
    pub fn request_id(&self) -> &RequestId {
        &self.raw.request_id
    }

    pub async fn next(&mut self) -> Result<Option<PresentationSubscriptionEvent>, ClientError> {
        if self.finished {
            return Ok(None);
        }
        match self.raw.recv().await? {
            DaemonEventKind::SessionPresentationSnapshotManifest(manifest) => {
                if manifest.transfer.cursor.revision != self.next_revision {
                    return Err(ClientError::SequenceGap {
                        expected: self.next_revision,
                        actual: manifest.transfer.cursor.revision,
                    });
                }
                let assembled = receive_snapshot_transfer(
                    &mut self.raw,
                    &self.snapshot.session_id,
                    &self.daemon_instance_id,
                    ExpectedSnapshotPurpose::Replacement,
                    Some(manifest),
                )
                .await?;
                let SessionPresentationSnapshotTransferPurpose::Replacement { event_id } =
                    assembled.purpose
                else {
                    return Err(ClientError::IdentityMismatch(
                        "presentation snapshot replacement has the wrong purpose",
                    ));
                };
                self.next_revision = self.next_revision.saturating_add(1);
                self.raw.last_sequence = assembled.snapshot.cursor.revision;
                self.snapshot = assembled.snapshot.clone();
                Ok(Some(PresentationSubscriptionEvent::SnapshotReplaced {
                    event_id,
                    snapshot: Box::new(assembled.snapshot),
                }))
            }
            DaemonEventKind::SessionPresentationSnapshotChunk(_)
            | DaemonEventKind::SessionPresentationSnapshotFinished(_) => {
                Err(ClientError::SnapshotTransferInvalid(
                    "presentation snapshot transfer frame arrived before its manifest",
                ))
            }
            DaemonEventKind::SessionPresentationEvent(event) => {
                if event.cursor.daemon_instance_id != self.daemon_instance_id {
                    return Err(ClientError::DaemonInstanceChanged);
                }
                if event.cursor.revision != self.next_revision {
                    return Err(ClientError::SequenceGap {
                        expected: self.next_revision,
                        actual: event.cursor.revision,
                    });
                }
                self.next_revision = self.next_revision.saturating_add(1);
                self.raw.last_sequence = event.cursor.revision;
                Ok(Some(PresentationSubscriptionEvent::Event(event)))
            }
            DaemonEventKind::SessionPresentationSubscriptionFinished(event) => {
                if event.last_delivered_cursor.daemon_instance_id != self.daemon_instance_id {
                    return Err(ClientError::DaemonInstanceChanged);
                }
                let expected = self.next_revision.saturating_sub(1);
                if event.last_delivered_cursor.revision != expected {
                    return Err(ClientError::SequenceGap {
                        expected,
                        actual: event.last_delivered_cursor.revision,
                    });
                }
                self.finished = true;
                self.raw.terminal = true;
                Ok(Some(PresentationSubscriptionEvent::Finished(event)))
            }
            other => Err(unexpected("session presentation stream event", &other)),
        }
    }
}

pub struct ExecutionAttachment {
    client: AgentLibreClient,
    attachment_id: RequestId,
    execution_id: agl_ids::ExecutionId,
    last_sequence: u64,
    pub started: ExecutionAttachmentStartedEvent,
    raw: RawSubscription,
    heartbeat: Option<Interval>,
    finished: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionAttachmentEvent {
    Output(ExecutionOutputEvent),
    Finished(ExecutionAttachmentFinishedEvent),
}

impl ExecutionAttachment {
    pub fn attachment_id(&self) -> &RequestId {
        &self.attachment_id
    }

    pub fn writer_lease_id(&self) -> Option<&agl_ids::WriterLeaseId> {
        self.started.writer_lease_id.as_ref()
    }

    pub async fn input(
        &self,
        bytes: ProcessBytes,
        eof: bool,
    ) -> Result<ExecutionInputAcceptedEvent, ClientError> {
        match self
            .client
            .request(DaemonRequestKind::ExecutionInput(ExecutionInputRequest {
                attachment_id: self.attachment_id.clone(),
                bytes,
                eof,
            }))
            .await?
        {
            DaemonEventKind::ExecutionInputAccepted(event)
                if event.attachment_id == self.attachment_id =>
            {
                Ok(event)
            }
            other => Err(unexpected("execution_input_accepted", &other)),
        }
    }

    pub async fn resize(
        &self,
        columns: u16,
        rows: u16,
    ) -> Result<ExecutionResizeAcceptedEvent, ClientError> {
        match self
            .client
            .request(DaemonRequestKind::ExecutionResize(ExecutionResizeRequest {
                attachment_id: self.attachment_id.clone(),
                columns,
                rows,
            }))
            .await?
        {
            DaemonEventKind::ExecutionResizeAccepted(event)
                if event.attachment_id == self.attachment_id =>
            {
                Ok(event)
            }
            other => Err(unexpected("execution_resize_accepted", &other)),
        }
    }

    pub async fn detach(&self) -> Result<ExecutionDetachAcceptedEvent, ClientError> {
        match self
            .client
            .request(DaemonRequestKind::ExecutionDetach(ExecutionDetachRequest {
                attachment_id: self.attachment_id.clone(),
            }))
            .await?
        {
            DaemonEventKind::ExecutionDetachAccepted(event)
                if event.attachment_id == self.attachment_id =>
            {
                Ok(event)
            }
            other => Err(unexpected("execution_detach_accepted", &other)),
        }
    }

    pub async fn renew_lease(&self) -> Result<ExecutionLeaseRenewedEvent, ClientError> {
        match self
            .client
            .request(DaemonRequestKind::ExecutionLeaseRenew(
                ExecutionLeaseRenewRequest {
                    attachment_id: self.attachment_id.clone(),
                },
            ))
            .await?
        {
            DaemonEventKind::ExecutionLeaseRenewed(event)
                if event.attachment_id == self.attachment_id =>
            {
                Ok(event)
            }
            other => Err(unexpected("execution_lease_renewed", &other)),
        }
    }

    pub async fn next(&mut self) -> Result<Option<ExecutionAttachmentEvent>, ClientError> {
        if self.finished {
            return Ok(None);
        }
        loop {
            let kind = if let Some(heartbeat) = self.heartbeat.as_mut() {
                tokio::select! {
                    _ = heartbeat.tick() => {
                        self.renew_lease().await?;
                        continue;
                    }
                    event = self.raw.recv() => event?,
                }
            } else {
                self.raw.recv().await?
            };
            match kind {
                DaemonEventKind::ExecutionOutput(event)
                    if event.attachment_id == self.attachment_id
                        && event.execution_id == self.execution_id =>
                {
                    let expected = self.last_sequence.saturating_add(1);
                    // Execution cursors cover the complete durable execution
                    // event sequence. Lifecycle, resize, and other metadata
                    // can therefore occupy sequence values that have no PTY
                    // output chunk. Output must be strictly monotonic, but it
                    // is not necessarily contiguous.
                    if event.chunk.sequence <= self.last_sequence {
                        return Err(ClientError::SequenceGap {
                            expected,
                            actual: event.chunk.sequence,
                        });
                    }
                    self.last_sequence = event.chunk.sequence;
                    self.raw.last_sequence = event.chunk.sequence;
                    return Ok(Some(ExecutionAttachmentEvent::Output(event)));
                }
                DaemonEventKind::ExecutionAttachmentFinished(event)
                    if event.attachment_id == self.attachment_id
                        && event.execution_id == self.execution_id =>
                {
                    if event.last_delivered_sequence < self.last_sequence {
                        return Err(ClientError::SequenceGap {
                            expected: self.last_sequence,
                            actual: event.last_delivered_sequence,
                        });
                    }
                    self.last_sequence = event.last_delivered_sequence;
                    self.raw.last_sequence = event.last_delivered_sequence;
                    self.finished = true;
                    self.raw.terminal = true;
                    return Ok(Some(ExecutionAttachmentEvent::Finished(event)));
                }
                other => return Err(unexpected("execution attachment stream event", &other)),
            }
        }
    }
}

struct RawSubscription {
    request_id: RequestId,
    events: mpsc::Receiver<DaemonEventKind>,
    failure: watch::Receiver<Option<ClientError>>,
    sender: mpsc::Sender<ConnectionCommand>,
    cancellation: StreamCancellation,
    terminal: bool,
    last_sequence: u64,
}

impl RawSubscription {
    async fn recv(&mut self) -> Result<DaemonEventKind, ClientError> {
        if let Some(error) = self.failure.borrow().clone() {
            return Err(error);
        }
        tokio::select! {
            biased;
            event = self.events.recv() => event.ok_or_else(|| {
                self.failure.borrow().clone().unwrap_or(ClientError::ConnectionClosed)
            }),
            changed = self.failure.changed() => {
                changed.map_err(|_| ClientError::ConnectionClosed)?;
                Err(self.failure.borrow().clone().unwrap_or(ClientError::ConnectionClosed))
            },
        }
    }
}

impl Drop for RawSubscription {
    fn drop(&mut self) {
        if self.terminal {
            return;
        }
        let _ = self.sender.try_send(ConnectionCommand::Send {
            request: DaemonRequest::new(
                RequestId::generate(),
                self.cancellation.request(self.request_id.clone()),
            ),
            route: None,
        });
    }
}

enum ConnectionCommand {
    Send {
        request: DaemonRequest,
        route: Option<Route>,
    },
}

#[derive(Clone, Copy)]
enum StreamCancellation {
    Subscription,
    ExecutionAttachment,
}

impl StreamCancellation {
    fn request(self, stream_request_id: RequestId) -> DaemonRequestKind {
        match self {
            Self::Subscription => {
                DaemonRequestKind::SubscriptionCancel(SubscriptionCancelRequest {
                    subscription_request_id: stream_request_id,
                })
            }
            Self::ExecutionAttachment => {
                DaemonRequestKind::ExecutionDetach(ExecutionDetachRequest {
                    attachment_id: stream_request_id,
                })
            }
        }
    }
}

fn map_send_error(error: mpsc::error::TrySendError<ConnectionCommand>) -> ClientError {
    match error {
        mpsc::error::TrySendError::Full(_) => ClientError::InputBackpressure,
        mpsc::error::TrySendError::Closed(_) => ClientError::ConnectionClosed,
    }
}

enum Route {
    OneShot {
        expected: Expected,
        reply: oneshot::Sender<Result<DaemonEventKind, ClientError>>,
    },
    Stream {
        expected: Expected,
        events: mpsc::Sender<DaemonEventKind>,
        failure: watch::Sender<Option<ClientError>>,
        last_sequence: u64,
    },
}

#[derive(Clone, Copy)]
enum Expected {
    Hello,
    SessionOpened,
    SessionStatus,
    SessionFinished,
    SessionList,
    SessionTranscript,
    RunAccepted,
    RunStatus,
    RunTree,
    RunEvents,
    ExecutionList,
    ExecutionStatus,
    ExecutionRead,
    ExecutionInput,
    ExecutionResize,
    ExecutionDetach,
    ExecutionKill,
    ExecutionLeaseRenew,
    CommandCatalog,
    CommandSuggestions,
    ApplicationAction,
    PresentationSnapshotStream,
    SubscriptionCancelled,
    HumanTerminalEnsured,
    HumanTerminalCommandAccepted,
    InferenceInventory,
    InferenceStatus,
    RunStream,
    PresentationStream,
    ExecutionStream,
}

impl Expected {
    fn for_request(kind: &DaemonRequestKind, stream: bool) -> Option<Self> {
        Some(match kind {
            DaemonRequestKind::Hello(_) if !stream => Self::Hello,
            DaemonRequestKind::SessionOpen(_) | DaemonRequestKind::SetupSmokeSessionOpen(_)
                if !stream =>
            {
                Self::SessionOpened
            }
            DaemonRequestKind::SessionClear(_) | DaemonRequestKind::SessionStatus(_) if !stream => {
                Self::SessionStatus
            }
            DaemonRequestKind::SessionFinish(_) if !stream => Self::SessionFinished,
            DaemonRequestKind::SessionList(_) if !stream => Self::SessionList,
            DaemonRequestKind::SessionTranscript(_) if !stream => Self::SessionTranscript,
            DaemonRequestKind::RunSubmit(_) if !stream => Self::RunAccepted,
            DaemonRequestKind::RunStatus(_) | DaemonRequestKind::RunCancel(_) if !stream => {
                Self::RunStatus
            }
            DaemonRequestKind::RunTree(_) if !stream => Self::RunTree,
            DaemonRequestKind::RunEvents(_) if !stream => Self::RunEvents,
            DaemonRequestKind::RunSubscribe(_) if stream => Self::RunStream,
            DaemonRequestKind::InferenceInventory(_) if !stream => Self::InferenceInventory,
            DaemonRequestKind::InferenceStatus(_) if !stream => Self::InferenceStatus,
            DaemonRequestKind::ExecutionList(_) if !stream => Self::ExecutionList,
            DaemonRequestKind::ExecutionStatus(_) if !stream => Self::ExecutionStatus,
            DaemonRequestKind::ExecutionRead(_) if !stream => Self::ExecutionRead,
            DaemonRequestKind::ExecutionInput(_) if !stream => Self::ExecutionInput,
            DaemonRequestKind::ExecutionResize(_) if !stream => Self::ExecutionResize,
            DaemonRequestKind::ExecutionDetach(_) if !stream => Self::ExecutionDetach,
            DaemonRequestKind::ExecutionKill(_) if !stream => Self::ExecutionKill,
            DaemonRequestKind::ExecutionLeaseRenew(_) if !stream => Self::ExecutionLeaseRenew,
            DaemonRequestKind::ExecutionAttach(_) if stream => Self::ExecutionStream,
            DaemonRequestKind::CommandCatalog(_) if !stream => Self::CommandCatalog,
            DaemonRequestKind::CommandSuggestions(_) if !stream => Self::CommandSuggestions,
            DaemonRequestKind::ApplicationAction(_) if !stream => Self::ApplicationAction,
            DaemonRequestKind::SessionPresentation(_) if stream => Self::PresentationSnapshotStream,
            DaemonRequestKind::SessionPresentationSubscribe(_) if stream => {
                Self::PresentationStream
            }
            DaemonRequestKind::SubscriptionCancel(_) if !stream => Self::SubscriptionCancelled,
            DaemonRequestKind::HumanTerminalEnsure(_) if !stream => Self::HumanTerminalEnsured,
            DaemonRequestKind::HumanHostTerminalEnsure(_) if !stream => Self::HumanTerminalEnsured,
            DaemonRequestKind::HumanTerminalCommandSubmit(_) if !stream => {
                Self::HumanTerminalCommandAccepted
            }
            _ => return None,
        })
    }

    fn accepts(self, event: &DaemonEventKind) -> bool {
        matches!(event, DaemonEventKind::Error(_))
            || match self {
                Self::Hello => matches!(event, DaemonEventKind::Hello(_)),
                Self::SessionOpened => matches!(event, DaemonEventKind::SessionOpened(_)),
                Self::SessionStatus => matches!(event, DaemonEventKind::SessionStatus(_)),
                Self::SessionFinished => matches!(event, DaemonEventKind::SessionFinished(_)),
                Self::SessionList => matches!(event, DaemonEventKind::SessionList(_)),
                Self::SessionTranscript => matches!(event, DaemonEventKind::SessionTranscript(_)),
                Self::RunAccepted => matches!(event, DaemonEventKind::RunAccepted(_)),
                Self::RunStatus => matches!(event, DaemonEventKind::RunStatus(_)),
                Self::RunTree => matches!(event, DaemonEventKind::RunTree(_)),
                Self::RunEvents => matches!(event, DaemonEventKind::RunEvents(_)),
                Self::InferenceInventory => {
                    matches!(event, DaemonEventKind::InferenceInventory(_))
                }
                Self::InferenceStatus => matches!(event, DaemonEventKind::InferenceStatus(_)),
                Self::ExecutionList => matches!(event, DaemonEventKind::ExecutionList(_)),
                Self::ExecutionStatus => matches!(event, DaemonEventKind::ExecutionStatus(_)),
                Self::ExecutionRead => matches!(event, DaemonEventKind::ExecutionRead(_)),
                Self::ExecutionInput => matches!(event, DaemonEventKind::ExecutionInputAccepted(_)),
                Self::ExecutionResize => {
                    matches!(event, DaemonEventKind::ExecutionResizeAccepted(_))
                }
                Self::ExecutionDetach => {
                    matches!(event, DaemonEventKind::ExecutionDetachAccepted(_))
                }
                Self::ExecutionKill => matches!(event, DaemonEventKind::ExecutionKillAccepted(_)),
                Self::ExecutionLeaseRenew => {
                    matches!(event, DaemonEventKind::ExecutionLeaseRenewed(_))
                }
                Self::CommandCatalog => matches!(event, DaemonEventKind::CommandCatalog(_)),
                Self::CommandSuggestions => matches!(event, DaemonEventKind::CommandSuggestions(_)),
                Self::ApplicationAction => {
                    matches!(event, DaemonEventKind::ApplicationActionResult(_))
                }
                Self::SubscriptionCancelled => {
                    matches!(event, DaemonEventKind::SubscriptionCancelled(_))
                }
                Self::HumanTerminalEnsured => {
                    matches!(event, DaemonEventKind::HumanTerminalEnsured(_))
                }
                Self::HumanTerminalCommandAccepted => {
                    matches!(event, DaemonEventKind::HumanTerminalCommandAccepted(_))
                }
                Self::RunStream => matches!(
                    event,
                    DaemonEventKind::RunSubscriptionStarted(_)
                        | DaemonEventKind::RunEvent(_)
                        | DaemonEventKind::RunSubscriptionFinished(_)
                ),
                Self::PresentationSnapshotStream => matches!(
                    event,
                    DaemonEventKind::SessionPresentationSnapshotManifest(_)
                        | DaemonEventKind::SessionPresentationSnapshotChunk(_)
                        | DaemonEventKind::SessionPresentationSnapshotFinished(_)
                ),
                Self::PresentationStream => matches!(
                    event,
                    DaemonEventKind::SessionPresentationSnapshotManifest(_)
                        | DaemonEventKind::SessionPresentationSnapshotChunk(_)
                        | DaemonEventKind::SessionPresentationSnapshotFinished(_)
                        | DaemonEventKind::SessionPresentationEvent(_)
                        | DaemonEventKind::SessionPresentationSubscriptionFinished(_)
                ),
                Self::ExecutionStream => matches!(
                    event,
                    DaemonEventKind::ExecutionAttachmentStarted(_)
                        | DaemonEventKind::ExecutionOutput(_)
                        | DaemonEventKind::ExecutionAttachmentFinished(_)
                ),
            }
    }

    fn is_terminal(self, event: &DaemonEventKind) -> bool {
        matches!(event, DaemonEventKind::Error(_))
            || match self {
                Self::RunStream => matches!(event, DaemonEventKind::RunSubscriptionFinished(_)),
                Self::PresentationSnapshotStream => matches!(
                    event,
                    DaemonEventKind::SessionPresentationSnapshotFinished(_)
                ),
                Self::PresentationStream => matches!(
                    event,
                    DaemonEventKind::SessionPresentationSubscriptionFinished(_)
                ),
                Self::ExecutionStream => {
                    matches!(event, DaemonEventKind::ExecutionAttachmentFinished(_))
                }
                _ => true,
            }
    }

    fn stream_cancellation(self) -> Option<StreamCancellation> {
        match self {
            Self::RunStream | Self::PresentationSnapshotStream | Self::PresentationStream => {
                Some(StreamCancellation::Subscription)
            }
            Self::ExecutionStream => Some(StreamCancellation::ExecutionAttachment),
            _ => None,
        }
    }
}

async fn connection_task(
    stream: UnixStream,
    mut commands: mpsc::Receiver<ConnectionCommand>,
    command_sender: mpsc::WeakSender<ConnectionCommand>,
) {
    let mut framed = Framed::new(
        stream,
        LinesCodec::new_with_max_length(MAX_JSONL_FRAME_BYTES),
    );
    let mut routes = BTreeMap::<RequestId, Route>::new();
    let mut ignored_terminals = BTreeSet::<RequestId>::new();
    let mut abandoned_streams = BTreeMap::<RequestId, Expected>::new();
    let failure = loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(ConnectionCommand::Send { request, route }) = command else {
                    break ClientError::ConnectionClosed;
                };
                if let Some(route) = route {
                    if routes.len() >= CONNECTION_ROUTE_CAPACITY {
                        fail_route(route, ClientError::InputBackpressure);
                        continue;
                    }
                    if routes.insert(request.request_id.clone(), route).is_some() {
                        break ClientError::IdentityMismatch("duplicate outstanding request ID");
                    }
                } else {
                    if ignored_terminals.len() >= IGNORED_TERMINAL_CAPACITY {
                        break ClientError::InputBackpressure;
                    }
                    ignored_terminals.insert(request.request_id.clone());
                }
                let line = match serde_json::to_string(&request) {
                    Ok(line) => line,
                    Err(_) => break ClientError::InvalidProtocolFrame,
                };
                if line.len() > MAX_JSONL_FRAME_BYTES {
                    break ClientError::FrameTooLarge;
                }
                if let Err(error) = framed.send(line).await {
                    break ClientError::Io(error.to_string());
                }
            }
            line = framed.next() => {
                let Some(line) = line else {
                    break ClientError::ConnectionClosed;
                };
                let line = match line {
                    Ok(line) => line,
                    Err(error) => break ClientError::Io(error.to_string()),
                };
                let event: DaemonEvent = match serde_json::from_str(&line) {
                    Ok(event) => event,
                    Err(_) => break ClientError::InvalidProtocolFrame,
                };
                if event.schema != EVENT_SCHEMA {
                    break ClientError::SchemaMismatch { expected: EVENT_SCHEMA };
                }
                let Some(request_id) = event.request_id.clone() else {
                    break ClientError::IdentityMismatch("daemon event has no request identity");
                };
                let Some(route) = routes.remove(&request_id) else {
                    if ignored_terminals.remove(&request_id) {
                        continue;
                    }
                    if let Some(expected) = abandoned_streams.get(&request_id).copied() {
                        if expected.is_terminal(&event.kind) {
                            abandoned_streams.remove(&request_id);
                        }
                        continue;
                    }
                    break ClientError::RequestMismatch { expected: request_id, actual: event.request_id };
                };
                if let Some(abandoned) = dispatch_route(&mut routes, request_id, route, event.kind) {
                    if abandoned_streams.len() >= ABANDONED_STREAM_CAPACITY {
                        break ClientError::InputBackpressure;
                    }
                    abandoned_streams.insert(abandoned.request_id.clone(), abandoned.expected);
                    best_effort_cancel_stream(&command_sender, abandoned);
                }
            }
        }
    };
    fail_routes(routes, failure);
}

struct AbandonedStream {
    request_id: RequestId,
    expected: Expected,
}

fn best_effort_cancel_stream(
    command_sender: &mpsc::WeakSender<ConnectionCommand>,
    abandoned: AbandonedStream,
) {
    let Some(command_sender) = command_sender.upgrade() else {
        return;
    };
    let cancellation = abandoned
        .expected
        .stream_cancellation()
        .expect("an abandoned stream must define cancellation");
    let _ = command_sender.try_send(ConnectionCommand::Send {
        request: DaemonRequest::new(
            RequestId::generate(),
            cancellation.request(abandoned.request_id),
        ),
        route: None,
    });
}

fn dispatch_route(
    routes: &mut BTreeMap<RequestId, Route>,
    request_id: RequestId,
    route: Route,
    event: DaemonEventKind,
) -> Option<AbandonedStream> {
    match route {
        Route::OneShot { expected, reply } => {
            let result = route_result(expected, event);
            let _ = reply.send(result);
            None
        }
        Route::Stream {
            expected,
            events,
            failure,
            last_sequence,
        } => {
            if !expected.accepts(&event) {
                let _ = failure.send(Some(unexpected("registered response family", &event)));
                return None;
            }
            if let DaemonEventKind::Error(error) = event {
                let _ = failure.send(Some(protocol_error(error)));
                return None;
            }
            let terminal = expected.is_terminal(&event);
            let delivered_sequence = stream_sequence(&event).unwrap_or(last_sequence);
            match events.try_send(event) {
                Ok(()) if !terminal => {
                    routes.insert(
                        request_id,
                        Route::Stream {
                            expected,
                            events,
                            failure,
                            last_sequence: delivered_sequence,
                        },
                    );
                    None
                }
                Ok(()) => None,
                Err(_) => {
                    let _ = failure.send(Some(ClientError::SubscriptionLagged {
                        request_id: request_id.clone(),
                        last_sequence,
                    }));
                    Some(AbandonedStream {
                        request_id,
                        expected,
                    })
                }
            }
        }
    }
}

fn stream_sequence(event: &DaemonEventKind) -> Option<u64> {
    match event {
        DaemonEventKind::RunSubscriptionStarted(event) => Some(event.after_sequence),
        DaemonEventKind::RunEvent(event) => Some(event.sequence),
        DaemonEventKind::RunSubscriptionFinished(event) => Some(event.last_sequence),
        DaemonEventKind::SessionPresentationSnapshotManifest(event) => {
            Some(event.transfer.cursor.revision)
        }
        DaemonEventKind::SessionPresentationSnapshotChunk(event) => {
            Some(event.transfer.cursor.revision)
        }
        DaemonEventKind::SessionPresentationSnapshotFinished(event) => {
            Some(event.transfer.cursor.revision)
        }
        DaemonEventKind::SessionPresentationEvent(event) => Some(event.cursor.revision),
        DaemonEventKind::SessionPresentationSubscriptionFinished(event) => {
            Some(event.last_delivered_cursor.revision)
        }
        DaemonEventKind::ExecutionAttachmentStarted(event) => Some(event.next_sequence),
        DaemonEventKind::ExecutionOutput(event) => Some(event.chunk.sequence),
        DaemonEventKind::ExecutionAttachmentFinished(event) => Some(event.last_delivered_sequence),
        _ => None,
    }
}

fn route_result(
    expected: Expected,
    event: DaemonEventKind,
) -> Result<DaemonEventKind, ClientError> {
    if let DaemonEventKind::Error(error) = event {
        return Err(protocol_error(error));
    }
    if !expected.accepts(&event) {
        return Err(unexpected("registered response family", &event));
    }
    Ok(event)
}

fn fail_routes(routes: BTreeMap<RequestId, Route>, error: ClientError) {
    for route in routes.into_values() {
        fail_route(route, error.clone());
    }
}

fn fail_route(route: Route, error: ClientError) {
    match route {
        Route::OneShot { reply, .. } => {
            let _ = reply.send(Err(error));
        }
        Route::Stream { failure, .. } => {
            let _ = failure.send(Some(error));
        }
    }
}

fn protocol_error(error: ProtocolError) -> ClientError {
    ClientError::Protocol {
        code: error.code,
        retryable: error.retryable,
    }
}

fn unexpected(expected: &'static str, actual: &DaemonEventKind) -> ClientError {
    ClientError::UnexpectedEvent {
        expected,
        actual: event_name(actual),
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
        DaemonEventKind::RunTree(_) => "run_tree",
        DaemonEventKind::RunEvents(_) => "run_events",
        DaemonEventKind::RunSubscriptionStarted(_) => "run_subscription_started",
        DaemonEventKind::RunEvent(_) => "run_event",
        DaemonEventKind::RunSubscriptionFinished(_) => "run_subscription_finished",
        DaemonEventKind::InferenceInventory(_) => "inference_inventory",
        DaemonEventKind::InferenceStatus(_) => "inference_status",
        DaemonEventKind::CommandCatalog(_) => "command_catalog",
        DaemonEventKind::CommandSuggestions(_) => "command_suggestions",
        DaemonEventKind::ApplicationActionResult(_) => "application_action_result",
        DaemonEventKind::SessionPresentationSnapshotManifest(_) => {
            "session_presentation_snapshot_manifest"
        }
        DaemonEventKind::SessionPresentationSnapshotChunk(_) => {
            "session_presentation_snapshot_chunk"
        }
        DaemonEventKind::SessionPresentationSnapshotFinished(_) => {
            "session_presentation_snapshot_finished"
        }
        DaemonEventKind::SessionPresentationEvent(_) => "session_presentation_event",
        DaemonEventKind::SessionPresentationSubscriptionFinished(_) => {
            "session_presentation_subscription_finished"
        }
        DaemonEventKind::SubscriptionCancelled(_) => "subscription_cancelled",
        DaemonEventKind::HumanTerminalEnsured(_) => "human_terminal_ensured",
        DaemonEventKind::HumanTerminalCommandAccepted(_) => "human_terminal_command_accepted",
        DaemonEventKind::ExecutionList(_) => "execution_list",
        DaemonEventKind::ExecutionStatus(_) => "execution_status",
        DaemonEventKind::ExecutionRead(_) => "execution_read",
        DaemonEventKind::ExecutionAttachmentStarted(_) => "execution_attachment_started",
        DaemonEventKind::ExecutionLeaseRenewed(_) => "execution_lease_renewed",
        DaemonEventKind::ExecutionOutput(_) => "execution_output",
        DaemonEventKind::ExecutionInputAccepted(_) => "execution_input_accepted",
        DaemonEventKind::ExecutionResizeAccepted(_) => "execution_resize_accepted",
        DaemonEventKind::ExecutionDetachAccepted(_) => "execution_detach_accepted",
        DaemonEventKind::ExecutionKillAccepted(_) => "execution_kill_accepted",
        DaemonEventKind::ExecutionAttachmentFinished(_) => "execution_attachment_finished",
        DaemonEventKind::Error(_) => "error",
    }
}

#[cfg(test)]
mod tests {
    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio::net::UnixStream;
    use tokio_util::codec::{Framed, LinesCodec};

    use super::*;

    async fn handshake(
        server: UnixStream,
        daemon_instance_id: agl_ids::DaemonInstanceId,
    ) -> Framed<UnixStream, LinesCodec> {
        let mut server = Framed::new(
            server,
            LinesCodec::new_with_max_length(MAX_JSONL_FRAME_BYTES),
        );
        let request: DaemonRequest =
            serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
        assert!(matches!(request.kind, DaemonRequestKind::Hello(_)));
        server
            .send(
                serde_json::to_string(&DaemonEvent::new(
                    Some(request.request_id),
                    DaemonEventKind::Hello(HelloEvent {
                        protocol_version: PROTOCOL_VERSION.to_owned(),
                        product_version: "test".to_owned(),
                        daemon_instance_id,
                        capabilities: Vec::new(),
                    }),
                ))
                .unwrap(),
            )
            .await
            .unwrap();
        server
    }

    fn setup_smoke_request() -> SetupSmokeSessionOpenRequest {
        SetupSmokeSessionOpenRequest {
            workspace_root: "/workspace".to_owned(),
            function_ref: "gemma4-12b".to_owned(),
            staged_bindings: agl_config::ModelBindings {
                version: 1,
                models: BTreeMap::from([(
                    agl_config::ModelId::new("gemma4-12b").unwrap(),
                    agl_config::ModelBinding {
                        path: "/models/gemma4-12b.gguf".into(),
                    },
                )]),
            },
            runtime_plan: SetupSmokeRuntimePlan {
                profile_id: "cpu-test".to_owned(),
                selected_device: None,
                runtime: agl_config::InferenceRuntimeConfig {
                    gpu_layers: 0,
                    context_tokens: 4_096,
                    threads: 2,
                    device: None,
                    batch_size: Some(128),
                    ubatch_size: Some(64),
                    flash_attention: Some(agl_config::RuntimeSwitch::Off),
                    cache_type_k: None,
                    cache_type_v: None,
                    mmap: Some(true),
                    kv_unified: Some(true),
                    mtp: agl_config::MtpRuntimeConfig::default(),
                },
                smoke_timeout_seconds: 30,
                expected_speed: "test".to_owned(),
            },
            max_output_tokens: 32,
        }
    }

    async fn send_snapshot_transfer(
        server: &mut Framed<UnixStream, LinesCodec>,
        request_id: &RequestId,
        snapshot: &SessionPresentationSnapshot,
        purpose: SessionPresentationSnapshotTransferPurpose,
    ) {
        let transfer =
            SessionPresentationSnapshotTransfer::encode(RequestId::generate(), purpose, snapshot)
                .unwrap();
        let mut frames = Vec::with_capacity(transfer.chunks.len() + 2);
        frames.push(DaemonEventKind::SessionPresentationSnapshotManifest(
            transfer.manifest,
        ));
        frames.extend(
            transfer
                .chunks
                .into_iter()
                .map(DaemonEventKind::SessionPresentationSnapshotChunk),
        );
        frames.push(DaemonEventKind::SessionPresentationSnapshotFinished(
            transfer.finished,
        ));
        for frame in frames {
            server
                .send(
                    serde_json::to_string(&DaemonEvent::new(Some(request_id.clone()), frame))
                        .unwrap(),
                )
                .await
                .unwrap();
        }
    }

    fn run_stream_event(run_id: &RunId, sequence: u64) -> DaemonEventKind {
        DaemonEventKind::RunEvent(Box::new(agl_events::SafeRuntimeEventEnvelope {
            schema: agl_events::EVENT_SCHEMA.to_owned(),
            event_id: agl_ids::EventId::generate(),
            sequence,
            occurred_at_unix_ms: sequence,
            scope: agl_events::EventScope::builder(run_id.clone())
                .build()
                .unwrap(),
            request_id: None,
            caused_by: None,
            payload: agl_events::SafeRuntimeEvent::TurnStarted {
                user_input_bytes: 0,
            },
        }))
    }

    #[test]
    fn peer_identity_check_fails_closed_for_another_uid() {
        assert!(verify_peer_identity(1000, 1000).is_ok());
        assert!(matches!(
            verify_peer_identity(1001, 1000),
            Err(ClientError::IdentityMismatch(_))
        ));
    }

    #[tokio::test]
    async fn initial_missing_socket_is_the_only_standalone_availability_signal() {
        let socket_path =
            std::env::temp_dir().join(format!("agl-client-missing-{}.sock", RequestId::generate()));
        let error = match AgentLibreClient::connect(&socket_path).await {
            Ok(_) => panic!("missing socket unexpectedly accepted a daemon connection"),
            Err(error) => error,
        };
        assert!(matches!(error, ClientError::DaemonUnavailable(_)));
    }

    #[tokio::test]
    async fn connected_invalid_daemon_is_not_reported_as_unavailable() {
        let socket_path =
            std::env::temp_dir().join(format!("agl-client-invalid-{}.sock", RequestId::generate()));
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut server = Framed::new(
                stream,
                LinesCodec::new_with_max_length(MAX_JSONL_FRAME_BYTES),
            );
            let _: DaemonRequest =
                serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
            server.send("not-json".to_string()).await.unwrap();
        });

        let error = match AgentLibreClient::connect(&socket_path).await {
            Ok(_) => panic!("invalid daemon unexpectedly completed the handshake"),
            Err(error) => error,
        };
        assert_eq!(error, ClientError::InvalidProtocolFrame);
        server.await.unwrap();
        std::fs::remove_file(socket_path).unwrap();
    }

    #[tokio::test]
    async fn connected_silent_daemon_has_a_bounded_unhealthy_handshake() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let server = tokio::spawn(async move {
            let mut server = Framed::new(
                server_stream,
                LinesCodec::new_with_max_length(MAX_JSONL_FRAME_BYTES),
            );
            let request: DaemonRequest =
                serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
            assert!(matches!(request.kind, DaemonRequestKind::Hello(_)));
            tokio::time::sleep(Duration::from_millis(50)).await;
        });

        let error = match AgentLibreClient::from_test_stream_with_handshake_timeout(
            client_stream,
            Duration::from_millis(5),
        )
        .await
        {
            Ok(_) => panic!("silent daemon unexpectedly completed the handshake"),
            Err(error) => error,
        };
        assert_eq!(error, ClientError::HandshakeTimeout);
        assert!(!matches!(error, ClientError::DaemonUnavailable(_)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn inference_status_routes_as_one_safe_typed_response() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let server = tokio::spawn(async move {
            let mut server = handshake(server_stream, agl_ids::DaemonInstanceId::generate()).await;
            let request: DaemonRequest =
                serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
            assert!(matches!(
                request.kind,
                DaemonRequestKind::InferenceStatus(InferenceStatusRequest {})
            ));
            server
                .send(
                    serde_json::to_string(&DaemonEvent::new(
                        Some(request.request_id),
                        DaemonEventKind::InferenceStatus(InferenceStatusEvent {
                            worker_build_id: "sha256:test-worker".to_owned(),
                            worker_state: ProtocolInferenceWorkerState::CoolingDown,
                            worker_pid: None,
                            launch_generation: None,
                            physical_device_id: Some("pci:0000:03:00.0".to_owned()),
                            reserved_bytes: 0,
                            cooldown_not_before_unix_ms: Some(9_000),
                        }),
                    ))
                    .unwrap(),
                )
                .await
                .unwrap();
        });

        let client = AgentLibreClient::from_test_stream(client_stream)
            .await
            .unwrap();
        let status = client.inference_status().await.unwrap();
        assert_eq!(status.worker_build_id, "sha256:test-worker");
        assert_eq!(
            status.worker_state,
            ProtocolInferenceWorkerState::CoolingDown
        );
        assert_eq!(status.cooldown_not_before_unix_ms, Some(9_000));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn setup_smoke_session_open_routes_only_the_typed_request() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let expected = setup_smoke_request();
        let server_expected = expected.clone();
        let session_id = agl_ids::SessionId::generate();
        let server_session_id = session_id.clone();
        let server = tokio::spawn(async move {
            let mut server = handshake(server_stream, agl_ids::DaemonInstanceId::generate()).await;
            let request: DaemonRequest =
                serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
            assert_eq!(
                request.kind,
                DaemonRequestKind::SetupSmokeSessionOpen(server_expected)
            );
            server
                .send(
                    serde_json::to_string(&DaemonEvent::new(
                        Some(request.request_id),
                        DaemonEventKind::SessionOpened(SessionOpenedEvent {
                            session_id: server_session_id,
                            resumed: false,
                        }),
                    ))
                    .unwrap(),
                )
                .await
                .unwrap();
        });

        let client = AgentLibreClient::from_test_stream(client_stream)
            .await
            .unwrap();
        let opened = client.open_setup_smoke_session(expected).await.unwrap();
        assert_eq!(opened.session_id, session_id);
        assert!(!opened.resumed);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn dispatcher_routes_out_of_order_responses_by_request_identity() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let server = tokio::spawn(async move {
            let mut server = handshake(server_stream, agl_ids::DaemonInstanceId::generate()).await;
            let first: DaemonRequest =
                serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
            let second: DaemonRequest =
                serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
            for request in [second, first] {
                let kind = match request.kind {
                    DaemonRequestKind::SessionList(_) => {
                        DaemonEventKind::SessionList(SessionListEvent {
                            sessions: Vec::new(),
                        })
                    }
                    DaemonRequestKind::ExecutionList(_) => {
                        DaemonEventKind::ExecutionList(ExecutionListEvent {
                            executions: Vec::new(),
                        })
                    }
                    other => panic!("unexpected request: {other:?}"),
                };
                server
                    .send(
                        serde_json::to_string(&DaemonEvent::new(Some(request.request_id), kind))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
            }
        });
        let client = AgentLibreClient::from_test_stream(client_stream)
            .await
            .unwrap();
        let (sessions, executions) = tokio::join!(
            client.list_sessions(SessionListRequest::default()),
            client.execution_list(ExecutionListRequest {
                session_id: None,
                root_run_id: None,
                include_finished: false,
            })
        );
        assert!(sessions.unwrap().sessions.is_empty());
        assert!(executions.unwrap().executions.is_empty());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn execution_attachment_accepts_monotonic_output_with_metadata_sequence_gaps() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let execution_id = agl_ids::ExecutionId::generate();
        let server_execution_id = execution_id.clone();
        let server = tokio::spawn(async move {
            let mut server = handshake(server_stream, agl_ids::DaemonInstanceId::generate()).await;
            let request: DaemonRequest =
                serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
            let attachment_id = request.request_id.clone();
            assert!(matches!(
                request.kind,
                DaemonRequestKind::ExecutionAttach(ExecutionAttachRequest {
                    ref execution_id,
                    after_sequence: 0,
                    writable: false,
                }) if execution_id == &server_execution_id
            ));
            let status = ExecutionStatus {
                execution_id: server_execution_id.clone(),
                owner: ExecutionOwner::Session {
                    session_id: agl_ids::SessionId::generate(),
                    root_run_id: RunId::generate(),
                },
                state: ExecutionState::Running,
                profile: ExecutionProfile::Workspace,
                io: ExecutionIo::Pty,
                cwd: std::path::PathBuf::from("/workspace"),
                terminal_size: Some(TerminalSize::default()),
                exit: None,
                first_retained_sequence: Some(3),
                last_sequence: 7,
                retained_bytes: 2,
                discarded_output_bytes: 0,
                output_truncated: false,
                output_expired: false,
                started_at_unix_ms: Some(1),
                finished_at_unix_ms: None,
                error_code: None,
            };
            let events = [
                DaemonEventKind::ExecutionAttachmentStarted(ExecutionAttachmentStartedEvent {
                    attachment_id: attachment_id.clone(),
                    status,
                    writable: false,
                    writer_lease_id: None,
                    next_sequence: 0,
                    lease_ttl_ms: None,
                    heartbeat_interval_ms: None,
                }),
                DaemonEventKind::ExecutionOutput(ExecutionOutputEvent {
                    attachment_id: attachment_id.clone(),
                    execution_id: server_execution_id.clone(),
                    chunk: ExecutionOutputChunk {
                        sequence: 3,
                        channel: ExecutionChannel::Terminal,
                        bytes: ProcessBytes::from_bytes(b"a"),
                    },
                    state: ExecutionState::Running,
                }),
                DaemonEventKind::ExecutionOutput(ExecutionOutputEvent {
                    attachment_id: attachment_id.clone(),
                    execution_id: server_execution_id.clone(),
                    chunk: ExecutionOutputChunk {
                        sequence: 5,
                        channel: ExecutionChannel::Terminal,
                        bytes: ProcessBytes::from_bytes(b"b"),
                    },
                    state: ExecutionState::Running,
                }),
                DaemonEventKind::ExecutionAttachmentFinished(ExecutionAttachmentFinishedEvent {
                    attachment_id,
                    execution_id: server_execution_id,
                    state: ExecutionState::Running,
                    last_delivered_sequence: 7,
                    reason: ExecutionAttachmentFinishReason::Detached,
                }),
            ];
            for event in events {
                server
                    .send(
                        serde_json::to_string(&DaemonEvent::new(
                            Some(request.request_id.clone()),
                            event,
                        ))
                        .unwrap(),
                    )
                    .await
                    .unwrap();
            }
        });

        let client = AgentLibreClient::from_test_stream(client_stream)
            .await
            .unwrap();
        let mut attachment = client
            .attach_execution(execution_id, 0, false)
            .await
            .unwrap();
        for expected in [3, 5] {
            assert!(matches!(
                attachment.next().await.unwrap(),
                Some(ExecutionAttachmentEvent::Output(event))
                    if event.chunk.sequence == expected
            ));
        }
        assert!(matches!(
            attachment.next().await.unwrap(),
            Some(ExecutionAttachmentEvent::Finished(event))
                if event.last_delivered_sequence == 7
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn outstanding_route_table_rejects_overflow_without_growing_unbounded() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let server = tokio::spawn(async move {
            let mut server = handshake(server_stream, agl_ids::DaemonInstanceId::generate()).await;
            for _ in 0..CONNECTION_ROUTE_CAPACITY {
                let _: DaemonRequest =
                    serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(50), server.next())
                    .await
                    .is_err(),
                "the route rejected by the client must not reach the daemon"
            );
        });
        let client = AgentLibreClient::from_test_stream(client_stream)
            .await
            .unwrap();
        let mut streams = Vec::with_capacity(CONNECTION_ROUTE_CAPACITY + 1);
        for _ in 0..=CONNECTION_ROUTE_CAPACITY {
            loop {
                match client
                    .stream(DaemonRequestKind::RunSubscribe(RunSubscribeRequest {
                        run_id: RunId::generate(),
                        after_sequence: 0,
                    }))
                    .await
                {
                    Ok(stream) => {
                        streams.push(stream);
                        break;
                    }
                    Err(ClientError::InputBackpressure) => tokio::task::yield_now().await,
                    Err(error) => panic!("unexpected stream admission error: {error}"),
                }
            }
        }
        let error =
            tokio::time::timeout(Duration::from_secs(1), streams.last_mut().unwrap().recv())
                .await
                .expect("overflow route was not rejected")
                .unwrap_err();
        assert_eq!(error, ClientError::InputBackpressure);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn dropped_subscription_cancel_ack_does_not_poison_other_routes() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let daemon_id = agl_ids::DaemonInstanceId::generate();
        let session_id = agl_ids::SessionId::generate();
        let server_session = session_id.clone();
        let server = tokio::spawn(async move {
            let mut server = handshake(server_stream, daemon_id.clone()).await;
            let subscribe: DaemonRequest =
                serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
            let subscription_id = subscribe.request_id.clone();
            let snapshot = empty_snapshot(server_session, daemon_id);
            send_snapshot_transfer(
                &mut server,
                &subscription_id,
                &snapshot,
                SessionPresentationSnapshotTransferPurpose::SubscriptionInitial,
            )
            .await;
            let cancel: DaemonRequest =
                serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
            server
                .send(
                    serde_json::to_string(&DaemonEvent::new(
                        Some(cancel.request_id),
                        DaemonEventKind::SubscriptionCancelled(SubscriptionCancelledEvent {
                            subscription_request_id: subscription_id,
                        }),
                    ))
                    .unwrap(),
                )
                .await
                .unwrap();
            let list: DaemonRequest =
                serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
            server
                .send(
                    serde_json::to_string(&DaemonEvent::new(
                        Some(list.request_id),
                        DaemonEventKind::SessionList(SessionListEvent {
                            sessions: Vec::new(),
                        }),
                    ))
                    .unwrap(),
                )
                .await
                .unwrap();
        });
        let client = AgentLibreClient::from_test_stream(client_stream)
            .await
            .unwrap();
        let subscription = client
            .subscribe_presentation(SessionPresentationSubscribeRequest {
                session_id: session_id.clone(),
            })
            .await
            .unwrap();
        drop(subscription);
        tokio::task::yield_now().await;
        assert!(
            client
                .list_sessions(SessionListRequest::default())
                .await
                .unwrap()
                .sessions
                .is_empty()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn finite_presentation_reassembles_a_split_typed_page() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let daemon_id = agl_ids::DaemonInstanceId::generate();
        let session_id = agl_ids::SessionId::generate();
        let expected = snapshot_with_text(session_id.clone(), daemon_id.clone(), 11, 900_000);
        let server_expected = expected.clone();
        let server_session = session_id.clone();
        let server = tokio::spawn(async move {
            let mut server = handshake(server_stream, daemon_id).await;
            let request: DaemonRequest =
                serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
            assert!(matches!(
                request.kind,
                DaemonRequestKind::SessionPresentation(SessionPresentationRequest {
                    ref session_id,
                    page_cursor: Some(ref cursor),
                }) if session_id == &server_session && cursor == "older-page"
            ));
            send_snapshot_transfer(
                &mut server,
                &request.request_id,
                &server_expected,
                SessionPresentationSnapshotTransferPurpose::Requested,
            )
            .await;
        });
        let client = AgentLibreClient::from_test_stream(client_stream)
            .await
            .unwrap();
        let snapshot = client
            .session_presentation(SessionPresentationRequest {
                session_id,
                page_cursor: Some("older-page".to_owned()),
            })
            .await
            .unwrap();
        assert_eq!(snapshot, expected);
        server.await.unwrap();
    }

    #[test]
    fn snapshot_assembler_rejects_order_count_digest_identity_and_oversize() {
        let daemon_id = agl_ids::DaemonInstanceId::generate();
        let session_id = agl_ids::SessionId::generate();
        let snapshot = snapshot_with_text(session_id.clone(), daemon_id.clone(), 4, 900_000);
        let transfer = SessionPresentationSnapshotTransfer::encode(
            RequestId::generate(),
            SessionPresentationSnapshotTransferPurpose::Requested,
            &snapshot,
        )
        .unwrap();

        let mut out_of_order = PresentationSnapshotAssembler::new(
            &session_id,
            &daemon_id,
            ExpectedSnapshotPurpose::Requested,
        );
        out_of_order.manifest(transfer.manifest.clone()).unwrap();
        assert!(matches!(
            out_of_order.chunk(transfer.chunks[1].clone()),
            Err(ClientError::SnapshotChunkOutOfOrder {
                expected: 0,
                actual: 1
            })
        ));

        let mut bad_count = transfer.manifest.clone();
        bad_count.chunk_count = bad_count.chunk_count.saturating_add(1);
        let mut count_assembler = PresentationSnapshotAssembler::new(
            &session_id,
            &daemon_id,
            ExpectedSnapshotPurpose::Requested,
        );
        assert!(matches!(
            count_assembler.manifest(bad_count),
            Err(ClientError::SnapshotTransferInvalid(_))
        ));

        let mut identity_assembler = PresentationSnapshotAssembler::new(
            &session_id,
            &daemon_id,
            ExpectedSnapshotPurpose::Requested,
        );
        identity_assembler
            .manifest(transfer.manifest.clone())
            .unwrap();
        let mut foreign_chunk = transfer.chunks[0].clone();
        foreign_chunk.transfer.transfer_id = RequestId::generate();
        assert!(matches!(
            identity_assembler.chunk(foreign_chunk),
            Err(ClientError::IdentityMismatch(_))
        ));

        let wrong_digest = PresentationSnapshotDigest::from_bytes(b"wrong snapshot");
        let mut digest_assembler = PresentationSnapshotAssembler::new(
            &session_id,
            &daemon_id,
            ExpectedSnapshotPurpose::Requested,
        );
        let mut digest_manifest = transfer.manifest.clone();
        digest_manifest.digest = wrong_digest.clone();
        digest_assembler.manifest(digest_manifest).unwrap();
        for chunk in transfer.chunks.clone() {
            digest_assembler.chunk(chunk).unwrap();
        }
        let mut digest_finished = transfer.finished.clone();
        digest_finished.digest = wrong_digest;
        assert_eq!(
            digest_assembler.finished(digest_finished).unwrap_err(),
            ClientError::SnapshotDigestMismatch
        );

        let mut oversized = transfer.manifest;
        oversized.decoded_bytes = u64::try_from(MAX_PRESENTATION_CONTENT_BYTES + 1).unwrap();
        let mut oversized_assembler = PresentationSnapshotAssembler::new(
            &session_id,
            &daemon_id,
            ExpectedSnapshotPurpose::Requested,
        );
        assert!(matches!(
            oversized_assembler.manifest(oversized),
            Err(ClientError::SnapshotTransferInvalid(_))
        ));
    }

    #[tokio::test]
    async fn subscription_reassembles_replacement_at_one_live_revision() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let daemon_id = agl_ids::DaemonInstanceId::generate();
        let session_id = agl_ids::SessionId::generate();
        let initial = empty_snapshot(session_id.clone(), daemon_id.clone());
        let replacement = snapshot_with_text(session_id.clone(), daemon_id.clone(), 1, 900_000);
        let replacement_for_server = replacement.clone();
        let event_id = agl_ids::EventId::generate();
        let server_event_id = event_id.clone();
        let (release_server, keep_server_open) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut server = handshake(server_stream, daemon_id).await;
            let request: DaemonRequest =
                serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
            send_snapshot_transfer(
                &mut server,
                &request.request_id,
                &initial,
                SessionPresentationSnapshotTransferPurpose::SubscriptionInitial,
            )
            .await;
            send_snapshot_transfer(
                &mut server,
                &request.request_id,
                &replacement_for_server,
                SessionPresentationSnapshotTransferPurpose::Replacement {
                    event_id: server_event_id,
                },
            )
            .await;
            let _ = keep_server_open.await;
        });
        let client = AgentLibreClient::from_test_stream(client_stream)
            .await
            .unwrap();
        let mut subscription = client
            .subscribe_presentation(SessionPresentationSubscribeRequest {
                session_id: session_id.clone(),
            })
            .await
            .unwrap();
        assert_eq!(subscription.snapshot.cursor.revision, 0);
        assert!(matches!(
            subscription.next().await.unwrap(),
            Some(PresentationSubscriptionEvent::SnapshotReplaced {
                event_id: received_event_id,
                snapshot: received_snapshot,
            }) if received_event_id == event_id && *received_snapshot == replacement
        ));
        assert_eq!(subscription.snapshot, replacement);
        release_server.send(()).unwrap();
        drop(subscription);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn saturated_subscription_is_cancelled_without_poisoning_other_routes() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let run_id = RunId::generate();
        let server_run_id = run_id.clone();
        let (cancel_seen, cancellation_observed) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut server = handshake(server_stream, agl_ids::DaemonInstanceId::generate()).await;
            let subscribe: DaemonRequest =
                serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
            assert!(matches!(
                subscribe.kind,
                DaemonRequestKind::RunSubscribe(RunSubscribeRequest { ref run_id, .. })
                    if run_id == &server_run_id
            ));
            let subscription_id = subscribe.request_id;
            server
                .send(
                    serde_json::to_string(&DaemonEvent::new(
                        Some(subscription_id.clone()),
                        DaemonEventKind::RunSubscriptionStarted(RunSubscriptionStartedEvent {
                            run_id: server_run_id.clone(),
                            after_sequence: 0,
                            replay_boundary: 0,
                        }),
                    ))
                    .unwrap(),
                )
                .await
                .unwrap();

            for sequence in 1..=u64::try_from(SUBSCRIPTION_CAPACITY + 1).unwrap() {
                server
                    .send(
                        serde_json::to_string(&DaemonEvent::new(
                            Some(subscription_id.clone()),
                            run_stream_event(&server_run_id, sequence),
                        ))
                        .unwrap(),
                    )
                    .await
                    .unwrap();
            }

            let cancel_line = tokio::time::timeout(Duration::from_secs(3), server.next())
                .await
                .expect("saturated stream did not request cancellation")
                .expect("client closed before requesting stream cancellation")
                .unwrap();
            let cancel: DaemonRequest = serde_json::from_str(&cancel_line).unwrap();
            assert!(matches!(
                cancel.kind,
                DaemonRequestKind::SubscriptionCancel(SubscriptionCancelRequest {
                    ref subscription_request_id,
                }) if subscription_request_id == &subscription_id
            ));
            server
                .send(
                    serde_json::to_string(&DaemonEvent::new(
                        Some(cancel.request_id),
                        DaemonEventKind::SubscriptionCancelled(SubscriptionCancelledEvent {
                            subscription_request_id: subscription_id.clone(),
                        }),
                    ))
                    .unwrap(),
                )
                .await
                .unwrap();

            // A synchronous daemon producer can already have another frame in
            // flight when cancellation is acknowledged. It remains scoped to
            // the abandoned stream and must not poison the multiplexed socket.
            server
                .send(
                    serde_json::to_string(&DaemonEvent::new(
                        Some(subscription_id),
                        run_stream_event(
                            &server_run_id,
                            u64::try_from(SUBSCRIPTION_CAPACITY + 2).unwrap(),
                        ),
                    ))
                    .unwrap(),
                )
                .await
                .unwrap();
            cancel_seen.send(()).unwrap();

            let list: DaemonRequest =
                serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
            assert!(matches!(list.kind, DaemonRequestKind::SessionList(_)));
            server
                .send(
                    serde_json::to_string(&DaemonEvent::new(
                        Some(list.request_id),
                        DaemonEventKind::SessionList(SessionListEvent {
                            sessions: Vec::new(),
                        }),
                    ))
                    .unwrap(),
                )
                .await
                .unwrap();
        });

        let client = AgentLibreClient::from_test_stream(client_stream)
            .await
            .unwrap();
        let mut subscription = client
            .subscribe_run(RunSubscribeRequest {
                run_id,
                after_sequence: 0,
            })
            .await
            .unwrap();
        let subscription_id = subscription.request_id().clone();
        tokio::time::timeout(Duration::from_secs(3), cancellation_observed)
            .await
            .expect("client never observed stream saturation")
            .unwrap();

        assert!(matches!(
            subscription.next().await.unwrap_err(),
            ClientError::SubscriptionLagged {
                request_id,
                last_sequence,
            } if request_id == subscription_id
                && last_sequence == u64::try_from(SUBSCRIPTION_CAPACITY).unwrap()
        ));
        assert!(
            client
                .list_sessions(SessionListRequest::default())
                .await
                .unwrap()
                .sessions
                .is_empty()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn human_terminal_ensure_is_a_typed_one_shot_response() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let session_id = agl_ids::SessionId::generate();
        let server_session = session_id.clone();
        let expected_terminal = terminal(&session_id);
        let server_terminal = expected_terminal.clone();
        let server = tokio::spawn(async move {
            let mut server = handshake(server_stream, agl_ids::DaemonInstanceId::generate()).await;
            let request: DaemonRequest =
                serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
            match request.kind {
                DaemonRequestKind::HumanTerminalEnsure(ref ensure) => {
                    assert_eq!(ensure.session_id, server_session);
                    assert_eq!(ensure.shell_profile_id, "bash-managed");
                }
                ref other => panic!("unexpected request: {other:?}"),
            }
            server
                .send(
                    serde_json::to_string(&DaemonEvent::new(
                        Some(request.request_id),
                        DaemonEventKind::HumanTerminalEnsured(HumanTerminalEnsuredEvent {
                            terminal: server_terminal,
                            disposition: TerminalEnsureDisposition::Created,
                        }),
                    ))
                    .unwrap(),
                )
                .await
                .unwrap();
        });
        let client = AgentLibreClient::from_test_stream(client_stream)
            .await
            .unwrap();
        let ensured = client
            .ensure_human_terminal(human_terminal_request(session_id))
            .await
            .unwrap();
        assert_eq!(ensured.terminal, expected_terminal);
        assert_eq!(ensured.disposition, TerminalEnsureDisposition::Created);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn human_host_terminal_ensure_uses_the_explicit_operator_request_family() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let session_id = agl_ids::SessionId::generate();
        let server_session = session_id.clone();
        let expected_terminal = terminal(&session_id);
        let server_terminal = expected_terminal.clone();
        let server = tokio::spawn(async move {
            let mut server = handshake(server_stream, agl_ids::DaemonInstanceId::generate()).await;
            let request: DaemonRequest =
                serde_json::from_str(&server.next().await.unwrap().unwrap()).unwrap();
            match request.kind {
                DaemonRequestKind::HumanHostTerminalEnsure(ref ensure) => {
                    assert_eq!(ensure.terminal.session_id, server_session);
                    assert_eq!(ensure.terminal.profile, ExecutionProfile::Host);
                    assert!(ensure.confirm_host_authority);
                }
                ref other => panic!("unexpected request: {other:?}"),
            }
            server
                .send(
                    serde_json::to_string(&DaemonEvent::new(
                        Some(request.request_id),
                        DaemonEventKind::HumanTerminalEnsured(HumanTerminalEnsuredEvent {
                            terminal: server_terminal,
                            disposition: TerminalEnsureDisposition::Created,
                        }),
                    ))
                    .unwrap(),
                )
                .await
                .unwrap();
        });
        let client = AgentLibreClient::from_test_stream(client_stream)
            .await
            .unwrap();
        let ensured = client
            .ensure_human_host_terminal(human_host_terminal_request(session_id))
            .await
            .unwrap();
        assert_eq!(ensured.terminal, expected_terminal);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn unconfirmed_host_terminal_is_rejected_before_it_reaches_the_socket() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let (release, wait) = oneshot::channel();
        let server = tokio::spawn(async move {
            let _server = handshake(server_stream, agl_ids::DaemonInstanceId::generate()).await;
            let _ = wait.await;
        });
        let client = AgentLibreClient::from_test_stream(client_stream)
            .await
            .unwrap();
        let mut request = human_host_terminal_request(agl_ids::SessionId::generate());
        request.confirm_host_authority = false;

        let error = client
            .ensure_human_host_terminal(request)
            .await
            .unwrap_err();
        assert!(matches!(error, ClientError::InvalidRequest(_)));
        release.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn invalid_terminal_overlay_is_rejected_before_it_reaches_the_socket() {
        let (client_stream, server_stream) = UnixStream::pair().unwrap();
        let (release, wait) = oneshot::channel();
        let server = tokio::spawn(async move {
            let _server = handshake(server_stream, agl_ids::DaemonInstanceId::generate()).await;
            let _ = wait.await;
        });
        let client = AgentLibreClient::from_test_stream(client_stream)
            .await
            .unwrap();
        let mut request = human_terminal_request(agl_ids::SessionId::generate());
        request
            .agl_env
            .values
            .insert("PATH".to_owned(), "/untrusted".to_owned());

        let error = client.ensure_human_terminal(request).await.unwrap_err();
        assert!(matches!(error, ClientError::InvalidRequest(_)));
        release.send(()).unwrap();
        server.await.unwrap();
    }

    fn human_terminal_request(session_id: agl_ids::SessionId) -> HumanTerminalEnsureRequest {
        HumanTerminalEnsureRequest {
            session_id,
            client_submission_id: "terminal-create-1".to_owned(),
            execution_context_revision: 1,
            profile: ExecutionProfile::Workspace,
            shell_profile_id: "bash-managed".to_owned(),
            terminal_size: TerminalSize::default(),
            agl_env: StructuredEnvironmentOverlay::default(),
            host_startup: HostStartupPolicy::ManagedOnly,
        }
    }

    fn human_host_terminal_request(
        session_id: agl_ids::SessionId,
    ) -> HumanHostTerminalEnsureRequest {
        let mut terminal = human_terminal_request(session_id);
        terminal.profile = ExecutionProfile::Host;
        HumanHostTerminalEnsureRequest {
            terminal,
            confirm_host_authority: true,
        }
    }

    fn display_path(text: &str) -> SanitizedDisplayPath {
        SanitizedDisplayPath {
            text: text.to_owned(),
            truncated: false,
        }
    }

    fn terminal(session_id: &agl_ids::SessionId) -> TerminalSessionView {
        TerminalSessionView {
            terminal_id: agl_ids::TerminalSessionId::generate(),
            execution_id: agl_ids::ExecutionId::generate(),
            owner: TerminalOwnerView::Human {
                session_id: session_id.clone(),
            },
            profile: ExecutionProfile::Workspace,
            shell: ShellProfileView {
                profile_id: "bash-managed".to_owned(),
                program: display_path("/bin/bash"),
                executable_digest: "sha256:aaaaaaaa".to_owned(),
                config_digest: "sha256:bbbbbbbb".to_owned(),
            },
            workspace_root: display_path("/workspace"),
            cwd: display_path("/workspace"),
            initial_environment_digest: "sha256:cccccccc".to_owned(),
            environment_names: vec!["PATH".to_owned()],
            command_sequence: 0,
            prompt_generation: Some(1),
            prompt_state: TerminalPromptState::Ready,
            process_state: ExecutionState::Running,
            exit: None,
            writer: TerminalWriterView::Owner,
            promoted: false,
        }
    }

    fn empty_snapshot(
        session_id: agl_ids::SessionId,
        daemon_instance_id: agl_ids::DaemonInstanceId,
    ) -> SessionPresentationSnapshot {
        SessionPresentationSnapshot {
            session_id: session_id.clone(),
            cursor: PresentationCursor {
                daemon_instance_id,
                revision: 0,
            },
            older_page_cursor: None,
            header: SessionHeader {
                session_id: session_id.clone(),
                status: SessionPresentationStatus::Active,
                durable: true,
                resumed: false,
                title: None,
                function_name: "test".to_owned(),
                model_id: None,
                operation_mode: ProtocolToolMode::ReadOnly,
                selected_skills: Vec::new(),
                runtime_context_revision: 1,
                workspace_root: display_path("/tmp"),
                workspace_history_scope: format!("sha256:{}", "a".repeat(64)),
                cwd: display_path("/tmp"),
                execution_context_revision: 1,
                context_used_tokens: None,
                context_limit_tokens: None,
                active_run_count: 0,
                queued_prompt_count: 0,
                active_execution_count: 0,
            },
            items: Vec::new(),
            active_run: None,
            queued_prompts: Vec::new(),
            terminals: Vec::new(),
            executions: Vec::new(),
            human_commands: Vec::new(),
            activity: None,
            command_context: CommandContext {
                session_id: Some(session_id),
                session_active: true,
                active_or_queued_turns: 0,
                active_executions: 0,
                host_shell_available: true,
                operation_mode: ProtocolToolMode::ReadOnly,
            },
        }
    }

    fn snapshot_with_text(
        session_id: agl_ids::SessionId,
        daemon_instance_id: agl_ids::DaemonInstanceId,
        revision: u64,
        text_bytes: usize,
    ) -> SessionPresentationSnapshot {
        let mut snapshot = empty_snapshot(session_id, daemon_instance_id);
        snapshot.cursor.revision = revision;
        snapshot.older_page_cursor = Some(format!("older-{revision}"));
        snapshot.items.push(SessionPresentationItem::UserMessage {
            message_id: agl_ids::MessageId::generate(),
            content: agl_content::Content::text("x".repeat(text_bytes)).unwrap(),
        });
        snapshot
    }
}
