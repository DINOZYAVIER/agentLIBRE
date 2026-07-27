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
    ApplicationActionRequest, CommandCatalog, CommandContext, ExecutionView, HumanCommandCardState,
    HumanCommandCardView, HumanTerminalCommandAccepted, HumanTerminalCommandSubmit,
    HumanTerminalEnsure, PresentationCursor, SessionHeader, SessionLaunchOptions,
    SessionPresentationEvent, SessionPresentationEventEnvelope, SessionPresentationItem,
    SessionPresentationSnapshot, SuggestionPage, SuggestionRequest, TerminalEnsured,
    TerminalSessionView, shared_command_catalog,
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
    IncompleteOutputNotFound,
    ContinuationAlreadyClaimed,
    StaleContinuationContext,
    InputBackpressure,
    ActivityCapacityExceeded,
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
            Self::IncompleteOutputNotFound => "incomplete_output_not_found",
            Self::ContinuationAlreadyClaimed => "continuation_already_claimed",
            Self::StaleContinuationContext => "stale_continuation_context",
            Self::InputBackpressure => "input_backpressure",
            Self::ActivityCapacityExceeded => "activity_capacity_exceeded",
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
    Incomplete,
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
            Self::Incomplete => "incomplete",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApplicationToolResult {
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
    IncompleteTurnContinued {
        admission: PromptAdmission,
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
    ) -> Result<ApplicationToolResult, ApplicationError>;
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
    fn submit_human_terminal_command(
        &self,
        context: ApplicationCallContext,
        request: HumanTerminalCommandSubmit,
    ) -> Result<HumanTerminalCommandAdmission, ApplicationError>;
    fn suggestions(
        &self,
        context: ApplicationCallContext,
        request: SuggestionRequest,
    ) -> Result<SuggestionPage, ApplicationError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HumanTerminalCommandAdmission {
    pub accepted: HumanTerminalCommandAccepted,
    pub card: HumanCommandCardView,
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
    next_activity_order_index: u64,
    last_activity_batch: Option<crate::ActivityGraphDeltaBatch>,
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
        if page_cursor.is_some() {
            return self.load_snapshot_page(session_id, page_cursor).await;
        }
        let page = self.load_snapshot_page(session_id, None).await?;
        self.install_snapshot(page)?;
        let projections = self.projections.lock().map_err(lock_error)?;
        let projection = projections.get(session_id).ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::NotFound,
                "session projection not found after latest-page installation",
            )
        })?;
        Ok(PresentationSnapshotPage {
            snapshot: projection.snapshot.clone(),
            older_page_cursor: projection.older_page_cursor.clone(),
        })
    }

    pub async fn invoke(
        &self,
        request: ApplicationActionRequest,
    ) -> Result<ApplicationToolResult, ApplicationError> {
        request.validate()?;
        let session_id = request.session_id.clone();
        let backend = Arc::clone(&self.backend);
        let result = self
            .blocking(move |context| backend.invoke(context, request))
            .await?;
        if let Some(session_id) = session_id {
            if matches!(&result, ApplicationToolResult::SessionExited { .. }) {
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
        // Install the live projection before the supervisor can emit its
        // first turn event. Durable sessions can reconstruct a missed event
        // from their transcript, but explicitly non-durable setup smokes
        // cannot, so admission must not race presentation registration.
        self.snapshot(&session_id).await?;
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
            | PromptAdmissionState::Incomplete
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

    pub async fn submit_human_terminal_command(
        &self,
        request: HumanTerminalCommandSubmit,
    ) -> Result<HumanTerminalCommandAccepted, ApplicationError> {
        request.validate()?;
        let session_id = request.session_id.clone();
        // Install the private same-epoch projection before the backend can
        // admit bytes and the terminal monitor can emit card updates.
        self.snapshot(&session_id).await?;
        let backend = Arc::clone(&self.backend);
        let admission = self
            .blocking(move |context| backend.submit_human_terminal_command(context, request))
            .await?;
        admission.card.validate()?;
        if admission.accepted.terminal_id != admission.card.terminal_id
            || admission.accepted.command_sequence != admission.card.command_sequence
        {
            return Err(ApplicationError::new(
                ApplicationErrorCode::Internal,
                "Human terminal command admission returned an inconsistent private card",
            ));
        }
        self.publish_admitted_human_command_card(&session_id, admission.card)?;
        Ok(admission.accepted)
    }

    fn publish_admitted_human_command_card(
        &self,
        session_id: &SessionId,
        card: HumanCommandCardView,
    ) -> Result<(), ApplicationError> {
        let mut projections = self.projections.lock().map_err(lock_error)?;
        let projection = projections.get_mut(session_id).ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::NotFound,
                "session projection not found",
            )
        })?;
        let superseded = projection
            .snapshot
            .human_commands
            .iter()
            .find(|existing| {
                existing.terminal_id == card.terminal_id
                    && existing.command_sequence == card.command_sequence
            })
            .is_some_and(|existing| {
                existing == &card
                    || existing.updated_at_unix_ms > card.updated_at_unix_ms
                    || human_command_card_state_rank(existing.state)
                        > human_command_card_state_rank(card.state)
            });
        if !superseded {
            publish_to_projection(
                session_id,
                SessionPresentationEvent::HumanCommandCardUpsert { card },
                projection,
            )?;
        }
        Ok(())
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
                    next_activity_order_index: 1,
                    last_activity_batch: None,
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

    /// Applies a bounded product event batch to the reconnectable projection.
    /// Live broadcast delivery is intentionally not part of success: a
    /// session with no attached client must still retain the latest activity
    /// and streaming state for its next snapshot.
    pub(crate) fn publish_batch(
        &self,
        session_id: &SessionId,
        events: impl IntoIterator<Item = SessionPresentationEvent>,
    ) -> Result<(), ApplicationError> {
        let events = events.into_iter().collect::<Vec<_>>();
        if events.is_empty() {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "presentation event batch must not be empty",
            ));
        }
        let mut projections = self.projections.lock().map_err(lock_error)?;
        let projection = projections.get_mut(session_id).ok_or_else(|| {
            ApplicationError::new(
                ApplicationErrorCode::NotFound,
                "session projection not found",
            )
        })?;
        for event in events {
            let _ = publish_to_projection(session_id, event, projection)?;
        }
        Ok(())
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
                    next_activity_order_index: 1,
                    last_activity_batch: None,
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

