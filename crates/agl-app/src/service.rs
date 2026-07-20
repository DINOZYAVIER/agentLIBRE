use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use agl_content::Content;
use agl_ids::{DaemonInstanceId, EventId, MessageId, RunId, SessionId, TurnId};
use agl_process::KillMode;
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast};

use crate::{
    ApplicationActionRequest, CommandCatalog, CommandContext, ExecutionView, HumanTerminalEnsure,
    PresentationCursor, SessionHeader, SessionLaunchOptions, SessionPresentationEvent,
    SessionPresentationEventEnvelope, SessionPresentationItem, SessionPresentationSnapshot,
    SuggestionPage, SuggestionRequest, TerminalEnsured, TerminalSessionView,
    shared_command_catalog,
};

const PRESENTATION_CHANNEL_CAPACITY: usize = 256;
const BLOCKING_BRIDGE_CAPACITY: usize = 32;
pub const MAX_PRESENTATION_PAGE_CURSOR_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationErrorCode {
    InvalidArguments,
    CommandUnavailable,
    SessionBusy,
    NotFound,
    NotAuthorized,
    AuthorizationRequired,
    ConfirmationRequired,
    StaleContextRevision,
    TerminalOwnerMismatch,
    WriterLeaseBusy,
    ModelNotInstalled,
    ModelContextTooSmall,
    SkillNotAdmitted,
    InputBackpressure,
    ResyncRequired,
    OutcomeUnknown,
    Internal,
}

impl ApplicationErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArguments => "invalid_arguments",
            Self::CommandUnavailable => "command_unavailable",
            Self::SessionBusy => "session_busy",
            Self::NotFound => "not_found",
            Self::NotAuthorized => "not_authorized",
            Self::AuthorizationRequired => "authorization_required",
            Self::ConfirmationRequired => "confirmation_required",
            Self::StaleContextRevision => "stale_context_revision",
            Self::TerminalOwnerMismatch => "terminal_owner_mismatch",
            Self::WriterLeaseBusy => "writer_lease_busy",
            Self::ModelNotInstalled => "model_not_installed",
            Self::ModelContextTooSmall => "model_context_too_small",
            Self::SkillNotAdmitted => "skill_not_admitted",
            Self::InputBackpressure => "input_backpressure",
            Self::ResyncRequired => "resync_required",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::Internal => "internal",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{code}: {message}", code = .code.as_str())]
pub struct ApplicationError {
    pub code: ApplicationErrorCode,
    pub message: String,
}

