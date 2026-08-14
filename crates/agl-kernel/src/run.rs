use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ToolDelivery, TurnRequest, TurnRequestKey, TurnRequestResult};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Turn,
    Cron,
    Subagent,
}

impl RunKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Cron => "cron",
            Self::Subagent => "subagent",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "turn" => Some(Self::Turn),
            "cron" => Some(Self::Cron),
            "subagent" => Some(Self::Subagent),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Running,
    Waiting,
    Succeeded,
    Incomplete,
    Failed,
    Cancelled,
}

impl RunState {
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

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "waiting" => Some(Self::Waiting),
            "succeeded" => Some(Self::Succeeded),
            "incomplete" => Some(Self::Incomplete),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Incomplete | Self::Failed | Self::Cancelled
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStepState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    OutcomeUnknown,
}

impl RunStepState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::OutcomeUnknown => "outcome_unknown",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "outcome_unknown" => Some(Self::OutcomeUnknown),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::OutcomeUnknown
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunDelivery {
    ReplaySafe,
    Idempotent,
    AtMostOnce,
}

impl RunDelivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReplaySafe => "replay_safe",
            Self::Idempotent => "idempotent",
            Self::AtMostOnce => "at_most_once",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "replay_safe" => Some(Self::ReplaySafe),
            "idempotent" => Some(Self::Idempotent),
            "at_most_once" => Some(Self::AtMostOnce),
            _ => None,
        }
    }
}