fn human_command_card_state_rank(state: HumanCommandCardState) -> u8 {
    match state {
        HumanCommandCardState::Starting => 0,
        HumanCommandCardState::Running => 1,
        HumanCommandCardState::Exited | HumanCommandCardState::OutcomeUnknown => 2,
    }
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
    mut event: SessionPresentationEvent,
    projection: &mut ProjectionState,
) -> Result<(SessionPresentationEventEnvelope, bool), ApplicationError> {
    prepare_activity_event(projection, &mut event)?;
    validate_event(session_id, &event)?;
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
        if snapshot.header.durable
            && !matches!(
                item,
                SessionPresentationItem::AssistantMessage {
                    state: crate::AssistantItemState::Streaming,
                    ..
                }
            )
        {
            continue;
        }
        let key = item.key();
        if snapshot.items.iter().all(|existing| existing.key() != key)
            && snapshot.items.len() < crate::MAX_PRESENTATION_ITEMS
        {
            snapshot.items.push(item.clone());
        }
    }
    // Human command cards and the activity graph are volatile presentation
    // state owned by this daemon epoch. Backend snapshots deliberately do not
    // persist them, but a refresh must not erase them for connected or
    // reconnecting clients in the same epoch.
    snapshot.human_commands = projection.snapshot.human_commands.clone();
    snapshot.activity = projection.snapshot.activity.clone();
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