impl ApplicationError {
    pub fn new(code: ApplicationErrorCode, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > 8 * 1024 {
            let mut end = 8 * 1024;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
        }
        Self { code, message }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionOpen {
    pub launch: SessionLaunchOptions,
}

impl SessionOpen {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        self.launch.validate()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionOpened {
    pub session_id: SessionId,
    pub resumed: bool,
    pub snapshot: SessionPresentationSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptSubmission {
    pub session_id: SessionId,
    pub client_submission_id: String,
    pub content: Content,
    pub budget: PromptBudget,
}

impl PromptSubmission {
    pub fn validate(&self) -> Result<(), ApplicationError> {
        validate_submission_id(&self.client_submission_id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptBudget {
    pub wall_time_ms: u64,
    pub model_input_tokens: u64,
    pub model_output_tokens: u64,
    pub model_attempts: u32,
    pub capability_calls: u32,
}

impl Default for PromptBudget {
    fn default() -> Self {
        Self {
            wall_time_ms: 300_000,
            model_input_tokens: 1_000_000,
            model_output_tokens: 100_000,
            model_attempts: 32,
            capability_calls: 64,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptAdmission {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub ordinal: u32,
    pub queued: bool,
    pub state: PromptAdmissionState,
    pub replayed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptAdmissionState {
    Queued,
    Running,
    Waiting,
    Succeeded,
    Failed,
    Cancelled,
}

impl PromptAdmissionState {
    pub fn is_queued(self) -> bool {
        matches!(self, Self::Queued | Self::Waiting)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApplicationActionResult {
    SessionOpened {
        opened: Box<SessionOpened>,
    },
    Status {
        header: SessionHeader,
    },
    ModelChanged {
        header: SessionHeader,
    },
    ModeChanged {
        header: SessionHeader,
    },
    SkillsChanged {
        header: SessionHeader,
    },
    WorkspaceChanged {
        header: SessionHeader,
    },
    Terminals {
        terminals: Vec<TerminalSessionView>,
    },
    TerminalPromoted {
        terminal: TerminalSessionView,
    },
    Executions {
        executions: Vec<ExecutionView>,
    },
    AttachAccepted {
        execution_id: agl_ids::ExecutionId,
        read_only: bool,
    },
    KillAccepted {
        execution_id: agl_ids::ExecutionId,
        mode: KillMode,
    },
    Reloaded {
        visible_tools: Vec<String>,
        context_revision: u64,
    },
    Cleared {
        removed_messages: u64,
        cursor: PresentationCursor,
    },
    SessionExited {
        session_id: SessionId,
        cancelled_runs: u32,
        terminated_terminals: u32,
        terminated_executions: u32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresentationSnapshotPage {
    pub snapshot: SessionPresentationSnapshot,
    pub older_page_cursor: Option<String>,
}

impl PresentationSnapshotPage {
    pub fn validate(self) -> Result<Self, ApplicationError> {
        self.snapshot.validate()?;
        validate_page_cursor(self.older_page_cursor.as_deref())?;
        Ok(self)
    }
}

pub trait ApplicationBackend: Send + Sync + 'static {
    fn open_session(
        &self,
        context: ApplicationCallContext,
        request: SessionOpen,
    ) -> Result<SessionOpened, ApplicationError>;
    fn snapshot_page(
        &self,
        context: ApplicationCallContext,
        session_id: &SessionId,
        page_cursor: Option<&str>,
    ) -> Result<PresentationSnapshotPage, ApplicationError>;
    fn invoke(
        &self,
        context: ApplicationCallContext,
        request: ApplicationActionRequest,
    ) -> Result<ApplicationActionResult, ApplicationError>;
    fn submit_prompt(
        &self,
        context: ApplicationCallContext,
        request: PromptSubmission,
    ) -> Result<PromptAdmission, ApplicationError>;
    fn ensure_human_terminal(
        &self,
        context: ApplicationCallContext,
        request: HumanTerminalEnsure,
    ) -> Result<TerminalEnsured, ApplicationError>;
    fn suggestions(
        &self,
        context: ApplicationCallContext,
        request: SuggestionRequest,
    ) -> Result<SuggestionPage, ApplicationError>;
}

/// Cancellation shared with a finite synchronous application-owner call.
///
/// Backends must check it before committing multi-stage work and while polling
/// an owner for a typed terminal outcome. Dropping the async caller marks the
/// context cancelled even though Tokio cannot stop an already-started blocking
/// closure.
#[derive(Clone, Debug)]
pub struct ApplicationCallContext {
    cancelled: Arc<AtomicBool>,
}

impl ApplicationCallContext {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl Default for ApplicationCallContext {
    fn default() -> Self {
        Self::new()
    }
}

struct ProjectionState {
    snapshot: SessionPresentationSnapshot,
    older_page_cursor: Option<String>,
    sender: broadcast::Sender<SessionPresentationEventEnvelope>,
    assistant_delta_sequences: BTreeMap<MessageId, u64>,
}

#[derive(Clone)]
pub struct ApplicationService {
    daemon_instance_id: DaemonInstanceId,
    backend: Arc<dyn ApplicationBackend>,
    blocking_bridge: Arc<Semaphore>,
    projections: Arc<Mutex<BTreeMap<SessionId, ProjectionState>>>,
}

impl ApplicationService {
    pub fn new(daemon_instance_id: DaemonInstanceId, backend: Arc<dyn ApplicationBackend>) -> Self {
        Self {
            daemon_instance_id,
            backend,
            blocking_bridge: Arc::new(Semaphore::new(BLOCKING_BRIDGE_CAPACITY)),
            projections: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn daemon_instance_id(&self) -> &DaemonInstanceId {
        &self.daemon_instance_id
    }

    #[cfg(test)]
    pub(crate) fn available_blocking_permits(&self) -> usize {
        self.blocking_bridge.available_permits()
    }

    pub async fn open_session(
        &self,
        request: SessionOpen,
    ) -> Result<SessionOpened, ApplicationError> {
        request.validate()?;
        let backend = Arc::clone(&self.backend);
        let mut opened = self
            .blocking(move |context| backend.open_session(context, request))
            .await?;
        let page = self.load_snapshot_page(&opened.session_id, None).await?;
        opened.snapshot = self.install_snapshot(page)?;
        Ok(opened)
    }

    pub async fn command_catalog(
        &self,
        context: CommandContext,
    ) -> Result<CommandCatalog, ApplicationError> {
        let catalog = shared_command_catalog(&context);
        catalog.validate()?;
        Ok(catalog)
    }

    pub async fn command_suggestions(
        &self,
        request: SuggestionRequest,
    ) -> Result<SuggestionPage, ApplicationError> {
        request.validate()?;
        let backend = Arc::clone(&self.backend);
        self.blocking(move |context| backend.suggestions(context, request))
            .await
            .map(SuggestionPage::validate)
    }

    pub async fn snapshot(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionPresentationSnapshot, ApplicationError> {
        let page = self.load_snapshot_page(session_id, None).await?;
        self.install_snapshot(page)
    }

    pub async fn snapshot_page(
        &self,
        session_id: &SessionId,
        page_cursor: Option<String>,
    ) -> Result<PresentationSnapshotPage, ApplicationError> {
        validate_page_cursor(page_cursor.as_deref())?;
        self.load_snapshot_page(session_id, page_cursor).await
    }

    pub async fn invoke(
        &self,
        request: ApplicationActionRequest,
    ) -> Result<ApplicationActionResult, ApplicationError> {
        request.validate()?;
        let session_id = request.session_id.clone();
        let backend = Arc::clone(&self.backend);
        let result = self
            .blocking(move |context| backend.invoke(context, request))
            .await?;
        if let Some(session_id) = session_id {
            if matches!(&result, ApplicationActionResult::SessionExited { .. }) {
                self.finish_session_projection(&session_id).await?;
            } else {
                self.refresh(&session_id).await?;
            }
        }
        Ok(result)
    }

    /// Publishes the terminal projection boundary shared by every client of a
    /// durable session. The explicit event lets streaming adapters close with
    /// `session_finished` instead of leaving peer subscribers waiting forever.
    pub async fn finish_session_projection(
        &self,
        session_id: &SessionId,
    ) -> Result<(), ApplicationError> {
        self.refresh(session_id).await?;
        self.publish(session_id, SessionPresentationEvent::SessionFinished)?;
        Ok(())
    }

    pub async fn submit_prompt(
        &self,
        request: PromptSubmission,
    ) -> Result<PromptAdmission, ApplicationError> {
        request.validate()?;
        let session_id = request.session_id.clone();
        let backend = Arc::clone(&self.backend);
        let admission = self
            .blocking(move |context| backend.submit_prompt(context, request))
            .await?;
        self.refresh(&session_id).await?;
        let event = match admission.state {
            PromptAdmissionState::Queued | PromptAdmissionState::Waiting => {
                SessionPresentationEvent::PromptQueued {
                    prompt: crate::QueuedPromptView {
                        run_id: admission.run_id.clone(),
                        ordinal: admission.ordinal,
                    },
                }
            }
            PromptAdmissionState::Running => SessionPresentationEvent::PromptActivated {
                run_id: admission.run_id.clone(),
            },
            PromptAdmissionState::Succeeded
            | PromptAdmissionState::Failed
            | PromptAdmissionState::Cancelled => SessionPresentationEvent::PromptFinished {
                run_id: admission.run_id.clone(),
                state: admission.state.as_str().to_owned(),
            },
        };
        self.publish(&session_id, event)?;
        Ok(admission)
    }

    pub async fn ensure_human_terminal(
        &self,
        request: HumanTerminalEnsure,
    ) -> Result<TerminalEnsured, ApplicationError> {
        request.validate()?;
        let session_id = request.session_id.clone();
        let backend = Arc::clone(&self.backend);
        let ensured = self
            .blocking(move |context| backend.ensure_human_terminal(context, request))
            .await?;
        ensured.validate_for_session(&session_id)?;
        self.refresh(&session_id).await?;
        Ok(ensured)
    }

    pub async fn refresh(&self, session_id: &SessionId) -> Result<(), ApplicationError> {
        let PresentationSnapshotPage {
            mut snapshot,
            older_page_cursor,
        } = self.load_snapshot_page(session_id, None).await?;
        snapshot.cursor.daemon_instance_id = self.daemon_instance_id.clone();
        snapshot.validate()?;
        let mut projections = self.projections.lock().map_err(lock_error)?;
        let Some(projection) = projections.get_mut(session_id) else {
            let (sender, _) = broadcast::channel(PRESENTATION_CHANNEL_CAPACITY);
            projections.insert(
                session_id.clone(),
                ProjectionState {
                    snapshot,
                    older_page_cursor,
                    sender,
                    assistant_delta_sequences: BTreeMap::new(),
                },
            );
            return Ok(());
        };
        let revision = projection.snapshot.cursor.revision.saturating_add(1);
        merge_provisional_items(projection, &mut snapshot);
        snapshot.cursor.revision = revision;
        snapshot.validate()?;
        projection.snapshot = snapshot.clone();
        projection.older_page_cursor = older_page_cursor.clone();
        retain_live_delta_sequences(projection);
        let envelope = SessionPresentationEventEnvelope {
            event_id: EventId::generate(),
            session_id: session_id.clone(),
            cursor: snapshot.cursor.clone(),
            event: SessionPresentationEvent::SnapshotReplaced {
                snapshot: Box::new(snapshot),
                older_page_cursor,
            },
        };
        let _ = projection.sender.send(envelope);
        Ok(())
    }

    pub async fn subscribe(
        &self,
        request: PresentationSubscribe,
    ) -> Result<PresentationSubscription, ApplicationError> {
        self.snapshot(&request.session_id).await?;
        let projections = self.projections.lock().map_err(lock_error)?;
        let projection = projections.get(&request.session_id).ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::NotFound,
                "session projection not found",
            )
        })?;
        Ok(PresentationSubscription {
            snapshot: projection.snapshot.clone(),
            older_page_cursor: projection.older_page_cursor.clone(),
            receiver: projection.sender.subscribe(),
            expected_revision: projection.snapshot.cursor.revision + 1,
        })
    }

    pub fn publish(
        &self,
        session_id: &SessionId,
        event: SessionPresentationEvent,
    ) -> Result<SessionPresentationEventEnvelope, ApplicationError> {
        validate_event(session_id, &event)?;
        let mut projections = self.projections.lock().map_err(lock_error)?;
        let projection = projections.get_mut(session_id).ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::NotFound,
                "session projection not found",
            )
        })?;
        let (envelope, _) = publish_to_projection(session_id, event, projection)?;
        Ok(envelope)
    }

    pub(crate) fn try_publish_batch_nonblocking(
        &self,
        session_id: &SessionId,
        events: impl IntoIterator<Item = SessionPresentationEvent>,
    ) -> Result<bool, ApplicationError> {
        let events = events.into_iter().collect::<Vec<_>>();
        if events.is_empty() {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "presentation event batch must not be empty",
            ));
        }
        for event in &events {
            validate_event(session_id, event)?;
        }
        let mut projections = self.projections.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => ApplicationError::new(
                ApplicationErrorCode::InputBackpressure,
                "application projection is busy",
            ),
            std::sync::TryLockError::Poisoned(error) => lock_error(error),
        })?;
        let projection = projections.get_mut(session_id).ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::NotFound,
                "session projection not found",
            )
        })?;
        let mut delivered = true;
        for event in events {
            let (_, event_delivered) = publish_to_projection(session_id, event, projection)?;
            delivered &= event_delivered;
        }
        Ok(delivered)
    }

    fn install_snapshot(
        &self,
        page: PresentationSnapshotPage,
    ) -> Result<SessionPresentationSnapshot, ApplicationError> {
        let PresentationSnapshotPage {
            mut snapshot,
            older_page_cursor,
        } = page.validate()?;
        snapshot.cursor.daemon_instance_id = self.daemon_instance_id.clone();
        snapshot.validate()?;
        let mut projections = self.projections.lock().map_err(lock_error)?;
        let session_id = snapshot.session_id.clone();
        match projections.entry(snapshot.session_id.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                let (sender, _) = broadcast::channel(PRESENTATION_CHANNEL_CAPACITY);
                entry.insert(ProjectionState {
                    snapshot,
                    older_page_cursor,
                    sender,
                    assistant_delta_sequences: BTreeMap::new(),
                });
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let projection = entry.get_mut();
                snapshot.cursor.revision = projection.snapshot.cursor.revision;
                merge_provisional_items(projection, &mut snapshot);
                snapshot.validate()?;
                if projection.snapshot != snapshot
                    || projection.older_page_cursor != older_page_cursor
                {
                    snapshot.cursor.revision =
                        projection.snapshot.cursor.revision.saturating_add(1);
                    projection.snapshot = snapshot.clone();
                    projection.older_page_cursor = older_page_cursor.clone();
                    retain_live_delta_sequences(projection);
                    let envelope = SessionPresentationEventEnvelope {
                        event_id: EventId::generate(),
                        session_id: session_id.clone(),
                        cursor: snapshot.cursor.clone(),
                        event: SessionPresentationEvent::SnapshotReplaced {
                            snapshot: Box::new(snapshot),
                            older_page_cursor,
                        },
                    };
                    let _ = projection.sender.send(envelope);
                }
            }
        }
        Ok(projections
            .get(&session_id)
            .expect("installed projection must remain present")
            .snapshot
            .clone())
    }

    async fn load_snapshot_page(
        &self,
        session_id: &SessionId,
        page_cursor: Option<String>,
    ) -> Result<PresentationSnapshotPage, ApplicationError> {
        let backend = Arc::clone(&self.backend);
        let session_id = session_id.clone();
        self.blocking(move |context| {
            backend.snapshot_page(context, &session_id, page_cursor.as_deref())
        })
        .await?
        .validate()
    }

    async fn blocking<T: Send + 'static>(
        &self,
        operation: impl FnOnce(ApplicationCallContext) -> Result<T, ApplicationError> + Send + 'static,
    ) -> Result<T, ApplicationError> {
        let permit = Arc::clone(&self.blocking_bridge)
            .try_acquire_owned()
            .map_err(|_| {
                ApplicationError::new(
                    ApplicationErrorCode::InputBackpressure,
                    "application blocking bridge is full",
                )
            })?;
        run_blocking(permit, operation).await
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentationSubscribe {
    pub session_id: SessionId,
}

pub struct PresentationSubscription {
    pub snapshot: SessionPresentationSnapshot,
    pub older_page_cursor: Option<String>,
    receiver: broadcast::Receiver<SessionPresentationEventEnvelope>,
    expected_revision: u64,
}

impl PresentationSubscription {
    pub async fn next(&mut self) -> Result<SessionPresentationEventEnvelope, ApplicationError> {
        let event = self.receiver.recv().await.map_err(|error| match error {
            broadcast::error::RecvError::Lagged(_) => ApplicationError::new(
                ApplicationErrorCode::ResyncRequired,
                "presentation subscriber lagged and must request a new snapshot",
            ),
            broadcast::error::RecvError::Closed => ApplicationError::new(
                ApplicationErrorCode::OutcomeUnknown,
                "presentation subscription closed",
            ),
        })?;
        if event.cursor.daemon_instance_id != self.snapshot.cursor.daemon_instance_id
            || event.cursor.revision != self.expected_revision
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::ResyncRequired,
                "presentation epoch or revision is not contiguous",
            ));
        }
        self.expected_revision = self.expected_revision.saturating_add(1);
        Ok(event)
    }
}

async fn run_blocking<T: Send + 'static>(
    permit: OwnedSemaphorePermit,
    operation: impl FnOnce(ApplicationCallContext) -> Result<T, ApplicationError> + Send + 'static,
) -> Result<T, ApplicationError> {
    let context = ApplicationCallContext::new();
    let cancellation = CancelOnDrop {
        context: context.clone(),
        armed: true,
    };
    let task = tokio::task::spawn_blocking(move || {
        // The permit belongs to the actual blocking operation, not to the
        // awaiter. Aborting the awaiter therefore cannot admit an unbounded
        // tail of detached blocking tasks.
        let _permit = permit;
        operation(context)
    });
    let result = task.await.map_err(|_| {
        ApplicationError::new(
            ApplicationErrorCode::Internal,
            "application owner task failed",
        )
    })?;
    cancellation.disarm();
    result
}

struct CancelOnDrop {
    context: ApplicationCallContext,
    armed: bool,
}

impl CancelOnDrop {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.context.cancel();
        }
    }
}

