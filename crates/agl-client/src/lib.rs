use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, Interval, MissedTickBehavior};
use tokio_util::codec::{Framed, LinesCodec};

use agl_ids::{RequestId, RunId};
use agl_protocol::*;

const OUTBOUND_CAPACITY: usize = 128;
const SUBSCRIPTION_CAPACITY: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientError {
    Io(String),
    Json(String),
    Protocol {
        code: ProtocolErrorCode,
        retryable: bool,
    },
    SchemaMismatch {
        expected: &'static str,
        actual: String,
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
    DaemonInstanceChanged,
    ConnectionClosed,
    InputBackpressure,
}

impl Display for ClientError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => formatter.write_str("daemon connection I/O failed"),
            Self::Json(_) => formatter.write_str("daemon protocol JSON was invalid"),
            Self::Protocol { code, retryable } => {
                write!(
                    formatter,
                    "daemon request failed with {code:?} (retryable={retryable})"
                )
            }
            Self::SchemaMismatch { expected, actual } => {
                write!(
                    formatter,
                    "daemon schema {actual} does not match {expected}"
                )
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
            Self::DaemonInstanceChanged => {
                formatter.write_str("daemon instance changed; request a fresh snapshot")
            }
            Self::ConnectionClosed => formatter.write_str("daemon connection closed"),
            Self::InputBackpressure => formatter.write_str("client request queue is full"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<std::io::Error> for ClientError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

#[derive(Clone)]
pub struct AgentLibreClient {
    sender: mpsc::Sender<ConnectionCommand>,
    hello: Arc<RwLock<Option<HelloEvent>>>,
}

impl AgentLibreClient {
    pub async fn connect(socket_path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(socket_path).await?;
        Self::from_stream(stream).await
    }

    pub async fn from_stream(stream: UnixStream) -> Result<Self, ClientError> {
        let (sender, receiver) = mpsc::channel(OUTBOUND_CAPACITY);
        tokio::spawn(connection_task(stream, receiver));
        let client = Self {
            sender,
            hello: Arc::new(RwLock::new(None)),
        };
        let hello = match client
            .request(DaemonRequestKind::Hello(HelloRequest {
                client_name: Some("agl-client".to_owned()),
                accepted_protocol_versions: vec![PROTOCOL_VERSION.to_owned()],
            }))
            .await?
        {
            DaemonEventKind::Hello(event) => event,
            other => return Err(unexpected("hello", &other)),
        };
        if hello.protocol_version != PROTOCOL_VERSION {
            return Err(ClientError::SchemaMismatch {
                expected: PROTOCOL_VERSION,
                actual: hello.protocol_version,
            });
        }
        *client
            .hello
            .write()
            .map_err(|_| ClientError::ConnectionClosed)? = Some(hello);
        Ok(client)
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
            .stream(
                DaemonRequestKind::RunSubscribe(request),
                Expected::RunStream,
            )
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

    pub async fn execution_attach(
        &self,
        request: ExecutionAttachRequest,
    ) -> Result<ExecutionAttachment, ClientError> {
        let execution_id = request.execution_id.clone();
        let writable = request.writable;
        let mut raw = self
            .stream(
                DaemonRequestKind::ExecutionAttach(request),
                Expected::ExecutionStream,
            )
            .await?;
        let started = match raw.recv().await? {
            DaemonEventKind::ExecutionAttachmentStarted(started)
                if started.status.execution_id == execution_id && started.writable == writable =>
            {
                started
            }
            other => return Err(unexpected("execution_attachment_started", &other)),
        };
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
    ) -> Result<SessionPresentationEvent, ClientError> {
        match self
            .request(DaemonRequestKind::SessionPresentation(request))
            .await?
        {
            DaemonEventKind::SessionPresentation(event) => Ok(event),
            other => Err(unexpected("session_presentation", &other)),
        }
    }

    pub async fn subscribe_presentation(
        &self,
        request: SessionPresentationSubscribeRequest,
    ) -> Result<PresentationSubscription, ClientError> {
        let session_id = request.session_id.clone();
        let mut raw = self
            .stream(
                DaemonRequestKind::SessionPresentationSubscribe(request),
                Expected::PresentationStream,
            )
            .await?;
        let started = match raw.recv().await? {
            DaemonEventKind::SessionPresentationSubscriptionStarted(started)
                if started.snapshot.session_id == session_id =>
            {
                started
            }
            other => {
                return Err(unexpected(
                    "session_presentation_subscription_started",
                    &other,
                ));
            }
        };
        let daemon_instance_id = self.hello()?.daemon_instance_id;
        if started.snapshot.cursor.daemon_instance_id != daemon_instance_id {
            return Err(ClientError::DaemonInstanceChanged);
        }
        let next_revision = started.snapshot.cursor.revision.saturating_add(1);
        Ok(PresentationSubscription {
            snapshot: started.snapshot,
            raw,
            next_revision,
            daemon_instance_id,
            finished: false,
        })
    }

    pub async fn start_user_shell(
        &self,
        request: UserShellStartRequest,
    ) -> Result<UserShellAcceptedEvent, ClientError> {
        match self
            .request(DaemonRequestKind::UserShellStart(request))
            .await?
        {
            DaemonEventKind::UserShellAccepted(event) => Ok(event),
            other => Err(unexpected("user_shell_accepted", &other)),
        }
    }

    async fn request(&self, kind: DaemonRequestKind) -> Result<DaemonEventKind, ClientError> {
        let request_id = RequestId::generate();
        let expected = Expected::for_request(&kind, false);
        let (reply, response) = oneshot::channel();
        self.sender
            .send(ConnectionCommand::Send {
                request: DaemonRequest::new(request_id, kind),
                route: Some(Route::OneShot { expected, reply }),
            })
            .await
            .map_err(|_| ClientError::ConnectionClosed)?;
        response.await.map_err(|_| ClientError::ConnectionClosed)?
    }

    async fn stream(
        &self,
        kind: DaemonRequestKind,
        expected: Expected,
    ) -> Result<RawSubscription, ClientError> {
        let request_id = RequestId::generate();
        let (events, receiver) = mpsc::channel(SUBSCRIPTION_CAPACITY);
        let (failure, failure_receiver) = watch::channel(None);
        self.sender
            .send(ConnectionCommand::Send {
                request: DaemonRequest::new(request_id.clone(), kind),
                route: Some(Route::Stream {
                    expected,
                    events,
                    failure,
                }),
            })
            .await
            .map_err(|_| ClientError::ConnectionClosed)?;
        Ok(RawSubscription {
            request_id,
            events: receiver,
            failure: failure_receiver,
            sender: self.sender.clone(),
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
    Event(Box<SessionPresentationEventEnvelope>),
    Finished(SessionPresentationSubscriptionFinishedEvent),
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
                        && event.execution_id == self.execution_id
                        && event.chunk.sequence > self.last_sequence =>
                {
                    self.last_sequence = event.chunk.sequence;
                    self.raw.last_sequence = event.chunk.sequence;
                    return Ok(Some(ExecutionAttachmentEvent::Output(event)));
                }
                DaemonEventKind::ExecutionAttachmentFinished(event)
                    if event.attachment_id == self.attachment_id
                        && event.execution_id == self.execution_id =>
                {
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
    terminal: bool,
    last_sequence: u64,
}

impl RawSubscription {
    async fn recv(&mut self) -> Result<DaemonEventKind, ClientError> {
        tokio::select! {
            biased;
            event = self.events.recv() => event.ok_or_else(|| {
                self.failure.borrow().clone().unwrap_or(ClientError::ConnectionClosed)
            }),
            changed = self.failure.changed() => {
                changed.map_err(|_| ClientError::ConnectionClosed)?;
                Err(self.failure.borrow().clone().unwrap_or(ClientError::ConnectionClosed))
            }
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
                DaemonRequestKind::SubscriptionCancel(SubscriptionCancelRequest {
                    subscription_request_id: self.request_id.clone(),
                }),
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

enum Route {
    OneShot {
        expected: Expected,
        reply: oneshot::Sender<Result<DaemonEventKind, ClientError>>,
    },
    Stream {
        expected: Expected,
        events: mpsc::Sender<DaemonEventKind>,
        failure: watch::Sender<Option<ClientError>>,
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
    SessionPresentation,
    SubscriptionCancelled,
    UserShellAccepted,
    RunStream,
    PresentationStream,
    ExecutionStream,
}

impl Expected {
    fn for_request(kind: &DaemonRequestKind, stream: bool) -> Self {
        match kind {
            DaemonRequestKind::Hello(_) => Self::Hello,
            DaemonRequestKind::SessionOpen(_) => Self::SessionOpened,
            DaemonRequestKind::SessionClear(_) | DaemonRequestKind::SessionStatus(_) => {
                Self::SessionStatus
            }
            DaemonRequestKind::SessionFinish(_) => Self::SessionFinished,
            DaemonRequestKind::SessionList(_) => Self::SessionList,
            DaemonRequestKind::SessionTranscript(_) => Self::SessionTranscript,
            DaemonRequestKind::RunSubmit(_) => Self::RunAccepted,
            DaemonRequestKind::RunStatus(_) | DaemonRequestKind::RunCancel(_) => Self::RunStatus,
            DaemonRequestKind::RunTree(_) => Self::RunTree,
            DaemonRequestKind::RunEvents(_) => Self::RunEvents,
            DaemonRequestKind::RunSubscribe(_) if stream => Self::RunStream,
            DaemonRequestKind::ExecutionList(_) => Self::ExecutionList,
            DaemonRequestKind::ExecutionStatus(_) => Self::ExecutionStatus,
            DaemonRequestKind::ExecutionRead(_) => Self::ExecutionRead,
            DaemonRequestKind::ExecutionInput(_) => Self::ExecutionInput,
            DaemonRequestKind::ExecutionResize(_) => Self::ExecutionResize,
            DaemonRequestKind::ExecutionDetach(_) => Self::ExecutionDetach,
            DaemonRequestKind::ExecutionKill(_) => Self::ExecutionKill,
            DaemonRequestKind::ExecutionLeaseRenew(_) => Self::ExecutionLeaseRenew,
            DaemonRequestKind::ExecutionAttach(_) if stream => Self::ExecutionStream,
            DaemonRequestKind::CommandCatalog(_) => Self::CommandCatalog,
            DaemonRequestKind::CommandSuggestions(_) => Self::CommandSuggestions,
            DaemonRequestKind::ApplicationAction(_) => Self::ApplicationAction,
            DaemonRequestKind::SessionPresentation(_) => Self::SessionPresentation,
            DaemonRequestKind::SessionPresentationSubscribe(_) if stream => {
                Self::PresentationStream
            }
            DaemonRequestKind::SubscriptionCancel(_) => Self::SubscriptionCancelled,
            DaemonRequestKind::UserShellStart(_) => Self::UserShellAccepted,
            _ => Self::RunEvents,
        }
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
                Self::SessionPresentation => {
                    matches!(event, DaemonEventKind::SessionPresentation(_))
                }
                Self::SubscriptionCancelled => {
                    matches!(event, DaemonEventKind::SubscriptionCancelled(_))
                }
                Self::UserShellAccepted => matches!(event, DaemonEventKind::UserShellAccepted(_)),
                Self::RunStream => matches!(
                    event,
                    DaemonEventKind::RunSubscriptionStarted(_)
                        | DaemonEventKind::RunEvent(_)
                        | DaemonEventKind::RunSubscriptionFinished(_)
                ),
                Self::PresentationStream => matches!(
                    event,
                    DaemonEventKind::SessionPresentationSubscriptionStarted(_)
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
}

async fn connection_task(stream: UnixStream, mut commands: mpsc::Receiver<ConnectionCommand>) {
    let mut framed = Framed::new(
        stream,
        LinesCodec::new_with_max_length(MAX_JSONL_FRAME_BYTES),
    );
    let mut routes = BTreeMap::<RequestId, Route>::new();
    let mut ignored_terminals = BTreeSet::<RequestId>::new();
    let failure = loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(ConnectionCommand::Send { request, route }) = command else {
                    break ClientError::ConnectionClosed;
                };
                if let Some(route) = route {
                    if routes.insert(request.request_id.clone(), route).is_some() {
                        break ClientError::IdentityMismatch("duplicate outstanding request ID");
                    }
                } else {
                    ignored_terminals.insert(request.request_id.clone());
                }
                let line = match serde_json::to_string(&request) {
                    Ok(line) => line,
                    Err(error) => break ClientError::Json(error.to_string()),
                };
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
                    Err(error) => break ClientError::Json(error.to_string()),
                };
                if event.schema != EVENT_SCHEMA {
                    break ClientError::SchemaMismatch { expected: EVENT_SCHEMA, actual: event.schema };
                }
                let Some(request_id) = event.request_id.clone() else {
                    break ClientError::IdentityMismatch("daemon event has no request identity");
                };
                let Some(route) = routes.remove(&request_id) else {
                    if ignored_terminals.remove(&request_id) {
                        continue;
                    }
                    break ClientError::RequestMismatch { expected: request_id, actual: event.request_id };
                };
                dispatch_route(&mut routes, request_id, route, event.kind);
            }
        }
    };
    fail_routes(routes, failure);
}

fn dispatch_route(
    routes: &mut BTreeMap<RequestId, Route>,
    request_id: RequestId,
    route: Route,
    event: DaemonEventKind,
) {
    match route {
        Route::OneShot { expected, reply } => {
            let result = route_result(expected, event);
            let _ = reply.send(result);
        }
        Route::Stream {
            expected,
            events,
            failure,
        } => {
            if !expected.accepts(&event) {
                let _ = failure.send(Some(unexpected("registered response family", &event)));
                return;
            }
            if let DaemonEventKind::Error(error) = event {
                let _ = failure.send(Some(protocol_error(error)));
                return;
            }
            let terminal = expected.is_terminal(&event);
            match events.try_send(event) {
                Ok(()) if !terminal => {
                    routes.insert(
                        request_id,
                        Route::Stream {
                            expected,
                            events,
                            failure,
                        },
                    );
                }
                Ok(()) => {}
                Err(_) => {
                    let _ = failure.send(Some(ClientError::SubscriptionLagged {
                        request_id,
                        last_sequence: 0,
                    }));
                }
            }
        }
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
        match route {
            Route::OneShot { reply, .. } => {
                let _ = reply.send(Err(error.clone()));
            }
            Route::Stream { failure, .. } => {
                let _ = failure.send(Some(error.clone()));
            }
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
        DaemonEventKind::CommandCatalog(_) => "command_catalog",
        DaemonEventKind::CommandSuggestions(_) => "command_suggestions",
        DaemonEventKind::ApplicationActionResult(_) => "application_action_result",
        DaemonEventKind::SessionPresentation(_) => "session_presentation",
        DaemonEventKind::SessionPresentationSubscriptionStarted(_) => {
            "session_presentation_subscription_started"
        }
        DaemonEventKind::SessionPresentationEvent(_) => "session_presentation_event",
        DaemonEventKind::SessionPresentationSubscriptionFinished(_) => {
            "session_presentation_subscription_finished"
        }
        DaemonEventKind::SubscriptionCancelled(_) => "subscription_cancelled",
        DaemonEventKind::UserShellAccepted(_) => "user_shell_accepted",
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
        let client = AgentLibreClient::from_stream(client_stream).await.unwrap();
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
            server
                .send(
                    serde_json::to_string(&DaemonEvent::new(
                        Some(subscription_id.clone()),
                        DaemonEventKind::SessionPresentationSubscriptionStarted(
                            SessionPresentationSubscriptionStartedEvent { snapshot },
                        ),
                    ))
                    .unwrap(),
                )
                .await
                .unwrap();
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
        let client = AgentLibreClient::from_stream(client_stream).await.unwrap();
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
                workspace_root: "/tmp".to_owned(),
                cwd: "/tmp".to_owned(),
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
            executions: Vec::new(),
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
}