fn prepare_activity_event(
    projection: &mut ProjectionState,
    event: &mut SessionPresentationEvent,
) -> Result<(), ApplicationError> {
    let SessionPresentationEvent::ActivityGraphDelta { batch } = event else {
        return Ok(());
    };
    let current_revision = projection
        .snapshot
        .activity
        .as_ref()
        .map_or(0, |graph| graph.graph_revision);
    if batch.graph_revision == 0 {
        batch.graph_revision = current_revision.saturating_add(1);
    } else if batch.graph_revision == current_revision
        && projection.last_activity_batch.as_ref() == Some(batch)
    {
        return Ok(());
    } else if batch.graph_revision != current_revision.saturating_add(1) {
        return Err(ApplicationError::new(
            ApplicationErrorCode::ResyncRequired,
            format!(
                "activity graph revision is not contiguous: current {current_revision}, received {}",
                batch.graph_revision
            ),
        ));
    }

    let existing = projection
        .snapshot
        .activity
        .as_ref()
        .map(|graph| {
            graph
                .nodes
                .iter()
                .map(|node| (node.node_id.clone(), node.clone()))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    for node in &mut batch.upserts {
        if let Some(previous) = existing.get(&node.node_id) {
            if node.parent_node_id != previous.parent_node_id
                || node.run_id != previous.run_id
                || node.turn_id != previous.turn_id
                || node.attempt_id != previous.attempt_id
                || node.step_id != previous.step_id
                || node.kind != previous.kind
            {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "activity node identity and parent are immutable",
                ));
            }
            node.order_index = previous.order_index;
            node.retry = previous.retry;
            node.started_at_unix_ms = previous.started_at_unix_ms;
        } else {
            if node.kind == crate::ActivityNodeKind::Attempt {
                node.retry = u32::try_from(
                    existing
                        .values()
                        .filter(|candidate| {
                            candidate.kind == crate::ActivityNodeKind::Attempt
                                && candidate.turn_id == node.turn_id
                        })
                        .count(),
                )
                .unwrap_or(u32::MAX);
            }
            node.order_index = projection.next_activity_order_index;
            projection.next_activity_order_index = projection
                .next_activity_order_index
                .checked_add(1)
                .ok_or_else(|| {
                    ApplicationError::new(
                        ApplicationErrorCode::ActivityCapacityExceeded,
                        "activity order index exhausted",
                    )
                })?;
            if node.started_at_unix_ms == 0 {
                node.started_at_unix_ms = node.updated_at_unix_ms;
            }
        }
        node.updated_at_unix_ms = node.updated_at_unix_ms.max(node.started_at_unix_ms);
        if let Some(finished) = node.finished_at_unix_ms {
            node.updated_at_unix_ms = node.updated_at_unix_ms.max(finished);
            node.elapsed_ms =
                u64::try_from(finished.saturating_sub(node.started_at_unix_ms)).unwrap_or_default();
        }
    }
    sort_activity_upserts(&mut batch.upserts, &existing)?;

    let mut candidate = apply_activity_batch(projection.snapshot.activity.as_ref(), batch, false)?;
    let focused_path = select_activity_current_path(&candidate);
    if batch.current_path.as_ref() != Some(&focused_path) {
        batch.current_path = Some(focused_path);
        candidate = apply_activity_batch(projection.snapshot.activity.as_ref(), batch, true)?;
    }
    enforce_active_activity_capacity(&candidate)?;
    while activity_graph_exceeds_retention(&candidate)? {
        let reason = if candidate.nodes.len() > crate::MAX_ACTIVITY_NODES {
            crate::ActivityAggregateReason::NodeLimit
        } else {
            crate::ActivityAggregateReason::ByteLimit
        };
        match collapse_oldest_completed_branch(&candidate, reason) {
            Ok((aggregate, removal)) => {
                batch.upserts.push(aggregate);
                batch.removals.push(removal);
            }
            Err(error) => {
                let Some(removal) = retire_oldest_activity_aggregate(&candidate) else {
                    return Err(error);
                };
                batch.removals.push(removal);
            }
        }
        batch.truncated = true;
        sort_activity_upserts(&mut batch.upserts, &existing)?;
        candidate = apply_activity_batch(projection.snapshot.activity.as_ref(), batch, false)?;
    }
    candidate.validate()?;
    Ok(())
}

fn sort_activity_upserts(
    upserts: &mut Vec<crate::ActivityNodeView>,
    existing: &BTreeMap<String, crate::ActivityNodeView>,
) -> Result<(), ApplicationError> {
    let mut remaining = std::mem::take(upserts);
    let mut emitted = existing.keys().cloned().collect::<BTreeSet<_>>();
    while !remaining.is_empty() {
        let Some(index) = remaining
            .iter()
            .enumerate()
            .filter(|(_, node)| {
                node.parent_node_id
                    .as_ref()
                    .is_none_or(|parent| emitted.contains(parent))
            })
            .min_by(|(_, left), (_, right)| {
                (left.order_index, left.node_id.as_str())
                    .cmp(&(right.order_index, right.node_id.as_str()))
            })
            .map(|(index, _)| index)
        else {
            return Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "activity delta contains a cycle or missing parent",
            ));
        };
        let node = remaining.remove(index);
        emitted.insert(node.node_id.clone());
        upserts.push(node);
    }
    Ok(())
}

fn apply_activity_batch(
    current: Option<&crate::ActivityGraphView>,
    batch: &crate::ActivityGraphDeltaBatch,
    validate: bool,
) -> Result<crate::ActivityGraphView, ApplicationError> {
    let current_revision = current.map_or(0, |graph| graph.graph_revision);
    if batch.graph_revision != current_revision.saturating_add(1) {
        return Err(ApplicationError::new(
            ApplicationErrorCode::ResyncRequired,
            "activity delta revision is not contiguous with the installed graph",
        ));
    }
    let mut graph = current.cloned().unwrap_or(crate::ActivityGraphView {
        graph_revision: 0,
        roots: Vec::new(),
        nodes: Vec::new(),
        current_path: Vec::new(),
        truncated: false,
    });
    for node in &batch.upserts {
        if let Some(previous) = graph
            .nodes
            .iter_mut()
            .find(|previous| previous.node_id == node.node_id)
        {
            *previous = node.clone();
        } else {
            graph.nodes.push(node.clone());
        }
    }
    if let Some(path) = &batch.current_path {
        graph.current_path = path.clone();
    }
    for removal in &batch.removals {
        let target = graph
            .nodes
            .iter()
            .find(|node| node.node_id == removal.subtree_root_id)
            .ok_or_else(|| {
                ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "activity removal references an unknown subtree",
                )
            })?;
        match removal.reason {
            crate::ActivityRemovalReason::CollapsedIntoAggregate
                if !batch.upserts.iter().any(|replacement| {
                    replacement.kind == crate::ActivityNodeKind::Aggregate
                        && replacement.order_index == target.order_index
                        && replacement.parent_node_id == target.parent_node_id
                }) =>
            {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "collapsed activity subtree requires its typed aggregate replacement",
                ));
            }
            crate::ActivityRemovalReason::RetentionExpired
                if target.kind != crate::ActivityNodeKind::Aggregate =>
            {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "retention expiry may remove aggregate activity nodes only",
                ));
            }
            _ => {}
        }
        remove_completed_activity_subtree(&mut graph, removal)?;
    }
    graph.graph_revision = batch.graph_revision;
    graph.truncated |= batch.truncated;
    canonicalize_activity_graph(&mut graph)?;
    if validate
        && graph.nodes.len() <= crate::MAX_ACTIVITY_NODES
        && serde_json::to_vec(&graph)
            .map_err(|_| {
                ApplicationError::new(
                    ApplicationErrorCode::Internal,
                    "activity graph could not be encoded",
                )
            })?
            .len()
            <= crate::MAX_ACTIVITY_GRAPH_BYTES
    {
        graph.validate()?;
    }
    Ok(graph)
}