fn lock_error<T>(_error: std::sync::PoisonError<T>) -> ApplicationError {
    ApplicationError::new(
        ApplicationErrorCode::Internal,
        "application projection lock poisoned",
    )
}

fn validate_submission_id(value: &str) -> Result<(), ApplicationError> {
    if value.is_empty() || value.len() > 256 || value.contains(['\0', '\n', '\r']) {
        return Err(ApplicationError::new(
            ApplicationErrorCode::InvalidArguments,
            "client submission ID must be nonempty bounded single-line text",
        ));
    }
    Ok(())
}

fn validate_page_cursor(page_cursor: Option<&str>) -> Result<(), ApplicationError> {
    let Some(page_cursor) = page_cursor else {
        return Ok(());
    };
    if page_cursor.is_empty()
        || page_cursor.len() > MAX_PRESENTATION_PAGE_CURSOR_BYTES
        || page_cursor.chars().any(char::is_control)
    {
        return Err(ApplicationError::new(
            ApplicationErrorCode::InvalidArguments,
            "presentation page cursor must be nonempty bounded text without controls",
        ));
    }
    Ok(())
}

fn publish_to_projection(
    session_id: &SessionId,
    event: SessionPresentationEvent,
    projection: &mut ProjectionState,
) -> Result<(SessionPresentationEventEnvelope, bool), ApplicationError> {
    apply_event(projection, &event)?;
    projection.snapshot.cursor.revision = projection.snapshot.cursor.revision.saturating_add(1);
    let envelope = SessionPresentationEventEnvelope {
        event_id: EventId::generate(),
        session_id: session_id.clone(),
        cursor: projection.snapshot.cursor.clone(),
        event,
    };
    let delivered = projection.sender.send(envelope.clone()).is_ok();
    Ok((envelope, delivered))
}

