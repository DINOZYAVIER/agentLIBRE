use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use agl_content::Content;
use agl_ids::{DaemonInstanceId, EventId, RunId, SessionId};
use agl_process::{ExecutionStatus, KillMode};
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, broadcast};

use crate::{
    ApplicationActionRequest, CommandCatalog, CommandContext, PresentationCursor, SessionHeader,
    SessionLaunchOptions, SessionPresentationEvent, SessionPresentationEventEnvelope,
    SessionPresentationSnapshot, SuggestionPage, SuggestionRequest, UserShellAdmission,
    UserShellSubmission, shared_command_catalog,
};

const PRESENTATION_CHANNEL_CAPACITY: usize = 256;
const BLOCKING_BRIDGE_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationErrorCode {
    InvalidArguments,
    CommandUnavailable,
    SessionBusy,
    NotFound,
    NotAuthorized,
    StaleContextRevision,
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
            Self::StaleContextRevision => "stale_context_revision",
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
            message.truncate(8 * 1024);
        }
        Self { code, message }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionOpen {
    pub launch: SessionLaunchOptions,
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
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptAdmission {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub ordinal: u32,
    pub queued: bool,
    pub replayed: bool,
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
    WorkingDirectoryChanged {
        header: SessionHeader,
    },
    Executions {
        executions: Vec<ExecutionStatus>,
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
        terminated_executions: u32,
    },
}

pub trait ApplicationBackend: Send + Sync + 'static {
    fn open_session(&self, request: SessionOpen) -> Result<SessionOpened, ApplicationError>;
    fn snapshot(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionPresentationSnapshot, ApplicationError>;
    fn invoke(
        &self,
        request: ApplicationActionRequest,
    ) -> Result<ApplicationActionResult, ApplicationError>;
    fn submit_prompt(&self, request: PromptSubmission)
    -> Result<PromptAdmission, ApplicationError>;
    fn start_user_shell(
        &self,
        request: UserShellSubmission,
    ) -> Result<UserShellAdmission, ApplicationError>;
    fn suggestions(&self, request: SuggestionRequest) -> Result<SuggestionPage, ApplicationError>;
}

struct ProjectionState {
    snapshot: SessionPresentationSnapshot,
    sender: broadcast::Sender<SessionPresentationEventEnvelope>,
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

    pub async fn open_session(
        &self,
        request: SessionOpen,
    ) -> Result<SessionOpened, ApplicationError> {
        let backend = Arc::clone(&self.backend);
        let opened = self.blocking(move || backend.open_session(request)).await?;
        self.install_snapshot(opened.snapshot.clone())?;
        Ok(opened)
    }

    pub async fn command_catalog(
        &self,
        context: CommandContext,
    ) -> Result<CommandCatalog, ApplicationError> {
        Ok(shared_command_catalog(&context))
    }

    pub async fn command_suggestions(
        &self,
        request: SuggestionRequest,
    ) -> Result<SuggestionPage, ApplicationError> {
        let backend = Arc::clone(&self.backend);
        self.blocking(move || backend.suggestions(request))
            .await
            .map(SuggestionPage::validate)
    }

    pub async fn snapshot(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionPresentationSnapshot, ApplicationError> {
        let backend = Arc::clone(&self.backend);
        let session_id = session_id.clone();
        let snapshot = self.blocking(move || backend.snapshot(&session_id)).await?;
        self.install_snapshot(snapshot.clone())?;
        Ok(snapshot)
    }

    pub async fn invoke(
        &self,
        request: ApplicationActionRequest,
    ) -> Result<ApplicationActionResult, ApplicationError> {
        let session_id = request.session_id.clone();
        let backend = Arc::clone(&self.backend);
        let result = self.blocking(move || backend.invoke(request)).await?;
        if let Some(session_id) = session_id {
            self.refresh(&session_id).await?;
        }
        Ok(result)
    }

    pub async fn submit_prompt(
        &self,
        request: PromptSubmission,
    ) -> Result<PromptAdmission, ApplicationError> {
        let session_id = request.session_id.clone();
        let backend = Arc::clone(&self.backend);
        let admission = self
            .blocking(move || backend.submit_prompt(request))
            .await?;
        self.refresh(&session_id).await?;
        Ok(admission)
    }

    pub async fn start_user_shell(
        &self,
        request: UserShellSubmission,
    ) -> Result<UserShellAdmission, ApplicationError> {
        request.validate()?;
        let session_id = request.session_id.clone();
        let backend = Arc::clone(&self.backend);
        let admission = self
            .blocking(move || backend.start_user_shell(request))
            .await?;
        self.refresh(&session_id).await?;
        Ok(admission)
    }

    pub async fn refresh(&self, session_id: &SessionId) -> Result<(), ApplicationError> {
        let backend = Arc::clone(&self.backend);
        let owned_session_id = session_id.clone();
        let mut snapshot = self
            .blocking(move || backend.snapshot(&owned_session_id))
            .await?;
        snapshot.cursor.daemon_instance_id = self.daemon_instance_id.clone();
        let mut projections = self.projections.lock().map_err(lock_error)?;
        let Some(projection) = projections.get_mut(session_id) else {
            let (sender, _) = broadcast::channel(PRESENTATION_CHANNEL_CAPACITY);
            projections.insert(session_id.clone(), ProjectionState { snapshot, sender });
            return Ok(());
        };
        let revision = projection.snapshot.cursor.revision.saturating_add(1);
        snapshot.cursor.revision = revision;
        projection.snapshot = snapshot.clone();
        let envelope = SessionPresentationEventEnvelope {
            event_id: EventId::generate(),
            session_id: session_id.clone(),
            cursor: snapshot.cursor.clone(),
            event: SessionPresentationEvent::SnapshotReplaced {
                snapshot: Box::new(snapshot),
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
        projection.snapshot.cursor.revision = projection.snapshot.cursor.revision.saturating_add(1);
        apply_event(&mut projection.snapshot, &event);
        let envelope = SessionPresentationEventEnvelope {
            event_id: EventId::generate(),
            session_id: session_id.clone(),
            cursor: projection.snapshot.cursor.clone(),
            event,
        };
        let _ = projection.sender.send(envelope.clone());
        Ok(envelope)
    }

    fn install_snapshot(
        &self,
        mut snapshot: SessionPresentationSnapshot,
    ) -> Result<(), ApplicationError> {
        snapshot.cursor.daemon_instance_id = self.daemon_instance_id.clone();
        let mut projections = self.projections.lock().map_err(lock_error)?;
        match projections.entry(snapshot.session_id.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                let (sender, _) = broadcast::channel(PRESENTATION_CHANNEL_CAPACITY);
                entry.insert(ProjectionState { snapshot, sender });
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                snapshot.cursor.revision = entry.get().snapshot.cursor.revision;
                entry.get_mut().snapshot = snapshot;
            }
        }
        Ok(())
    }

    async fn blocking<T: Send + 'static>(
        &self,
        operation: impl FnOnce() -> Result<T, ApplicationError> + Send + 'static,
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
    _permit: OwnedSemaphorePermit,
    operation: impl FnOnce() -> Result<T, ApplicationError> + Send + 'static,
) -> Result<T, ApplicationError> {
    tokio::task::spawn_blocking(operation).await.map_err(|_| {
        ApplicationError::new(
            ApplicationErrorCode::Internal,
            "application owner task failed",
        )
    })?
}

fn lock_error<T>(_error: std::sync::PoisonError<T>) -> ApplicationError {
    ApplicationError::new(
        ApplicationErrorCode::Internal,
        "application projection lock poisoned",
    )
}

fn apply_event(snapshot: &mut SessionPresentationSnapshot, event: &SessionPresentationEvent) {
    match event {
        SessionPresentationEvent::SnapshotReplaced {
            snapshot: replacement,
        } => {
            let cursor = snapshot.cursor.clone();
            *snapshot = replacement.as_ref().clone();
            snapshot.cursor = cursor;
        }
        SessionPresentationEvent::HeaderChanged { header } => snapshot.header = header.clone(),
        SessionPresentationEvent::ItemUpsert { item } => {
            let key = item.key();
            if let Some(existing) = snapshot
                .items
                .iter_mut()
                .find(|existing| existing.key() == key)
            {
                *existing = item.clone();
            } else {
                snapshot.items.push(item.clone());
            }
        }
        SessionPresentationEvent::ItemRemoved { item_key } => {
            snapshot.items.retain(|item| item.key() != *item_key);
        }
        SessionPresentationEvent::PromptQueued { prompt } => {
            snapshot.queued_prompts.push(prompt.clone())
        }
        SessionPresentationEvent::PromptActivated { run_id } => {
            snapshot
                .queued_prompts
                .retain(|prompt| &prompt.run_id != run_id);
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
        SessionPresentationEvent::AssistantTextDelta { .. }
        | SessionPresentationEvent::PromptFinished { .. }
        | SessionPresentationEvent::ExecutionOutput { .. }
        | SessionPresentationEvent::CommandAvailabilityChanged
        | SessionPresentationEvent::Notice { .. } => {}
    }
}