fn remove_completed_activity_subtree(
    graph: &mut crate::ActivityGraphView,
    removal: &crate::ActivityNodeRemoval,
) -> Result<(), ApplicationError> {
    if graph
        .nodes
        .iter()
        .all(|node| node.node_id != removal.subtree_root_id)
    {
        return Err(ApplicationError::new(
            ApplicationErrorCode::InvalidArguments,
            "activity removal references an unknown subtree",
        ));
    }
    let mut removed = BTreeSet::from([removal.subtree_root_id.clone()]);
    loop {
        let before = removed.len();
        for node in &graph.nodes {
            if node
                .parent_node_id
                .as_ref()
                .is_some_and(|parent| removed.contains(parent))
            {
                removed.insert(node.node_id.clone());
            }
        }
        if removed.len() == before {
            break;
        }
    }
    if graph.current_path.iter().any(|id| removed.contains(id))
        || graph
            .nodes
            .iter()
            .any(|node| removed.contains(&node.node_id) && !node.state.is_terminal())
    {
        return Err(ApplicationError::new(
            ApplicationErrorCode::InvalidArguments,
            "activity removal may target completed non-focused subtrees only",
        ));
    }
    graph.nodes.retain(|node| !removed.contains(&node.node_id));
    Ok(())
}

fn canonicalize_activity_graph(
    graph: &mut crate::ActivityGraphView,
) -> Result<(), ApplicationError> {
    let mut by_id = graph
        .nodes
        .drain(..)
        .map(|node| (node.node_id.clone(), node))
        .collect::<BTreeMap<_, _>>();
    let mut children = BTreeMap::<Option<String>, Vec<String>>::new();
    for node in by_id.values() {
        children
            .entry(node.parent_node_id.clone())
            .or_default()
            .push(node.node_id.clone());
    }
    for ids in children.values_mut() {
        ids.sort_by(|left, right| {
            let left = by_id.get(left).expect("activity child exists");
            let right = by_id.get(right).expect("activity child exists");
            (left.order_index, left.node_id.as_str())
                .cmp(&(right.order_index, right.node_id.as_str()))
        });
    }
    fn visit(
        parent: Option<String>,
        children: &BTreeMap<Option<String>, Vec<String>>,
        by_id: &mut BTreeMap<String, crate::ActivityNodeView>,
        output: &mut Vec<crate::ActivityNodeView>,
    ) {
        for id in children.get(&parent).into_iter().flatten() {
            let Some(node) = by_id.remove(id) else {
                continue;
            };
            output.push(node);
            visit(Some(id.clone()), children, by_id, output);
        }
    }
    visit(None, &children, &mut by_id, &mut graph.nodes);
    if !by_id.is_empty() {
        return Err(ApplicationError::new(
            ApplicationErrorCode::InvalidArguments,
            "activity graph contains a cycle or disconnected topology",
        ));
    }
    graph.roots = graph
        .nodes
        .iter()
        .filter(|node| node.parent_node_id.is_none())
        .map(|node| node.node_id.clone())
        .collect();
    Ok(())
}

fn enforce_active_activity_capacity(
    graph: &crate::ActivityGraphView,
) -> Result<(), ApplicationError> {
    let active = graph
        .nodes
        .iter()
        .filter(|node| {
            !node.state.is_terminal() || graph.current_path.iter().any(|id| id == &node.node_id)
        })
        .collect::<Vec<_>>();
    let bytes = serde_json::to_vec(&(&active, &graph.current_path))
        .map_err(|_| {
            ApplicationError::new(
                ApplicationErrorCode::Internal,
                "active activity topology could not be encoded",
            )
        })?
        .len();
    if active.len() > crate::MAX_ACTIVE_ACTIVITY_NODES || bytes > crate::MAX_ACTIVE_ACTIVITY_BYTES {
        return Err(ApplicationError::new(
            ApplicationErrorCode::ActivityCapacityExceeded,
            "active activity topology exceeds its reserved capacity",
        ));
    }
    Ok(())
}