fn merge_provisional_items(
    projection: &ProjectionState,
    snapshot: &mut SessionPresentationSnapshot,
) {
    for item in &projection.snapshot.items {
        if !matches!(
            item,
            SessionPresentationItem::AssistantMessage {
                state: crate::AssistantItemState::Streaming,
                ..
            }
        ) {
            continue;
        }
        let key = item.key();
        if snapshot.items.iter().all(|existing| existing.key() != key)
            && snapshot.items.len() < crate::MAX_PRESENTATION_ITEMS
        {
            snapshot.items.push(item.clone());
        }
    }
}

fn retain_live_delta_sequences(projection: &mut ProjectionState) {
    let live = projection
        .snapshot
        .items
        .iter()
        .filter_map(|item| match item {
            SessionPresentationItem::AssistantMessage {
                message_id,
                state: crate::AssistantItemState::Streaming,
                ..
            } => Some(message_id.clone()),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    projection
        .assistant_delta_sequences
        .retain(|message_id, _| live.contains(message_id));
}

fn apply_event(
    projection: &mut ProjectionState,
    event: &SessionPresentationEvent,
) -> Result<(), ApplicationError> {
    let snapshot = &mut projection.snapshot;
    match event {
        SessionPresentationEvent::SnapshotReplaced {
            snapshot: replacement,
            older_page_cursor,
        } => {
            let cursor = snapshot.cursor.clone();
            *snapshot = replacement.as_ref().clone();
            snapshot.cursor = cursor;
            projection.older_page_cursor = older_page_cursor.clone();
            projection.assistant_delta_sequences.clear();
        }
        SessionPresentationEvent::HeaderChanged { header } => snapshot.header = header.clone(),
        SessionPresentationEvent::ItemUpsert { item } => {
            let key = item.key();
            if let SessionPresentationItem::AssistantMessage { message_id, .. } = item {
                projection.assistant_delta_sequences.remove(message_id);
            }
            if let Some(existing) = snapshot
                .items
                .iter_mut()
                .find(|existing| existing.key() == key)
            {
                *existing = item.clone();
            } else {
                if snapshot.items.len() >= crate::MAX_PRESENTATION_ITEMS {
                    return Err(ApplicationError::new(
                        ApplicationErrorCode::InputBackpressure,
                        "session presentation item limit reached",
                    ));
                }
                snapshot.items.push(item.clone());
            }
        }
        SessionPresentationEvent::ItemRemoved { item_key } => {
            if let Some(message_id) = snapshot.items.iter().find_map(|item| match item {
                SessionPresentationItem::AssistantMessage { message_id, .. }
                    if item.key() == *item_key =>
                {
                    Some(message_id.clone())
                }
                _ => None,
            }) {
                projection.assistant_delta_sequences.remove(&message_id);
            }
            snapshot.items.retain(|item| item.key() != *item_key);
        }
        SessionPresentationEvent::PromptQueued { prompt } => {
            if let Some(existing) = snapshot
                .queued_prompts
                .iter_mut()
                .find(|existing| existing.run_id == prompt.run_id)
            {
                *existing = prompt.clone();
            } else {
                snapshot.queued_prompts.push(prompt.clone());
            }
            sync_prompt_counts(snapshot);
        }
        SessionPresentationEvent::PromptActivated { run_id } => {
            snapshot
                .queued_prompts
                .retain(|prompt| &prompt.run_id != run_id);
            if snapshot
                .active_run
                .as_ref()
                .is_none_or(|active| &active.run_id != run_id)
            {
                snapshot.active_run = Some(crate::ActiveRunView {
                    run_id: run_id.clone(),
                    turn_id: None,
                    state: "running".to_owned(),
                });
            }
            sync_prompt_counts(snapshot);
        }
        SessionPresentationEvent::TerminalAdded { terminal } => {
            if snapshot
                .terminals
                .iter()
                .all(|existing| existing.terminal_id != terminal.terminal_id)
            {
                snapshot.terminals.push(terminal.clone());
            }
        }
        SessionPresentationEvent::TerminalChanged { terminal } => {
            if let Some(existing) = snapshot
                .terminals
                .iter_mut()
                .find(|existing| existing.terminal_id == terminal.terminal_id)
            {
                *existing = terminal.clone();
            } else {
                snapshot.terminals.push(terminal.clone());
            }
        }
        SessionPresentationEvent::TerminalRemoved { terminal_id } => {
            snapshot
                .terminals
                .retain(|terminal| &terminal.terminal_id != terminal_id);
        }
        SessionPresentationEvent::ExecutionStateChanged { execution } => {
            if let Some(existing) = snapshot
                .executions
                .iter_mut()
                .find(|existing| existing.execution_id == execution.execution_id)
            {
                *existing = execution.clone();
            } else {
                snapshot.executions.push(execution.clone());
            }
        }
        SessionPresentationEvent::SessionFinished => {
            snapshot.header.status = crate::SessionPresentationStatus::Finished;
        }
        SessionPresentationEvent::AssistantTextDelta {
            provisional_message_id,
            sequence,
            text,
            ..
        } => {
            let expected = projection
                .assistant_delta_sequences
                .get(provisional_message_id)
                .map_or(1, |previous| previous.saturating_add(1));
            if *sequence != expected {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::ResyncRequired,
                    format!(
                        "assistant delta sequence is not contiguous: expected {expected}, got {sequence}"
                    ),
                ));
            }
            append_assistant_delta(snapshot, provisional_message_id, text)?;
            projection
                .assistant_delta_sequences
                .insert(provisional_message_id.clone(), *sequence);
        }
        SessionPresentationEvent::PromptFinished { run_id, .. } => {
            if snapshot
                .active_run
                .as_ref()
                .is_some_and(|active| &active.run_id == run_id)
            {
                snapshot.active_run = None;
            }
            snapshot
                .queued_prompts
                .retain(|prompt| &prompt.run_id != run_id);
            sync_prompt_counts(snapshot);
        }
        SessionPresentationEvent::TerminalCommandStarted { .. }
        | SessionPresentationEvent::TerminalCommandFinished { .. }
        | SessionPresentationEvent::CommandAvailabilityChanged
        | SessionPresentationEvent::Notice { .. } => {}
    }
    Ok(())
}

