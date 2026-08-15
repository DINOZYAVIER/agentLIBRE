use agl_events::SafeRuntimeEventEnvelope;
use agl_exec::ExecutionContextSnapshot;
use agl_ids::{RunId, SessionId, StepId, TurnId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    DelegationTreeBudget, RunBudget, RunKind, RunRequest, RunRequestResult, RunRevision, RunState,
    RunStepRevision, RunStepState, RunUsage,
};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RunConcurrencyKey(String);

impl RunConcurrencyKey {
    pub const MAX_BYTES: usize = 256;

    pub fn parse(value: impl Into<String>) -> Result<Self, RunRepositoryError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > Self::MAX_BYTES
            || value.chars().any(char::is_control)
            || value.trim() != value
        {
            return Err(RunRepositoryError::InvalidValue {
                field: "runs.concurrency_key",
                reason: "must be nonempty bounded UTF-8 without control or surrounding whitespace"
                    .to_owned(),
            });
        }
        Ok(Self(value))
    }

    pub fn session(session_id: &SessionId) -> Result<Self, RunRepositoryError> {
        Self::parse(format!("session:{session_id}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RunConcurrencyKey {
    type Error = RunRepositoryError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<RunConcurrencyKey> for String {
    fn from(value: RunConcurrencyKey) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChildRunDraft {
    pub run_id: RunId,
    pub parent_run_id: RunId,
    pub spawned_by_step_id: StepId,
    pub subagent_id: String,
    pub input: serde_json::Value,
    pub priority: i32,
    pub effective_policy_hash: String,
    pub budget: RunBudget,
    pub child_spec_digest: String,
    pub model_profile_digest: String,
    pub tree_budget: DelegationTreeBudget,
    pub execution_context: ExecutionContextSnapshot,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChildRunAdmission {
    pub run: DurableRunRecord,
    pub replayed: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DurableRunDraft {
    pub run_id: RunId,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub kind: RunKind,
    pub priority: i32,
    pub concurrency_key: Option<RunConcurrencyKey>,
    pub input: serde_json::Value,
    pub checkpoint: Option<serde_json::Value>,
    pub effective_policy_hash: Option<String>,
    pub execution_context: ExecutionContextSnapshot,
    pub budget: RunBudget,
    pub not_before_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DurableRunRecord {
    pub run_id: RunId,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub kind: RunKind,
    pub state: RunState,
    pub revision: RunRevision,
    pub priority: i32,
    pub concurrency_key: Option<RunConcurrencyKey>,
    pub input: serde_json::Value,
    pub checkpoint: Option<serde_json::Value>,
    pub effective_policy_hash: Option<String>,
    pub execution_context: ExecutionContextSnapshot,
    pub budget: RunBudget,
    pub usage: RunUsage,
    pub lease_owner: Option<String>,
    pub lease_generation: u64,
    pub lease_expires_at_ms: Option<i64>,
    pub cancellation_requested_at_ms: Option<i64>,
    pub attempts: u32,
    pub not_before_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub terminal_result: Option<serde_json::Value>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub parent_run_id: Option<RunId>,
    pub root_run_id: RunId,
    pub depth: u32,
    pub subagent_id: Option<String>,
    pub spawned_by_step_id: Option<StepId>,
    pub child_spec_digest: Option<String>,
    pub model_profile_digest: Option<String>,
    pub result_delivered_at_ms: Option<i64>,
    pub tree_usage_recorded_at_ms: Option<i64>,
    pub delegation_budget: Option<DelegationTreeBudget>,
    pub delegation_reserved_descendants: u32,
    pub delegation_reserved_output_tokens: u64,
    pub delegation_used_output_tokens: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DurableRunAdmission {
    pub run: DurableRunRecord,
    pub replayed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdempotentRunRecord {
    pub namespace: String,
    pub key: String,
    pub fingerprint: String,
    pub admitted_run_id: Option<RunId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafeRunStatus {
    pub run_id: RunId,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub kind: RunKind,
    pub state: RunState,
    pub revision: RunRevision,
    pub priority: i32,
    pub concurrency_key: Option<RunConcurrencyKey>,
    pub usage: RunUsage,
    pub cancellation_requested: bool,
    pub attempts: u32,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub error_code: Option<String>,
    pub parent_run_id: Option<RunId>,
    pub root_run_id: RunId,
    pub depth: u32,
    pub subagent_id: Option<String>,
    pub spawned_by_step_id: Option<StepId>,
    pub child_spec_digest: Option<String>,
    pub model_profile_digest: Option<String>,
    pub result_delivered: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunLease {
    pub run_id: RunId,
    pub owner: String,
    pub generation: u64,
    pub expires_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunStepDraft {
    pub step_id: StepId,
    pub request: RunRequest,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunStepRecord {
    pub step_id: StepId,
    pub run_id: RunId,
    pub turn_id: Option<TurnId>,
    pub request_sequence: u64,
    pub request: RunRequest,
    pub result: Option<RunRequestResult>,
    pub state: RunStepState,
    pub revision: RunStepRevision,
    pub attempts: u32,
    pub lease_owner: Option<String>,
    pub lease_generation: u64,
    pub lease_expires_at_ms: Option<i64>,
    pub not_before_ms: Option<i64>,
    pub error_code: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub finished_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StepLease {
    pub step_id: StepId,
    pub run_id: RunId,
    pub owner: String,
    pub generation: u64,
    pub expires_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunEventRecord {
    pub envelope: SafeRuntimeEventEnvelope,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RecoveryReport {
    pub requeued_runs: u64,
    pub requeued_steps: u64,
    pub outcome_unknown_steps: u64,
    pub failed_runs: u64,
    pub reclaimed_idempotency_keys: u64,
}

pub trait RunRepository: Send + Sync {
    fn admit_run(&self, draft: &DurableRunDraft) -> Result<DurableRunRecord, RunRepositoryError>;
    fn compare_and_set_run_execution_context(
        &self,
        run_id: &RunId,
        expected_revision: u64,
        next: &ExecutionContextSnapshot,
    ) -> Result<ExecutionContextSnapshot, RunRepositoryError>;
    fn admit_run_at(
        &self,
        draft: &DurableRunDraft,
        now_ms: i64,
    ) -> Result<DurableRunRecord, RunRepositoryError>;
    fn admit_child_run(
        &self,
        draft: &ChildRunDraft,
    ) -> Result<ChildRunAdmission, RunRepositoryError>;
    fn admit_child_run_at(
        &self,
        draft: &ChildRunDraft,
        now_ms: i64,
    ) -> Result<ChildRunAdmission, RunRepositoryError>;
    fn child_run_by_spawn_step(
        &self,
        step_id: &StepId,
    ) -> Result<Option<DurableRunRecord>, RunRepositoryError>;
    fn run_children(
        &self,
        parent_run_id: &RunId,
    ) -> Result<Vec<DurableRunRecord>, RunRepositoryError>;
    fn run_tree(&self, run_id: &RunId) -> Result<Vec<SafeRunStatus>, RunRepositoryError>;
    fn expire_delegation_trees(
        &self,
        now_ms: i64,
    ) -> Result<Vec<SafeRunStatus>, RunRepositoryError>;
    #[allow(clippy::too_many_arguments)]
    fn admit_idempotent_run(
        &self,
        draft: &DurableRunDraft,
        namespace: &str,
        key: &str,
        fingerprint: &str,
        owner: &str,
        lease_expires_at_ms: i64,
        now_ms: i64,
    ) -> Result<DurableRunAdmission, RunRepositoryError>;
    fn idempotent_run(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<IdempotentRunRecord>, RunRepositoryError>;
    fn run(&self, run_id: &RunId) -> Result<Option<DurableRunRecord>, RunRepositoryError>;
    fn safe_run_status(&self, run_id: &RunId) -> Result<Option<SafeRunStatus>, RunRepositoryError>;
    fn safe_runs_for_concurrency_key(
        &self,
        key: &RunConcurrencyKey,
        include_terminal: bool,
    ) -> Result<Vec<SafeRunStatus>, RunRepositoryError>;
    fn claim_next_run(
        &self,
        owner: &str,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Result<Option<RunLease>, RunRepositoryError>;
    fn heartbeat_run(
        &self,
        lease: &RunLease,
        expires_at_ms: i64,
        now_ms: i64,
    ) -> Result<(), RunRepositoryError>;
    fn request_run_cancellation(
        &self,
        run_id: &RunId,
        now_ms: i64,
    ) -> Result<SafeRunStatus, RunRepositoryError>;
    fn request_run_tree_cancellation(
        &self,
        run_id: &RunId,
        now_ms: i64,
    ) -> Result<Vec<SafeRunStatus>, RunRepositoryError>;
    fn publish_run_step(
        &self,
        lease: &RunLease,
        checkpoint: &serde_json::Value,
        step: &RunStepDraft,
        events: &[SafeRuntimeEventEnvelope],
        now_ms: i64,
    ) -> Result<RunStepRecord, RunRepositoryError>;
    fn claim_run_step(
        &self,
        run_lease: &RunLease,
        step_id: &StepId,
        expires_at_ms: i64,
        now_ms: i64,
    ) -> Result<StepLease, RunRepositoryError>;
    #[allow(clippy::too_many_arguments)]
    fn complete_run_step(
        &self,
        run_lease: &RunLease,
        step_lease: &StepLease,
        state: RunStepState,
        result: Option<&RunRequestResult>,
        checkpoint: &serde_json::Value,
        usage: &RunUsage,
        events: &[SafeRuntimeEventEnvelope],
        error_code: Option<&str>,
        now_ms: i64,
    ) -> Result<RunStepRecord, RunRepositoryError>;
    #[allow(clippy::too_many_arguments)]
    fn retry_run_step(
        &self,
        run_lease: &RunLease,
        step_lease: &StepLease,
        retry_limit: u32,
        not_before_ms: i64,
        error_code: &str,
        checkpoint: &serde_json::Value,
        usage: &RunUsage,
        events: &[SafeRuntimeEventEnvelope],
        now_ms: i64,
    ) -> Result<(), RunRepositoryError>;
    #[allow(clippy::too_many_arguments)]
    fn finish_run(
        &self,
        lease: &RunLease,
        state: RunState,
        checkpoint: Option<&serde_json::Value>,
        usage: &RunUsage,
        terminal_result: Option<&serde_json::Value>,
        error_code: Option<&str>,
        error_message: Option<&str>,
        events: &[SafeRuntimeEventEnvelope],
        now_ms: i64,
    ) -> Result<DurableRunRecord, RunRepositoryError>;
    fn run_steps(&self, run_id: &RunId) -> Result<Vec<RunStepRecord>, RunRepositoryError>;
    fn run_step_by_sequence(
        &self,
        run_id: &RunId,
        request_sequence: u64,
    ) -> Result<Option<RunStepRecord>, RunRepositoryError>;
    fn run_events_after(
        &self,
        run_id: &RunId,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<SafeRuntimeEventEnvelope>, RunRepositoryError>;
    fn latest_run_event_sequence(&self, run_id: &RunId) -> Result<u64, RunRepositoryError>;
    fn recover_expired_work(&self, now_ms: i64) -> Result<RecoveryReport, RunRepositoryError>;
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum RunRepositoryError {
    #[error("invalid run repository {field}: {reason}")]
    InvalidValue { field: &'static str, reason: String },
    #[error("run repository record not found: {resource}")]
    NotFound { resource: String },
    #[error("run repository transition rejected for {resource}: {from} -> {to}")]
    TransitionRejected {
        resource: String,
        from: String,
        to: String,
    },
    #[error("run repository lease lost: {resource}")]
    LeaseLost { resource: String },
    #[error("run idempotency conflict for {namespace}/{key}")]
    IdempotencyConflict { namespace: String, key: String },
    #[error("run delegation denied: {code}")]
    DelegationDenied { code: &'static str },
    #[error("run repository failed: {reason}")]
    Repository { reason: String },
}