fn select_activity_current_path(graph: &crate::ActivityGraphView) -> Vec<String> {
    let priority = |state: crate::ActivityNodeState| match state {
        crate::ActivityNodeState::Running => Some(0u8),
        crate::ActivityNodeState::Waiting => Some(1),
        crate::ActivityNodeState::Pending => Some(2),
        _ => None,
    };
    let depth = |node: &crate::ActivityNodeView| {
        let mut depth = 0usize;
        let mut parent = node.parent_node_id.as_deref();
        while let Some(parent_id) = parent {
            let Some(parent_node) = graph.nodes.iter().find(|node| node.node_id == parent_id)
            else {
                break;
            };
            depth = depth.saturating_add(1);
            parent = parent_node.parent_node_id.as_deref();
        }
        depth
    };
    let mut leaves = graph
        .nodes
        .iter()
        .filter(|node| priority(node.state).is_some())
        .filter(|node| {
            graph.nodes.iter().all(|child| {
                child.parent_node_id.as_deref() != Some(node.node_id.as_str())
                    || priority(child.state).is_none()
            })
        })
        .collect::<Vec<_>>();
    leaves.sort_by(|left, right| {
        priority(left.state)
            .cmp(&priority(right.state))
            .then_with(|| right.updated_at_unix_ms.cmp(&left.updated_at_unix_ms))
            .then_with(|| depth(right).cmp(&depth(left)))
            .then_with(|| left.order_index.cmp(&right.order_index))
            .then_with(|| left.node_id.cmp(&right.node_id))
    });
    let Some(leaf) = leaves.first() else {
        return Vec::new();
    };
    let mut path = vec![leaf.node_id.clone()];
    let mut parent = leaf.parent_node_id.as_deref();
    while let Some(parent_id) = parent {
        let Some(parent_node) = graph.nodes.iter().find(|node| node.node_id == parent_id) else {
            return Vec::new();
        };
        path.push(parent_node.node_id.clone());
        parent = parent_node.parent_node_id.as_deref();
    }
    path.reverse();
    path
}

fn activity_graph_exceeds_retention(
    graph: &crate::ActivityGraphView,
) -> Result<bool, ApplicationError> {
    let bytes = serde_json::to_vec(graph)
        .map_err(|_| {
            ApplicationError::new(
                ApplicationErrorCode::Internal,
                "activity graph could not be encoded",
            )
        })?
        .len();
    Ok(graph.nodes.len() > crate::MAX_ACTIVITY_NODES || bytes > crate::MAX_ACTIVITY_GRAPH_BYTES)
}