fn sync_prompt_counts(snapshot: &mut SessionPresentationSnapshot) {
    snapshot.header.active_run_count = u32::from(snapshot.active_run.is_some());
    snapshot.header.queued_prompt_count =
        u32::try_from(snapshot.queued_prompts.len()).unwrap_or(u32::MAX);
    snapshot.command_context.active_or_queued_turns = snapshot
        .header
        .active_run_count
        .saturating_add(snapshot.header.queued_prompt_count);
}

fn append_assistant_delta(
    snapshot: &mut SessionPresentationSnapshot,
    message_id: &MessageId,
    delta: &str,
) -> Result<(), ApplicationError> {
    let existing = snapshot
        .items
        .iter_mut()
        .find(|item| item.key() == message_id.as_str());
    match existing {
        Some(SessionPresentationItem::AssistantMessage { content, state, .. }) => {
            if *state != crate::AssistantItemState::Streaming {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::ResyncRequired,
                    "assistant delta arrived after the item became terminal",
                ));
            }
            let mut text = content.text_only().ok_or_else(|| {
                ApplicationError::new(
                    ApplicationErrorCode::ResyncRequired,
                    "streaming assistant item contains non-text content",
                )
            })?;
            text.push_str(delta);
            *content = Content::text(text).map_err(|error| {
                ApplicationError::new(ApplicationErrorCode::InputBackpressure, error.to_string())
            })?;
        }
        Some(_) => {
            return Err(ApplicationError::new(
                ApplicationErrorCode::ResyncRequired,
                "assistant delta message ID collides with another item",
            ));
        }
        None => {
            if snapshot.items.len() >= crate::MAX_PRESENTATION_ITEMS {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InputBackpressure,
                    "session presentation item limit reached",
                ));
            }
            snapshot
                .items
                .push(SessionPresentationItem::AssistantMessage {
                    message_id: message_id.clone(),
                    content: Content::text(delta).map_err(|error| {
                        ApplicationError::new(
                            ApplicationErrorCode::InvalidArguments,
                            error.to_string(),
                        )
                    })?,
                    state: crate::AssistantItemState::Streaming,
                });
        }
    }
    Ok(())
}

