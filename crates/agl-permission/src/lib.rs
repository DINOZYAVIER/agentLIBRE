use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Display, Formatter};

use agl_ids::{RunId, SessionId};
use agl_kernel::{EffectId, OperationKind, RunState, SensitiveInput, ToolId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PermissionOperationId(String);

impl PermissionOperationId {
    pub fn new(value: impl Into<String>) -> Result<Self, PermissionMachineError> {
        let value = value.into();
        validate_bounded("operation_id", &value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for PermissionOperationId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PermissionRevision(u64);

impl PermissionRevision {
    pub const INITIAL: Self = Self(1);

    pub fn new(value: u64) -> Result<Self, PermissionMachineError> {
        if value == 0 {
            return Err(PermissionMachineError::InvalidValue {
                field: "revision",
                reason: "revision must be positive".to_owned(),
            });
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }

    fn next(self) -> Result<Self, PermissionMachineError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(PermissionMachineError::RevisionOverflow)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDuration {
    OneTurn,
    Session,
}

impl PermissionDuration {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OneTurn => "one_turn",
            Self::Session => "session",
        }
    }

    pub fn parse(value: &str) -> Result<Self, PermissionMachineError> {
        match value {
            "one_turn" => Ok(Self::OneTurn),
            "session" => Ok(Self::Session),
            _ => Err(PermissionMachineError::InvalidValue {
                field: "duration",
                reason: format!("unsupported permission duration {value:?}"),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRequestState {
    Pending,
    Granted,
    Denied,
    Revoked,
}

impl PermissionRequestState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::Revoked => "revoked",
        }
    }

    pub fn parse(value: &str) -> Result<Self, PermissionMachineError> {
        match value {
            "pending" => Ok(Self::Pending),
            "granted" => Ok(Self::Granted),
            "denied" => Ok(Self::Denied),
            "revoked" => Ok(Self::Revoked),
            _ => Err(PermissionMachineError::InvalidValue {
                field: "request_state",
                reason: format!("unsupported permission request state {value:?}"),
            }),
        }
    }

    pub fn is_terminal(self) -> bool {
        self != Self::Pending
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRequestResolution {
    Granted,
    Denied,
    Revoked,
}

impl PermissionRequestResolution {
    fn state(self) -> PermissionRequestState {
        match self {
            Self::Granted => PermissionRequestState::Granted,
            Self::Denied => PermissionRequestState::Denied,
            Self::Revoked => PermissionRequestState::Revoked,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PermissionGrantState {
    Active,
    Consumed { run_id: RunId },
    Expired,
    Revoked,
}

impl PermissionGrantState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Consumed { .. } => "consumed",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
        }
    }

    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Active)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionRequestTransition {
    pub operation_id: PermissionOperationId,
    pub previous_state: PermissionRequestState,
    pub new_state: PermissionRequestState,
    pub previous_revision: PermissionRevision,
    pub new_revision: PermissionRevision,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionGrantTransition {
    pub operation_id: PermissionOperationId,
    pub previous_state: PermissionGrantState,
    pub new_state: PermissionGrantState,
    pub previous_revision: PermissionRevision,
    pub new_revision: PermissionRevision,
    pub admitted_run_id: Option<RunId>,
}

#[derive(Clone, Debug)]
pub struct PermissionRequestMachine {
    state: PermissionRequestState,
    revision: PermissionRevision,
    accepted:
        BTreeMap<PermissionOperationId, (PermissionRequestResolution, PermissionRequestTransition)>,
}

impl Default for PermissionRequestMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionRequestMachine {
    pub fn new() -> Self {
        Self {
            state: PermissionRequestState::Pending,
            revision: PermissionRevision::INITIAL,
            accepted: BTreeMap::new(),
        }
    }

    pub fn restore(state: PermissionRequestState, revision: PermissionRevision) -> Self {
        Self {
            state,
            revision,
            accepted: BTreeMap::new(),
        }
    }

    pub fn state(&self) -> PermissionRequestState {
        self.state
    }

    pub fn revision(&self) -> PermissionRevision {
        self.revision
    }

    pub fn resolve(
        &mut self,
        operation_id: PermissionOperationId,
        resolution: PermissionRequestResolution,
    ) -> Result<PermissionRequestTransition, PermissionMachineError> {
        if let Some((accepted_input, transition)) = self.accepted.get(&operation_id) {
            return if *accepted_input == resolution {
                Ok(transition.clone())
            } else {
                Err(PermissionMachineError::IdempotencyConflict { operation_id })
            };
        }
        if self.state.is_terminal() {
            return Err(PermissionMachineError::TerminalRequest { state: self.state });
        }
        let new_revision = self.revision.next()?;
        let transition = PermissionRequestTransition {
            operation_id: operation_id.clone(),
            previous_state: self.state,
            new_state: resolution.state(),
            previous_revision: self.revision,
            new_revision,
        };
        self.state = transition.new_state;
        self.revision = new_revision;
        self.accepted
            .insert(operation_id, (resolution, transition.clone()));
        Ok(transition)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum GrantCommand {
    Admit(RunId),
    Expire,
    Revoke,
}

#[derive(Clone, Debug)]
pub struct PermissionGrantMachine {
    duration: PermissionDuration,
    state: PermissionGrantState,
    revision: PermissionRevision,
    accepted: BTreeMap<PermissionOperationId, (GrantCommand, PermissionGrantTransition)>,
}

impl PermissionGrantMachine {
    pub fn new(duration: PermissionDuration) -> Self {
        Self {
            duration,
            state: PermissionGrantState::Active,
            revision: PermissionRevision::INITIAL,
            accepted: BTreeMap::new(),
        }
    }

    pub fn restore(
        duration: PermissionDuration,
        state: PermissionGrantState,
        revision: PermissionRevision,
    ) -> Result<Self, PermissionMachineError> {
        if matches!(state, PermissionGrantState::Consumed { .. })
            && duration != PermissionDuration::OneTurn
        {
            return Err(PermissionMachineError::InvalidValue {
                field: "grant_state",
                reason: "only one_turn grants can be consumed".to_owned(),
            });
        }
        Ok(Self {
            duration,
            state,
            revision,
            accepted: BTreeMap::new(),
        })
    }

    pub fn state(&self) -> &PermissionGrantState {
        &self.state
    }

    pub fn revision(&self) -> PermissionRevision {
        self.revision
    }

    pub fn admit(
        &mut self,
        operation_id: PermissionOperationId,
        run_id: RunId,
    ) -> Result<PermissionGrantTransition, PermissionMachineError> {
        self.apply(operation_id, GrantCommand::Admit(run_id))
    }

    pub fn expire(
        &mut self,
        operation_id: PermissionOperationId,
    ) -> Result<PermissionGrantTransition, PermissionMachineError> {
        self.apply(operation_id, GrantCommand::Expire)
    }

    pub fn revoke(
        &mut self,
        operation_id: PermissionOperationId,
    ) -> Result<PermissionGrantTransition, PermissionMachineError> {
        self.apply(operation_id, GrantCommand::Revoke)
    }

    fn apply(
        &mut self,
        operation_id: PermissionOperationId,
        command: GrantCommand,
    ) -> Result<PermissionGrantTransition, PermissionMachineError> {
        if let Some((accepted_input, transition)) = self.accepted.get(&operation_id) {
            return if *accepted_input == command {
                Ok(transition.clone())
            } else {
                Err(PermissionMachineError::IdempotencyConflict { operation_id })
            };
        }
        if self.state.is_terminal() {
            return Err(PermissionMachineError::TerminalGrant {
                state: self.state.clone(),
            });
        }

        let (new_state, admitted_run_id) = match (&self.duration, &command) {
            (PermissionDuration::OneTurn, GrantCommand::Admit(run_id)) => (
                PermissionGrantState::Consumed {
                    run_id: run_id.clone(),
                },
                Some(run_id.clone()),
            ),
            (PermissionDuration::Session, GrantCommand::Admit(run_id)) => {
                (PermissionGrantState::Active, Some(run_id.clone()))
            }
            (PermissionDuration::Session, GrantCommand::Expire) => {
                (PermissionGrantState::Expired, None)
            }
            (_, GrantCommand::Revoke) => (PermissionGrantState::Revoked, None),
            (PermissionDuration::OneTurn, GrantCommand::Expire) => {
                return Err(PermissionMachineError::InvalidTransition {
                    reason: "one_turn grant is consumed, not expired".to_owned(),
                });
            }
        };
        let new_revision = self.revision.next()?;
        let transition = PermissionGrantTransition {
            operation_id: operation_id.clone(),
            previous_state: self.state.clone(),
            new_state,
            previous_revision: self.revision,
            new_revision,
            admitted_run_id,
        };
        self.state = transition.new_state.clone();
        self.revision = new_revision;
        self.accepted
            .insert(operation_id, (command, transition.clone()));
        Ok(transition)
    }
}

pub fn permission_grant_is_live_for_run(
    state: &PermissionGrantState,
    run: Option<(&RunId, RunState)>,
) -> bool {
    match state {
        PermissionGrantState::Active => true,
        PermissionGrantState::Consumed { run_id } => run.is_some_and(|(candidate, state)| {
            candidate == run_id
                && matches!(
                    state,
                    RunState::Queued | RunState::Running | RunState::Waiting
                )
        }),
        PermissionGrantState::Expired | PermissionGrantState::Revoked => false,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PermissionRequestDraft {
    pub requested_tools: Vec<ToolId>,
    pub max_operation_kind: OperationKind,
    pub state_effects: BTreeSet<EffectId>,
    pub sensitive_inputs: BTreeSet<SensitiveInput>,
    pub scope: Value,
    pub duration: PermissionDuration,
    pub reason: String,
    pub requester_ref: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequestRecord {
    pub id: String,
    pub requested_tools: Vec<ToolId>,
    pub max_operation_kind: OperationKind,
    pub state_effects: BTreeSet<EffectId>,
    pub sensitive_inputs: BTreeSet<SensitiveInput>,
    pub scope: Value,
    pub duration: PermissionDuration,
    pub reason: String,
    pub requester_ref: String,
    pub state: PermissionRequestState,
    pub revision: PermissionRevision,
    pub created_at: String,
    pub updated_at: String,
    pub resolved_at: Option<String>,
    pub resolution_ref: Option<String>,
    pub resolution_note: Option<String>,
    pub transition_operation_id: Option<PermissionOperationId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PermissionGrantDraft {
    pub request_id: Option<String>,
    pub tool_id: ToolId,
    pub max_operation_kind: OperationKind,
    pub state_effects: BTreeSet<EffectId>,
    pub sensitive_inputs: BTreeSet<SensitiveInput>,
    pub scope: Value,
    pub duration: PermissionDuration,
    pub granted_by_ref: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PermissionGrantRecord {
    pub id: String,
    pub request_id: Option<String>,
    pub tool_id: ToolId,
    pub max_operation_kind: OperationKind,
    pub state_effects: BTreeSet<EffectId>,
    pub sensitive_inputs: BTreeSet<SensitiveInput>,
    pub scope: Value,
    pub duration: PermissionDuration,
    pub granted_by_ref: String,
    pub state: PermissionGrantState,
    pub revision: PermissionRevision,
    pub created_at: String,
    pub updated_at: String,
    pub revoked_at: Option<String>,
    pub revoke_ref: Option<String>,
    pub admitted_at: Option<String>,
    pub last_admitted_run_id: Option<RunId>,
    pub consumed_at: Option<String>,
    pub transition_operation_id: Option<PermissionOperationId>,
}

pub trait PermissionRepository: Send + Sync {
    fn create_request(
        &self,
        draft: PermissionRequestDraft,
    ) -> Result<PermissionRequestRecord, PermissionError>;
    fn request(&self, id: &str) -> Result<Option<PermissionRequestRecord>, PermissionError>;
    fn requests_by_state(
        &self,
        state: PermissionRequestState,
    ) -> Result<Vec<PermissionRequestRecord>, PermissionError>;
    fn grant_request(
        &self,
        request_id: &str,
        granted_by_ref: &str,
        operation_id: PermissionOperationId,
        resolution_ref: Option<&str>,
    ) -> Result<Vec<PermissionGrantRecord>, PermissionError>;
    fn resolve_request(
        &self,
        request_id: &str,
        resolution: PermissionRequestResolution,
        operation_id: PermissionOperationId,
        resolution_ref: Option<&str>,
        note: Option<&str>,
    ) -> Result<PermissionRequestRecord, PermissionError>;
    fn create_grant(
        &self,
        draft: PermissionGrantDraft,
    ) -> Result<PermissionGrantRecord, PermissionError>;
    fn grant(&self, id: &str) -> Result<Option<PermissionGrantRecord>, PermissionError>;
    fn active_grants(&self) -> Result<Vec<PermissionGrantRecord>, PermissionError>;
    fn admit_grant(
        &self,
        grant_id: &str,
        run_id: &RunId,
        operation_id: PermissionOperationId,
    ) -> Result<PermissionGrantRecord, PermissionError>;
    fn revoke_grant(
        &self,
        grant_id: &str,
        operation_id: PermissionOperationId,
        revoke_ref: Option<&str>,
    ) -> Result<PermissionGrantRecord, PermissionError>;
    fn expire_session_grants(&self, session_id: &SessionId) -> Result<usize, PermissionError>;
    fn live_process_grant_ids(&self) -> Result<BTreeSet<String>, PermissionError>;
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum PermissionMachineError {
    #[error("invalid permission {field}: {reason}")]
    InvalidValue { field: &'static str, reason: String },
    #[error("permission operation {operation_id} was reused with different input")]
    IdempotencyConflict { operation_id: PermissionOperationId },
    #[error("permission request is terminal in {state:?}")]
    TerminalRequest { state: PermissionRequestState },
    #[error("permission grant is terminal in {state:?}")]
    TerminalGrant { state: PermissionGrantState },
    #[error("invalid permission transition: {reason}")]
    InvalidTransition { reason: String },
    #[error("permission revision overflow")]
    RevisionOverflow,
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum PermissionError {
    #[error(transparent)]
    Machine(#[from] PermissionMachineError),
    #[error("permission record not found: {id}")]
    NotFound { id: String },
    #[error("permission revision conflict for {id}")]
    RevisionConflict { id: String },
    #[error("permission idempotency conflict for {operation_id}")]
    IdempotencyConflict { operation_id: String },
    #[error("permission repository failed: {reason}")]
    Repository { reason: String },
}

fn validate_bounded(field: &'static str, value: &str) -> Result<(), PermissionMachineError> {
    if value.is_empty()
        || value.len() > 512
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(PermissionMachineError::InvalidValue {
            field,
            reason: "must be nonempty bounded text without control or surrounding whitespace"
                .to_owned(),
        });
    }
    Ok(())
}