fn collapse_oldest_completed_branch(
    graph: &crate::ActivityGraphView,
    reason: crate::ActivityAggregateReason,
) -> Result<(crate::ActivityNodeView, crate::ActivityNodeRemoval), ApplicationError> {
    let mut candidates = graph
        .nodes
        .iter()
        .filter(|root| root.kind != crate::ActivityNodeKind::Aggregate && root.state.is_terminal())
        .filter(|root| {
            root.parent_node_id.as_ref().is_none_or(|parent_id| {
                graph
                    .nodes
                    .iter()
                    .find(|node| &node.node_id == parent_id)
                    .is_some_and(|parent| !parent.state.is_terminal())
            })
        })
        .filter_map(|root| {
            let subtree = activity_subtree(graph, &root.node_id);
            subtree
                .iter()
                .all(|node| node.state.is_terminal())
                .then_some((root, subtree))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|(left, _), (right, _)| {
        (left.order_index, left.node_id.as_str()).cmp(&(right.order_index, right.node_id.as_str()))
    });
    let Some((root, subtree)) = candidates.into_iter().next() else {
        return Err(ApplicationError::new(
            ApplicationErrorCode::ActivityCapacityExceeded,
            "activity retention is full and no completed root can be collapsed",
        ));
    };
    let count = |state| {
        u32::try_from(subtree.iter().filter(|node| node.state == state).count()).unwrap_or(u32::MAX)
    };
    let finished = subtree
        .iter()
        .filter_map(|node| node.finished_at_unix_ms)
        .max()
        .unwrap_or(root.updated_at_unix_ms);
    let aggregate = crate::ActivityNodeView {
        node_id: format!("aggregate:{}:{}", root.run_id, root.order_index),
        parent_node_id: root.parent_node_id.clone(),
        order_index: root.order_index,
        run_id: root.run_id.clone(),
        turn_id: root.turn_id.clone(),
        attempt_id: None,
        step_id: None,
        kind: crate::ActivityNodeKind::Aggregate,
        phase: crate::ActivityPhase::Retention,
        state: crate::ActivityNodeState::Truncated,
        retry: 0,
        started_at_unix_ms: root.started_at_unix_ms,
        updated_at_unix_ms: finished,
        finished_at_unix_ms: Some(finished),
        elapsed_ms: u64::try_from(finished.saturating_sub(root.started_at_unix_ms))
            .unwrap_or_default(),
        summary: format!("{} completed activity nodes collapsed", subtree.len()),
        detail: crate::ActivityDetailView::Aggregate(crate::ActivityAggregateDetail {
            collapsed_nodes: u32::try_from(subtree.len()).unwrap_or(u32::MAX),
            succeeded: count(crate::ActivityNodeState::Succeeded),
            failed: count(crate::ActivityNodeState::Failed),
            cancelled: count(crate::ActivityNodeState::Cancelled),
            incomplete: count(crate::ActivityNodeState::Incomplete),
            elapsed_ms: subtree
                .iter()
                .fold(0u64, |sum, node| sum.saturating_add(node.elapsed_ms)),
            reason,
        }),
    };
    Ok((
        aggregate,
        crate::ActivityNodeRemoval {
            subtree_root_id: root.node_id.clone(),
            reason: crate::ActivityRemovalReason::CollapsedIntoAggregate,
        },
    ))
}

fn retire_oldest_activity_aggregate(
    graph: &crate::ActivityGraphView,
) -> Option<crate::ActivityNodeRemoval> {
    graph
        .nodes
        .iter()
        .filter(|node| node.kind == crate::ActivityNodeKind::Aggregate)
        .min_by(|left, right| {
            (left.order_index, left.node_id.as_str())
                .cmp(&(right.order_index, right.node_id.as_str()))
        })
        .map(|node| crate::ActivityNodeRemoval {
            subtree_root_id: node.node_id.clone(),
            reason: crate::ActivityRemovalReason::RetentionExpired,
        })
}

fn activity_subtree<'a>(
    graph: &'a crate::ActivityGraphView,
    root_id: &str,
) -> Vec<&'a crate::ActivityNodeView> {
    let mut ids = BTreeSet::from([root_id.to_owned()]);
    loop {
        let before = ids.len();
        for node in &graph.nodes {
            if node
                .parent_node_id
                .as_ref()
                .is_some_and(|parent| ids.contains(parent))
            {
                ids.insert(node.node_id.clone());
            }
        }
        if ids.len() == before {
            break;
        }
    }
    graph
        .nodes
        .iter()
        .filter(|node| ids.contains(&node.node_id))
        .collect()
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
            projection.next_activity_order_index = snapshot
                .activity
                .as_ref()
                .and_then(|graph| graph.nodes.iter().map(|node| node.order_index).max())
                .unwrap_or(0)
                .saturating_add(1);
            projection.last_activity_batch = None;
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
        SessionPresentationEvent::HumanCommandCardUpsert { card } => {
            if let Some(existing) = snapshot.human_commands.iter_mut().find(|existing| {
                existing.terminal_id == card.terminal_id
                    && existing.command_sequence == card.command_sequence
            }) {
                *existing = card.clone();
            } else {
                if snapshot.human_commands.len() >= crate::MAX_HUMAN_COMMAND_CARDS {
                    let Some(position) = snapshot.human_commands.iter().position(|card| {
                        !matches!(
                            card.state,
                            crate::HumanCommandCardState::Starting
                                | crate::HumanCommandCardState::Running
                        )
                    }) else {
                        return Err(ApplicationError::new(
                            ApplicationErrorCode::InputBackpressure,
                            "all retained Human command cards are active",
                        ));
                    };
                    snapshot.human_commands.remove(position);
                }
                snapshot.human_commands.push(card.clone());
            }
            while snapshot
                .human_commands
                .iter()
                .map(|card| card.output.as_str().len())
                .sum::<usize>()
                > crate::MAX_HUMAN_COMMAND_AGGREGATE_OUTPUT_BYTES
            {
                let Some(position) = snapshot.human_commands.iter().position(|card| {
                    !matches!(
                        card.state,
                        crate::HumanCommandCardState::Starting
                            | crate::HumanCommandCardState::Running
                    )
                }) else {
                    return Err(ApplicationError::new(
                        ApplicationErrorCode::InputBackpressure,
                        "active Human command cards exceed the aggregate output bound",
                    ));
                };
                snapshot.human_commands.remove(position);
            }
        }
        SessionPresentationEvent::HumanCommandCardRemoved {
            terminal_id,
            command_sequence,
        } => snapshot.human_commands.retain(|card| {
            &card.terminal_id != terminal_id || card.command_sequence != *command_sequence
        }),
        SessionPresentationEvent::ActivityGraphDelta { batch } => {
            if projection.last_activity_batch.as_ref() == Some(batch) {
                return Ok(());
            }
            let graph = apply_activity_batch(snapshot.activity.as_ref(), batch, true)?;
            snapshot.activity = Some(graph);
            projection.last_activity_batch = Some(batch.clone());
        }
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
        SessionPresentationEvent::HeaderChanged { header } => {
            if &header.session_id != session_id {
                return Err(ApplicationError::new(
                    ApplicationErrorCode::InvalidArguments,
                    "presentation header belongs to a different session",
                ));
            }
            header.workspace_root.validate()?;
            header.cwd.validate()?;
            crate::projection::validate_workspace_history_scope(&header.workspace_history_scope)
        }
        SessionPresentationEvent::TerminalAdded { terminal }
        | SessionPresentationEvent::TerminalChanged { terminal } => {
            terminal.validate_for_session(session_id)
        }
        SessionPresentationEvent::ExecutionStateChanged { execution } => execution.validate(),
        SessionPresentationEvent::HumanCommandCardUpsert { card } => card.validate(),
        SessionPresentationEvent::ActivityGraphDelta { batch } => batch.validate_shape(),
        SessionPresentationEvent::AssistantTextDelta { text, .. } if text.len() > 16 * 1024 => {
            Err(ApplicationError::new(
                ApplicationErrorCode::InvalidArguments,
                "assistant text delta exceeds its byte bound",
            ))
        }
        SessionPresentationEvent::TerminalCommandFinished { cwd, .. } => cwd.validate(),
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