fn validate_event(
    session_id: &SessionId,
    event: &SessionPresentationEvent,
) -> Result<(), ApplicationError> {
    match event {
        SessionPresentationEvent::SnapshotReplaced {
            snapshot,
            older_page_cursor,
        } => {
            snapshot.validate()?;
            if &snapshot.session_id != session_id {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "replacement snapshot belongs to a different session",
                ));
            }
            validate_page_cursor(older_page_cursor.as_deref())
        }
        SessionPresentationEvent::HeaderChanged { header } if &header.session_id != session_id => {
            Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "presentation header belongs to a different session",
            ))
        }
        SessionPresentationEvent::TerminalAdded { terminal }
        | SessionPresentationEvent::TerminalChanged { terminal } => {
            terminal.validate_for_session(session_id)
        }
        SessionPresentationEvent::ExecutionStateChanged { execution } => execution.validate(),
        SessionPresentationEvent::AssistantTextDelta { text, .. } if text.len() > 16 * 1024 => {
            Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "assistant text delta exceeds its byte bound",
            ))
        }
        SessionPresentationEvent::TerminalCommandFinished { cwd, .. }
            if cwd.is_empty()
                || cwd.len() > crate::MAX_TERMINAL_PATH_BYTES
                || cwd.contains('\0') =>
        {
            Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "terminal command cwd must be nonempty bounded text without NUL",
            ))
        }
        SessionPresentationEvent::Notice { code, message, .. }
            if code.is_empty() || code.len() > 8 * 1024 || message.len() > 8 * 1024 =>
        {
            Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "presentation notice exceeds its byte bound",
            ))
        }
        _ => Ok(()),
    }
}