impl From<ToolDelivery> for RunDelivery {
    fn from(value: ToolDelivery) -> Self {
        match value {
            ToolDelivery::ReplaySafe => Self::ReplaySafe,
            ToolDelivery::IdempotentRunStep => Self::Idempotent,
            ToolDelivery::AtMostOnce => Self::AtMostOnce,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRequest {
    pub delivery: RunDelivery,
    pub request: TurnRequest,
}

impl RunRequest {
    pub fn new(delivery: RunDelivery, request: TurnRequest) -> Self {
        Self { delivery, request }
    }

    pub fn key(&self) -> &TurnRequestKey {
        self.request.key()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRequestResult {
    pub result: TurnRequestResult,
}

impl RunRequestResult {
    pub fn new(result: TurnRequestResult) -> Self {
        Self { result }
    }

    pub fn for_request(
        request: &RunRequest,
        result: TurnRequestResult,
    ) -> Result<Self, RunRequestIdentityError> {
        if request.key() != result.key() {
            return Err(RunRequestIdentityError::KeyMismatch);
        }
        if request.request.kind() != result.kind() {
            return Err(RunRequestIdentityError::KindMismatch);
        }
        Ok(Self { result })
    }

    pub fn key(&self) -> &TurnRequestKey {
        self.result.key()
    }

    pub fn into_inner(self) -> TurnRequestResult {
        self.result
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunRequestIdentityError {
    KeyMismatch,
    KindMismatch,
}

impl fmt::Display for RunRequestIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::KeyMismatch => "run request result key does not match request",
            Self::KindMismatch => "run request result kind does not match request",
        })
    }
}

impl std::error::Error for RunRequestIdentityError {}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunOperationId(String);

impl RunOperationId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunStepOperationId(String);

impl RunStepOperationId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunRevision(u64);

impl RunRevision {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunStepRevision(u64);

impl RunStepRevision {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunTerminalOutcome {
    Succeeded {
        result: Option<Value>,
    },
    Incomplete {
        result: Option<Value>,
        reason: String,
    },
    Failed {
        error_code: String,
        error_message: String,
    },
    Cancelled {
        error_code: Option<String>,
        error_message: Option<String>,
    },
}

impl RunTerminalOutcome {
    pub fn state(&self) -> RunState {
        match self {
            Self::Succeeded { .. } => RunState::Succeeded,
            Self::Incomplete { .. } => RunState::Incomplete,
            Self::Failed { .. } => RunState::Failed,
            Self::Cancelled { .. } => RunState::Cancelled,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunTransitionInput {
    Admit,
    Claim,
    ScheduleRetry,
    RequestCancellation,
    Finish { outcome: RunTerminalOutcome },
    RecoverExpiredLease,
    RecoverUnknownStep,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunTransitionRecord {
    pub operation_id: RunOperationId,
    pub previous_revision: RunRevision,
    pub new_revision: RunRevision,
    pub from: Option<RunState>,
    pub to: RunState,
    pub cancellation_requested: bool,
    pub input: RunTransitionInput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunMachineError {
    InvalidTransition {
        from: Option<RunState>,
        input: &'static str,
    },
    TerminalImmutable(RunState),
    OperationConflict {
        operation_id: RunOperationId,
    },
    RevisionOverflow,
}

impl fmt::Display for RunMachineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "run transition rejected: {self:?}")
    }
}

impl std::error::Error for RunMachineError {}

#[derive(Clone, Debug, Default)]
pub struct RunMachine {
    state: Option<RunState>,
    revision: RunRevision,
    cancellation_requested: bool,
    accepted: BTreeMap<RunOperationId, (String, RunTransitionRecord)>,
}

impl RunMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn restore(state: RunState, revision: RunRevision, cancellation_requested: bool) -> Self {
        Self {
            state: Some(state),
            revision,
            cancellation_requested,
            accepted: BTreeMap::new(),
        }
    }

    pub fn state(&self) -> Option<RunState> {
        self.state
    }

    pub fn revision(&self) -> RunRevision {
        self.revision
    }

    pub fn cancellation_requested(&self) -> bool {
        self.cancellation_requested
    }

    pub fn admit(
        &mut self,
        operation_id: RunOperationId,
    ) -> Result<RunTransitionRecord, RunMachineError> {
        self.apply(operation_id, RunTransitionInput::Admit)
    }

    pub fn claim(
        &mut self,
        operation_id: RunOperationId,
    ) -> Result<RunTransitionRecord, RunMachineError> {
        self.apply(operation_id, RunTransitionInput::Claim)
    }

    pub fn schedule_retry(
        &mut self,
        operation_id: RunOperationId,
    ) -> Result<RunTransitionRecord, RunMachineError> {
        self.apply(operation_id, RunTransitionInput::ScheduleRetry)
    }

    pub fn request_cancellation(
        &mut self,
        operation_id: RunOperationId,
    ) -> Result<RunTransitionRecord, RunMachineError> {
        self.apply(operation_id, RunTransitionInput::RequestCancellation)
    }

    pub fn finish(
        &mut self,
        operation_id: RunOperationId,
        outcome: RunTerminalOutcome,
    ) -> Result<RunTransitionRecord, RunMachineError> {
        self.apply(operation_id, RunTransitionInput::Finish { outcome })
    }

    pub fn recover_expired_lease(
        &mut self,
        operation_id: RunOperationId,
    ) -> Result<RunTransitionRecord, RunMachineError> {
        self.apply(operation_id, RunTransitionInput::RecoverExpiredLease)
    }

    pub fn recover_unknown_step(
        &mut self,
        operation_id: RunOperationId,
    ) -> Result<RunTransitionRecord, RunMachineError> {
        self.apply(operation_id, RunTransitionInput::RecoverUnknownStep)
    }

    pub fn apply(
        &mut self,
        operation_id: RunOperationId,
        input: RunTransitionInput,
    ) -> Result<RunTransitionRecord, RunMachineError> {
        let fingerprint = serde_json::to_string(&input).expect("run input is serializable");
        if let Some((accepted_fingerprint, record)) = self.accepted.get(&operation_id) {
            return if accepted_fingerprint == &fingerprint {
                Ok(record.clone())
            } else {
                Err(RunMachineError::OperationConflict { operation_id })
            };
        }
        if let Some(state) = self.state.filter(|state| state.is_terminal()) {
            return Err(RunMachineError::TerminalImmutable(state));
        }

        let from = self.state;
        let (to, cancellation_requested) = match (&input, from) {
            (RunTransitionInput::Admit, None) => (RunState::Queued, false),
            (RunTransitionInput::Claim, Some(RunState::Queued | RunState::Waiting))
                if !self.cancellation_requested =>
            {
                (RunState::Running, false)
            }
            (RunTransitionInput::ScheduleRetry, Some(RunState::Running)) => {
                (RunState::Waiting, self.cancellation_requested)
            }
            (
                RunTransitionInput::RequestCancellation,
                Some(RunState::Queued | RunState::Waiting),
            ) => (RunState::Cancelled, true),
            (RunTransitionInput::RequestCancellation, Some(RunState::Running)) => {
                (RunState::Running, true)
            }
            (RunTransitionInput::Finish { outcome }, Some(RunState::Running)) => {
                (outcome.state(), self.cancellation_requested)
            }
            (RunTransitionInput::RecoverExpiredLease, Some(RunState::Running)) => {
                (RunState::Queued, self.cancellation_requested)
            }
            (RunTransitionInput::RecoverUnknownStep, Some(RunState::Running)) => {
                (RunState::Failed, self.cancellation_requested)
            }
            _ => {
                return Err(RunMachineError::InvalidTransition {
                    from,
                    input: run_input_name(&input),
                });
            }
        };
        let new_revision = RunRevision(
            self.revision
                .0
                .checked_add(1)
                .ok_or(RunMachineError::RevisionOverflow)?,
        );
        let record = RunTransitionRecord {
            operation_id: operation_id.clone(),
            previous_revision: self.revision,
            new_revision,
            from,
            to,
            cancellation_requested,
            input,
        };
        self.state = Some(to);
        self.revision = new_revision;
        self.cancellation_requested = cancellation_requested;
        self.accepted
            .insert(operation_id, (fingerprint, record.clone()));
        Ok(record)
    }
}

fn run_input_name(input: &RunTransitionInput) -> &'static str {
    match input {
        RunTransitionInput::Admit => "admit",
        RunTransitionInput::Claim => "claim",
        RunTransitionInput::ScheduleRetry => "schedule_retry",
        RunTransitionInput::RequestCancellation => "request_cancellation",
        RunTransitionInput::Finish { .. } => "finish",
        RunTransitionInput::RecoverExpiredLease => "recover_expired_lease",
        RunTransitionInput::RecoverUnknownStep => "recover_unknown_step",
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunStepTransitionInput {
    Publish {
        request: RunRequest,
    },
    Claim {
        retry_limit: u32,
    },
    Complete {
        state: RunStepState,
        result: Option<RunRequestResult>,
    },
    Retry {
        error_code: String,
        retryable: bool,
        retry_limit: u32,
    },
    CancelWithRun,
    RecoverExpiredLease,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunStepTransitionRecord {
    pub operation_id: RunStepOperationId,
    pub previous_revision: RunStepRevision,
    pub new_revision: RunStepRevision,
    pub from: Option<RunStepState>,
    pub to: RunStepState,
    pub attempts: u32,
    pub input: RunStepTransitionInput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunStepMachineError {
    InvalidTransition {
        from: Option<RunStepState>,
        input: &'static str,
    },
    TerminalImmutable(RunStepState),
    OperationConflict {
        operation_id: RunStepOperationId,
    },
    RequestResultMismatch(RunRequestIdentityError),
    RetryForbidden,
    RetryLimitReached,
    RevisionOverflow,
    AttemptOverflow,
}

impl fmt::Display for RunStepMachineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "run step transition rejected: {self:?}")
    }
}

impl std::error::Error for RunStepMachineError {}

#[derive(Clone, Debug, Default)]
pub struct RunStepMachine {
    state: Option<RunStepState>,
    revision: RunStepRevision,
    request: Option<RunRequest>,
    attempts: u32,
    accepted: BTreeMap<RunStepOperationId, (String, RunStepTransitionRecord)>,
}

impl RunStepMachine {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn restore(
        state: RunStepState,
        revision: RunStepRevision,
        request: RunRequest,
        attempts: u32,
    ) -> Self {
        Self {
            state: Some(state),
            revision,
            request: Some(request),
            attempts,
            accepted: BTreeMap::new(),
        }
    }

    pub fn state(&self) -> Option<RunStepState> {
        self.state
    }
    pub fn revision(&self) -> RunStepRevision {
        self.revision
    }
    pub fn request(&self) -> Option<&RunRequest> {
        self.request.as_ref()
    }
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    pub fn publish(
        &mut self,
        operation_id: RunStepOperationId,
        request: RunRequest,
    ) -> Result<RunStepTransitionRecord, RunStepMachineError> {
        self.apply(operation_id, RunStepTransitionInput::Publish { request })
    }
    pub fn claim(
        &mut self,
        operation_id: RunStepOperationId,
        retry_limit: u32,
    ) -> Result<RunStepTransitionRecord, RunStepMachineError> {
        self.apply(operation_id, RunStepTransitionInput::Claim { retry_limit })
    }
    pub fn complete(
        &mut self,
        operation_id: RunStepOperationId,
        state: RunStepState,
        result: Option<RunRequestResult>,
    ) -> Result<RunStepTransitionRecord, RunStepMachineError> {
        self.apply(
            operation_id,
            RunStepTransitionInput::Complete { state, result },
        )
    }
    pub fn retry(
        &mut self,
        operation_id: RunStepOperationId,
        error_code: impl Into<String>,
        retryable: bool,
        retry_limit: u32,
    ) -> Result<RunStepTransitionRecord, RunStepMachineError> {
        self.apply(
            operation_id,
            RunStepTransitionInput::Retry {
                error_code: error_code.into(),
                retryable,
                retry_limit,
            },
        )
    }
    pub fn cancel_with_run(
        &mut self,
        operation_id: RunStepOperationId,
    ) -> Result<RunStepTransitionRecord, RunStepMachineError> {
        self.apply(operation_id, RunStepTransitionInput::CancelWithRun)
    }
    pub fn recover_expired_lease(
        &mut self,
        operation_id: RunStepOperationId,
    ) -> Result<RunStepTransitionRecord, RunStepMachineError> {
        self.apply(operation_id, RunStepTransitionInput::RecoverExpiredLease)
    }

    pub fn apply(
        &mut self,
        operation_id: RunStepOperationId,
        input: RunStepTransitionInput,
    ) -> Result<RunStepTransitionRecord, RunStepMachineError> {
        let fingerprint = serde_json::to_string(&input).expect("step input is serializable");
        if let Some((accepted_fingerprint, record)) = self.accepted.get(&operation_id) {
            return if accepted_fingerprint == &fingerprint {
                Ok(record.clone())
            } else {
                Err(RunStepMachineError::OperationConflict { operation_id })
            };
        }
        if let Some(state) = self.state.filter(|state| state.is_terminal()) {
            return Err(RunStepMachineError::TerminalImmutable(state));
        }
        let from = self.state;
        let mut attempts = self.attempts;
        let to = match (&input, from) {
            (RunStepTransitionInput::Publish { request }, None) => {
                self.request = Some(request.clone());
                RunStepState::Pending
            }
            (RunStepTransitionInput::Claim { retry_limit }, Some(RunStepState::Pending)) => {
                if attempts >= *retry_limit {
                    return Err(RunStepMachineError::RetryLimitReached);
                }
                attempts = attempts
                    .checked_add(1)
                    .ok_or(RunStepMachineError::AttemptOverflow)?;
                RunStepState::Running
            }
            (RunStepTransitionInput::Complete { state, result }, Some(RunStepState::Running)) => {
                if !matches!(
                    state,
                    RunStepState::Succeeded | RunStepState::Failed | RunStepState::Cancelled
                ) {
                    return Err(RunStepMachineError::InvalidTransition {
                        from,
                        input: "complete",
                    });
                }
                if let Some(result) = result {
                    RunRequestResult::for_request(
                        self.request.as_ref().expect("published request"),
                        result.result.clone(),
                    )
                    .map_err(RunStepMachineError::RequestResultMismatch)?;
                }
                *state
            }
            (
                RunStepTransitionInput::Retry {
                    retryable,
                    retry_limit,
                    ..
                },
                Some(RunStepState::Running),
            ) => {
                if !*retryable
                    || self.request.as_ref().expect("published request").delivery
                        == RunDelivery::AtMostOnce
                {
                    return Err(RunStepMachineError::RetryForbidden);
                }
                if attempts >= *retry_limit {
                    return Err(RunStepMachineError::RetryLimitReached);
                }
                RunStepState::Pending
            }
            (
                RunStepTransitionInput::CancelWithRun,
                Some(RunStepState::Pending | RunStepState::Running),
            ) => RunStepState::Cancelled,
            (RunStepTransitionInput::RecoverExpiredLease, Some(RunStepState::Running)) => {
                if self.request.as_ref().expect("published request").delivery
                    == RunDelivery::AtMostOnce
                {
                    RunStepState::OutcomeUnknown
                } else {
                    RunStepState::Pending
                }
            }
            _ => {
                return Err(RunStepMachineError::InvalidTransition {
                    from,
                    input: step_input_name(&input),
                });
            }
        };
        let new_revision = RunStepRevision(
            self.revision
                .0
                .checked_add(1)
                .ok_or(RunStepMachineError::RevisionOverflow)?,
        );
        let record = RunStepTransitionRecord {
            operation_id: operation_id.clone(),
            previous_revision: self.revision,
            new_revision,
            from,
            to,
            attempts,
            input,
        };
        self.state = Some(to);
        self.revision = new_revision;
        self.attempts = attempts;
        self.accepted
            .insert(operation_id, (fingerprint, record.clone()));
        Ok(record)
    }
}

fn step_input_name(input: &RunStepTransitionInput) -> &'static str {
    match input {
        RunStepTransitionInput::Publish { .. } => "publish",
        RunStepTransitionInput::Claim { .. } => "claim",
        RunStepTransitionInput::Complete { .. } => "complete",
        RunStepTransitionInput::Retry { .. } => "retry",
        RunStepTransitionInput::CancelWithRun => "cancel_with_run",
        RunStepTransitionInput::RecoverExpiredLease => "recover_expired_lease",
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunBudget {
    pub wall_time_ms: u64,
    pub model_input_tokens: u64,
    pub model_output_tokens: u64,
    pub model_attempts: u32,
    pub tool_calls: u32,
}

impl Default for RunBudget {
    fn default() -> Self {
        Self {
            wall_time_ms: 600_000,
            model_input_tokens: 1_000_000,
            model_output_tokens: 100_000,
            model_attempts: 32,
            tool_calls: 64,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunUsage {
    pub wall_time_ms: u64,
    pub model_input_tokens: u64,
    pub model_output_tokens: u64,
    pub model_attempts: u32,
    pub tool_calls: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationTreeBudget {
    pub max_depth: u32,
    pub max_children_per_run: u32,
    pub max_descendants: u32,
    pub max_total_output_tokens: u64,
    pub timeout_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunBudgetDimension {
    WallTime,
    ModelInputTokens,
    ModelOutputTokens,
    ModelAttempts,
    ToolCalls,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunUsageAccepted {
    pub usage: RunUsage,
    pub exhausted: Vec<RunBudgetDimension>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunChildReservationRequest {
    pub reservation_id: String,
    pub requested: RunBudget,
    pub tree_wall_time_remaining_ms: u64,
    pub tree_output_tokens_remaining: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunChildReservationAccepted {
    pub reservation_id: String,
    pub effective_budget: RunBudget,
    pub reserved_output_tokens: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunChildUsageCommit {
    pub reservation_id: String,
    pub reserved_output_tokens: u64,
    pub actual_output_tokens: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunChildUsageAccepted {
    pub reservation_id: String,
    pub released_output_tokens: u64,
    pub committed_output_tokens: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunBudgetError {
    UsageDecreased {
        dimension: RunBudgetDimension,
    },
    BudgetExhausted {
        dimensions: Vec<RunBudgetDimension>,
    },
    OperationConflict {
        operation_id: String,
    },
    ReservationConflict {
        reservation_id: String,
    },
    ReservationNotFound {
        reservation_id: String,
    },
    UnderReserved {
        reservation_id: String,
        available: u64,
        required: u64,
    },
    ArithmeticOverflow {
        dimension: RunBudgetDimension,
    },
}

impl fmt::Display for RunBudgetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "run budget operation rejected: {self:?}")
    }
}

impl std::error::Error for RunBudgetError {}

#[derive(Clone, Debug)]
pub struct RunBudgetLedger {
    limits: RunBudget,
    usage: RunUsage,
    reserved_output_tokens: u64,
    committed_child_output_tokens: u64,
    accepted_usage: BTreeMap<String, (RunUsage, RunUsageAccepted)>,
    accepted_reservations:
        BTreeMap<String, (RunChildReservationRequest, RunChildReservationAccepted)>,
    active_reservations: BTreeMap<String, u64>,
    accepted_child_usage: BTreeMap<String, (RunChildUsageCommit, RunChildUsageAccepted)>,
    committed_reservations: BTreeMap<String, RunChildUsageCommit>,
}

impl RunBudgetLedger {
    pub fn new(limits: RunBudget) -> Self {
        Self {
            limits,
            usage: RunUsage::default(),
            reserved_output_tokens: 0,
            committed_child_output_tokens: 0,
            accepted_usage: BTreeMap::new(),
            accepted_reservations: BTreeMap::new(),
            active_reservations: BTreeMap::new(),
            accepted_child_usage: BTreeMap::new(),
            committed_reservations: BTreeMap::new(),
        }
    }

    pub fn restore(limits: RunBudget, usage: RunUsage) -> Self {
        Self {
            limits,
            usage,
            reserved_output_tokens: 0,
            committed_child_output_tokens: 0,
            accepted_usage: BTreeMap::new(),
            accepted_reservations: BTreeMap::new(),
            active_reservations: BTreeMap::new(),
            accepted_child_usage: BTreeMap::new(),
            committed_reservations: BTreeMap::new(),
        }
    }

    pub fn with_delegated_output(
        mut self,
        reserved_output_tokens: u64,
        committed_child_output_tokens: u64,
    ) -> Result<Self, RunBudgetError> {
        reserved_output_tokens
            .checked_add(committed_child_output_tokens)
            .ok_or(RunBudgetError::ArithmeticOverflow {
                dimension: RunBudgetDimension::ModelOutputTokens,
            })?;
        self.reserved_output_tokens = reserved_output_tokens;
        self.committed_child_output_tokens = committed_child_output_tokens;
        Ok(self)
    }

    pub fn limits(&self) -> &RunBudget {
        &self.limits
    }
    pub fn usage(&self) -> &RunUsage {
        &self.usage
    }

    pub fn reserve_child(
        &mut self,
        operation_id: impl Into<String>,
        request: RunChildReservationRequest,
    ) -> Result<RunChildReservationAccepted, RunBudgetError> {
        let operation_id = operation_id.into();
        if let Some((accepted_input, accepted)) = self.accepted_reservations.get(&operation_id) {
            return if accepted_input == &request {
                Ok(accepted.clone())
            } else {
                Err(RunBudgetError::OperationConflict { operation_id })
            };
        }
        if self
            .active_reservations
            .contains_key(&request.reservation_id)
            || self
                .committed_reservations
                .contains_key(&request.reservation_id)
        {
            return Err(RunBudgetError::ReservationConflict {
                reservation_id: request.reservation_id,
            });
        }

        let aggregate_output = self
            .usage
            .model_output_tokens
            .checked_add(self.reserved_output_tokens)
            .and_then(|value| value.checked_add(self.committed_child_output_tokens))
            .ok_or(RunBudgetError::ArithmeticOverflow {
                dimension: RunBudgetDimension::ModelOutputTokens,
            })?;
        let mut effective = request.requested.clone();
        effective.wall_time_ms = effective
            .wall_time_ms
            .min(
                self.limits
                    .wall_time_ms
                    .saturating_sub(self.usage.wall_time_ms),
            )
            .min(request.tree_wall_time_remaining_ms);
        effective.model_input_tokens = effective.model_input_tokens.min(
            self.limits
                .model_input_tokens
                .saturating_sub(self.usage.model_input_tokens),
        );
        effective.model_output_tokens = effective
            .model_output_tokens
            .min(
                self.limits
                    .model_output_tokens
                    .saturating_sub(aggregate_output),
            )
            .min(request.tree_output_tokens_remaining);
        effective.model_attempts = effective.model_attempts.min(
            self.limits
                .model_attempts
                .saturating_sub(self.usage.model_attempts),
        );
        effective.tool_calls = effective
            .tool_calls
            .min(self.limits.tool_calls.saturating_sub(self.usage.tool_calls));

        let exhausted = zero_budget_dimensions(&effective);
        if !exhausted.is_empty() {
            return Err(RunBudgetError::BudgetExhausted {
                dimensions: exhausted,
            });
        }
        self.reserved_output_tokens = self
            .reserved_output_tokens
            .checked_add(effective.model_output_tokens)
            .ok_or(RunBudgetError::ArithmeticOverflow {
                dimension: RunBudgetDimension::ModelOutputTokens,
            })?;
        self.active_reservations.insert(
            request.reservation_id.clone(),
            effective.model_output_tokens,
        );
        let accepted = RunChildReservationAccepted {
            reservation_id: request.reservation_id.clone(),
            reserved_output_tokens: effective.model_output_tokens,
            effective_budget: effective,
        };
        self.accepted_reservations
            .insert(operation_id, (request, accepted.clone()));
        Ok(accepted)
    }

    pub fn commit_child_usage(
        &mut self,
        operation_id: impl Into<String>,
        commit: RunChildUsageCommit,
    ) -> Result<RunChildUsageAccepted, RunBudgetError> {
        let operation_id = operation_id.into();
        if let Some((accepted_input, accepted)) = self.accepted_child_usage.get(&operation_id) {
            return if accepted_input == &commit {
                Ok(accepted.clone())
            } else {
                Err(RunBudgetError::OperationConflict { operation_id })
            };
        }
        if self
            .committed_reservations
            .contains_key(&commit.reservation_id)
        {
            return Err(RunBudgetError::ReservationConflict {
                reservation_id: commit.reservation_id,
            });
        }
        if let Some(reserved) = self.active_reservations.get(&commit.reservation_id) {
            if *reserved != commit.reserved_output_tokens {
                return Err(RunBudgetError::ReservationConflict {
                    reservation_id: commit.reservation_id,
                });
            }
        } else if self.reserved_output_tokens < commit.reserved_output_tokens {
            return Err(RunBudgetError::UnderReserved {
                reservation_id: commit.reservation_id,
                available: self.reserved_output_tokens,
                required: commit.reserved_output_tokens,
            });
        }
        let next_reserved_output_tokens = self
            .reserved_output_tokens
            .checked_sub(commit.reserved_output_tokens)
            .ok_or_else(|| RunBudgetError::UnderReserved {
                reservation_id: commit.reservation_id.clone(),
                available: self.reserved_output_tokens,
                required: commit.reserved_output_tokens,
            })?;
        let next_committed_child_output_tokens = self
            .committed_child_output_tokens
            .checked_add(commit.actual_output_tokens)
            .ok_or(RunBudgetError::ArithmeticOverflow {
                dimension: RunBudgetDimension::ModelOutputTokens,
            })?;
        self.reserved_output_tokens = next_reserved_output_tokens;
        self.committed_child_output_tokens = next_committed_child_output_tokens;
        self.active_reservations.remove(&commit.reservation_id);
        self.committed_reservations
            .insert(commit.reservation_id.clone(), commit.clone());
        let accepted = RunChildUsageAccepted {
            reservation_id: commit.reservation_id.clone(),
            released_output_tokens: commit.reserved_output_tokens,
            committed_output_tokens: commit.actual_output_tokens,
        };
        self.accepted_child_usage
            .insert(operation_id, (commit, accepted.clone()));
        Ok(accepted)
    }

    pub fn observe_usage(
        &mut self,
        operation_id: impl Into<String>,
        next: RunUsage,
    ) -> Result<RunUsageAccepted, RunBudgetError> {
        let operation_id = operation_id.into();
        if let Some((accepted_input, accepted)) = self.accepted_usage.get(&operation_id) {
            return if accepted_input == &next {
                Ok(accepted.clone())
            } else {
                Err(RunBudgetError::OperationConflict { operation_id })
            };
        }
        for (decreased, dimension) in [
            (
                next.wall_time_ms < self.usage.wall_time_ms,
                RunBudgetDimension::WallTime,
            ),
            (
                next.model_input_tokens < self.usage.model_input_tokens,
                RunBudgetDimension::ModelInputTokens,
            ),
            (
                next.model_output_tokens < self.usage.model_output_tokens,
                RunBudgetDimension::ModelOutputTokens,
            ),
            (
                next.model_attempts < self.usage.model_attempts,
                RunBudgetDimension::ModelAttempts,
            ),
            (
                next.tool_calls < self.usage.tool_calls,
                RunBudgetDimension::ToolCalls,
            ),
        ] {
            if decreased {
                return Err(RunBudgetError::UsageDecreased { dimension });
            }
        }
        let exhausted = self.exhausted_dimensions(&next)?;
        let accepted = RunUsageAccepted {
            usage: next.clone(),
            exhausted,
        };
        self.usage = next.clone();
        self.accepted_usage
            .insert(operation_id, (next, accepted.clone()));
        Ok(accepted)
    }

    pub fn authorize_model_request(&self) -> Result<(), RunBudgetError> {
        let exhausted = self
            .exhausted_dimensions(&self.usage)?
            .into_iter()
            .filter(|dimension| {
                matches!(
                    dimension,
                    RunBudgetDimension::WallTime
                        | RunBudgetDimension::ModelInputTokens
                        | RunBudgetDimension::ModelOutputTokens
                        | RunBudgetDimension::ModelAttempts
                )
            })
            .collect::<Vec<_>>();
        if exhausted.is_empty() {
            Ok(())
        } else {
            Err(RunBudgetError::BudgetExhausted {
                dimensions: exhausted,
            })
        }
    }

    pub fn authorize_tool_request(&self) -> Result<(), RunBudgetError> {
        let exhausted = self
            .exhausted_dimensions(&self.usage)?
            .into_iter()
            .filter(|dimension| {
                matches!(
                    dimension,
                    RunBudgetDimension::WallTime | RunBudgetDimension::ToolCalls
                )
            })
            .collect::<Vec<_>>();
        if exhausted.is_empty() {
            Ok(())
        } else {
            Err(RunBudgetError::BudgetExhausted {
                dimensions: exhausted,
            })
        }
    }

    fn exhausted_dimensions(
        &self,
        usage: &RunUsage,
    ) -> Result<Vec<RunBudgetDimension>, RunBudgetError> {
        let aggregate_output = usage
            .model_output_tokens
            .checked_add(self.reserved_output_tokens)
            .and_then(|value| value.checked_add(self.committed_child_output_tokens))
            .ok_or(RunBudgetError::ArithmeticOverflow {
                dimension: RunBudgetDimension::ModelOutputTokens,
            })?;
        Ok(exhausted_dimensions(&self.limits, usage, aggregate_output))
    }
}

fn zero_budget_dimensions(budget: &RunBudget) -> Vec<RunBudgetDimension> {
    let mut exhausted = Vec::new();
    if budget.wall_time_ms == 0 {
        exhausted.push(RunBudgetDimension::WallTime);
    }
    if budget.model_input_tokens == 0 {
        exhausted.push(RunBudgetDimension::ModelInputTokens);
    }
    if budget.model_output_tokens == 0 {
        exhausted.push(RunBudgetDimension::ModelOutputTokens);
    }
    if budget.model_attempts == 0 {
        exhausted.push(RunBudgetDimension::ModelAttempts);
    }
    if budget.tool_calls == 0 {
        exhausted.push(RunBudgetDimension::ToolCalls);
    }
    exhausted
}

fn exhausted_dimensions(
    limits: &RunBudget,
    usage: &RunUsage,
    aggregate_output: u64,
) -> Vec<RunBudgetDimension> {
    let mut exhausted = Vec::new();
    if usage.wall_time_ms >= limits.wall_time_ms {
        exhausted.push(RunBudgetDimension::WallTime);
    }
    if usage.model_input_tokens >= limits.model_input_tokens {
        exhausted.push(RunBudgetDimension::ModelInputTokens);
    }
    if aggregate_output >= limits.model_output_tokens {
        exhausted.push(RunBudgetDimension::ModelOutputTokens);
    }
    if usage.model_attempts >= limits.model_attempts {
        exhausted.push(RunBudgetDimension::ModelAttempts);
    }
    if usage.tool_calls >= limits.tool_calls {
        exhausted.push(RunBudgetDimension::ToolCalls);
    }
    exhausted
}