#[cfg(test)]
mod activity_tests {
    use super::*;

    fn node(
        run_id: &RunId,
        node_id: String,
        parent_node_id: Option<String>,
        order_index: u64,
        state: crate::ActivityNodeState,
    ) -> crate::ActivityNodeView {
        let terminal = state.is_terminal();
        crate::ActivityNodeView {
            node_id,
            parent_node_id,
            order_index,
            run_id: run_id.clone(),
            turn_id: None,
            attempt_id: None,
            step_id: None,
            kind: if order_index == 1 {
                crate::ActivityNodeKind::Run
            } else {
                crate::ActivityNodeKind::Step
            },
            phase: crate::ActivityPhase::Tool,
            state,
            retry: 0,
            started_at_unix_ms: 1,
            updated_at_unix_ms: 5,
            finished_at_unix_ms: terminal.then_some(5),
            elapsed_ms: if terminal { 4 } else { 0 },
            summary: "bounded product activity".to_owned(),
            detail: crate::ActivityDetailView::None,
        }
    }

    #[test]
    fn completed_retention_collapses_to_a_typed_aggregate() {
        let run_id = RunId::generate();
        let root_id = format!("run:{run_id}");
        let mut nodes = vec![node(
            &run_id,
            root_id.clone(),
            None,
            1,
            crate::ActivityNodeState::Succeeded,
        )];
        for index in 2..=513 {
            nodes.push(node(
                &run_id,
                format!("step:{index:04}"),
                Some(root_id.clone()),
                index,
                crate::ActivityNodeState::Succeeded,
            ));
        }
        let oversized = crate::ActivityGraphView {
            graph_revision: 1,
            roots: vec![root_id.clone()],
            nodes,
            current_path: Vec::new(),
            truncated: false,
        };
        let (aggregate, removal) =
            collapse_oldest_completed_branch(&oversized, crate::ActivityAggregateReason::NodeLimit)
                .unwrap();
        assert_eq!(aggregate.kind, crate::ActivityNodeKind::Aggregate);
        assert_eq!(aggregate.order_index, 1);
        assert_eq!(removal.subtree_root_id, root_id);
        assert!(matches!(
            aggregate.detail,
            crate::ActivityDetailView::Aggregate(crate::ActivityAggregateDetail {
                collapsed_nodes: 513,
                succeeded: 513,
                reason: crate::ActivityAggregateReason::NodeLimit,
                ..
            })
        ));
    }

    #[test]
    fn active_topology_fails_closed_at_its_reserved_bound() {
        let run_id = RunId::generate();
        let root_id = format!("run:{run_id}");
        let mut nodes = vec![node(
            &run_id,
            root_id.clone(),
            None,
            1,
            crate::ActivityNodeState::Running,
        )];
        for index in 2..=257 {
            nodes.push(node(
                &run_id,
                format!("step:{index:04}"),
                Some(root_id.clone()),
                index,
                crate::ActivityNodeState::Waiting,
            ));
        }
        let graph = crate::ActivityGraphView {
            graph_revision: 1,
            roots: vec![root_id.clone()],
            nodes,
            current_path: vec![root_id],
            truncated: false,
        };
        let error = enforce_active_activity_capacity(&graph).unwrap_err();
        assert_eq!(error.code, ApplicationErrorCode::ActivityCapacityExceeded);
    }

    #[test]
    fn retention_can_collapse_a_completed_sibling_under_an_active_run() {
        let run_id = RunId::generate();
        let root_id = format!("run:{run_id}");
        let completed_id = "step:completed".to_owned();
        let graph = crate::ActivityGraphView {
            graph_revision: 1,
            roots: vec![root_id.clone()],
            nodes: vec![
                node(
                    &run_id,
                    root_id.clone(),
                    None,
                    1,
                    crate::ActivityNodeState::Running,
                ),
                node(
                    &run_id,
                    completed_id.clone(),
                    Some(root_id.clone()),
                    2,
                    crate::ActivityNodeState::Succeeded,
                ),
                node(
                    &run_id,
                    "step:waiting".to_owned(),
                    Some(root_id),
                    3,
                    crate::ActivityNodeState::Waiting,
                ),
            ],
            current_path: vec![format!("run:{run_id}"), "step:waiting".to_owned()],
            truncated: false,
        };
        graph.validate().unwrap();
        let (aggregate, removal) =
            collapse_oldest_completed_branch(&graph, crate::ActivityAggregateReason::Retention)
                .unwrap();
        assert_eq!(removal.subtree_root_id, completed_id);
        assert_eq!(aggregate.parent_node_id, Some(format!("run:{run_id}")));
        assert_eq!(aggregate.order_index, 2);
    }

    #[test]
    fn atomic_delta_replaces_a_completed_subtree_without_exposing_an_invalid_graph() {
        let run_id = RunId::generate();
        let root_id = format!("run:{run_id}");
        let current = crate::ActivityGraphView {
            graph_revision: 1,
            roots: vec![root_id.clone()],
            nodes: vec![
                node(
                    &run_id,
                    root_id.clone(),
                    None,
                    1,
                    crate::ActivityNodeState::Succeeded,
                ),
                node(
                    &run_id,
                    "step:done".to_owned(),
                    Some(root_id.clone()),
                    2,
                    crate::ActivityNodeState::Succeeded,
                ),
            ],
            current_path: Vec::new(),
            truncated: false,
        };
        current.validate().unwrap();
        let aggregate = crate::ActivityNodeView {
            node_id: format!("aggregate:{run_id}:1"),
            parent_node_id: None,
            order_index: 1,
            run_id,
            turn_id: None,
            attempt_id: None,
            step_id: None,
            kind: crate::ActivityNodeKind::Aggregate,
            phase: crate::ActivityPhase::Retention,
            state: crate::ActivityNodeState::Truncated,
            retry: 0,
            started_at_unix_ms: 1,
            updated_at_unix_ms: 5,
            finished_at_unix_ms: Some(5),
            elapsed_ms: 4,
            summary: "2 completed activity nodes collapsed".to_owned(),
            detail: crate::ActivityDetailView::Aggregate(crate::ActivityAggregateDetail {
                collapsed_nodes: 2,
                succeeded: 2,
                failed: 0,
                cancelled: 0,
                incomplete: 0,
                elapsed_ms: 8,
                reason: crate::ActivityAggregateReason::Retention,
            }),
        };
        let next = apply_activity_batch(
            Some(&current),
            &crate::ActivityGraphDeltaBatch {
                graph_revision: 2,
                upserts: vec![aggregate.clone()],
                removals: vec![crate::ActivityNodeRemoval {
                    subtree_root_id: root_id,
                    reason: crate::ActivityRemovalReason::CollapsedIntoAggregate,
                }],
                current_path: Some(Vec::new()),
                truncated: true,
            },
            true,
        )
        .unwrap();
        assert_eq!(next.graph_revision, 2);
        assert_eq!(next.nodes, [aggregate]);
        assert!(next.truncated);
        next.validate().unwrap();
    }

    #[test]
    fn activity_focus_is_deterministic_across_event_and_snapshot_order() {
        let run_id = RunId::generate();
        let root_id = format!("run:{run_id}");
        let mut root = node(
            &run_id,
            root_id.clone(),
            None,
            1,
            crate::ActivityNodeState::Running,
        );
        root.updated_at_unix_ms = 1;
        let mut waiting = node(
            &run_id,
            "step:waiting".to_owned(),
            Some(root_id.clone()),
            2,
            crate::ActivityNodeState::Waiting,
        );
        waiting.updated_at_unix_ms = 100;
        let mut running = node(
            &run_id,
            "step:running".to_owned(),
            Some(root_id.clone()),
            3,
            crate::ActivityNodeState::Running,
        );
        running.updated_at_unix_ms = 2;
        let graph = crate::ActivityGraphView {
            graph_revision: 1,
            roots: vec![root_id.clone()],
            nodes: vec![root, waiting, running],
            current_path: Vec::new(),
            truncated: false,
        };
        assert_eq!(
            select_activity_current_path(&graph),
            [root_id, "step:running".to_owned()]
        );
    }
}
