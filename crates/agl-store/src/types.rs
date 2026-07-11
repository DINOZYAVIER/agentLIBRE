use std::path::PathBuf;

use agl_events::SafeRuntimeEventEnvelope;
use agl_ids::{RunId, SessionId, StepId, TurnId};
use serde::{Deserialize, Serialize};

use crate::error::{Result, StoreError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreHealth {
    pub database_path: PathBuf,
    pub migration_version: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StoreSchemaStatus {
    pub database_path: PathBuf,
    pub database_exists: bool,
    pub schema_version: Option<u32>,
    pub current_schema_version: u32,
    pub applied_migrations: Vec<u32>,
    pub migration_required: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StoreMigrationReport {
    pub database_path: PathBuf,
    pub before_schema_version: u32,
    pub after_schema_version: u32,
    pub applied_migrations: Vec<AppliedStoreMigration>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AppliedStoreMigration {
    pub version: u32,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StoreStatus {
    pub database_path: PathBuf,
    pub schema_version: u32,
    pub domains: Vec<StoreDomainHealth>,
    pub idempotency: StoreIdempotencyHealth,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StoreDomainHealth {
    pub domain: StoreDomain,
    pub status: StoreDomainStatus,
    pub total_rows: u64,
    pub active_rows: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StoreIdempotencyHealth {
    pub in_progress: u64,
    pub stale_in_progress: Vec<StoreStaleIdempotencyRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StoreStaleIdempotencyRecord {
    pub namespace: String,
    pub key: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreDomain {
    Memory,
    Notes,
    Cron,
    Permissions,
}

impl StoreDomain {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Notes => "notes",
            Self::Cron => "cron",
            Self::Permissions => "permissions",
        }
    }

    pub fn all() -> [Self; 4] {
        [Self::Memory, Self::Notes, Self::Cron, Self::Permissions]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreDomainStatus {
    Ok,
}

impl StoreDomainStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreExportOptions {
    pub domain: StoreDomain,
    pub include_deleted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdempotencyRecord {
    pub namespace: String,
    pub key: String,
    pub fingerprint: String,
    pub status: IdempotencyStatus,
    pub result_ref: Option<String>,
    pub lease_owner: Option<String>,
    pub lease_expires_at_ms: Option<i64>,
    pub admitted_run_id: Option<RunId>,
    pub attempts: u32,
    pub last_error_code: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdempotencyStatus {
    InProgress,
    Completed,
    Failed,
    Skipped,
}

impl IdempotencyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "in_progress" => Ok(Self::InProgress),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "skipped" => Ok(Self::Skipped),
            _ => Err(StoreError::InvalidValue {
                field: "status",
                value: value.to_string(),
                reason: "invalid idempotency status",
            }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdempotencyOutcome {
    Inserted(IdempotencyRecord),
    Replayed(IdempotencyRecord),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatrixNotificationOutboxStatus {
    Queued,
    Sent,
    Failed,
}

impl MatrixNotificationOutboxStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sent => "sent",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "queued" => Ok(Self::Queued),
            "sent" => Ok(Self::Sent),
            "failed" => Ok(Self::Failed),
            _ => Err(StoreError::InvalidValue {
                field: "matrix_notification_outbox.status",
                value: value.to_string(),
                reason: "invalid Matrix notification outbox status",
            }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MatrixNotificationOutboxItem {
    pub id: String,
    pub notify_ref: String,
    pub source_kind: String,
    pub source_id: String,
    pub dedupe_key: String,
    pub body: String,
    pub status: MatrixNotificationOutboxStatus,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub delivered_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MatrixNotificationOutboxDraft {
    pub notify_ref: String,
    pub source_kind: String,
    pub source_id: String,
    pub dedupe_key: String,
    pub body: String,
}

impl MatrixNotificationOutboxDraft {
    pub fn new(
        notify_ref: impl Into<String>,
        source_kind: impl Into<String>,
        source_id: impl Into<String>,
        dedupe_key: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            notify_ref: notify_ref.into(),
            source_kind: source_kind.into(),
            source_id: source_id.into(),
            dedupe_key: dedupe_key.into(),
            body: body.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRequestStatus {
    Pending,
    Granted,
    Denied,
    Revoked,
}

impl PermissionRequestStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::Revoked => "revoked",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "granted" => Ok(Self::Granted),
            "denied" => Ok(Self::Denied),
            "revoked" => Ok(Self::Revoked),
            _ => Err(StoreError::InvalidValue {
                field: "permission_requests.status",
                value: value.to_string(),
                reason: "invalid permission request status",
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionGrantStatus {
    Active,
    Revoked,
    Expired,
}

impl PermissionGrantStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "active" => Ok(Self::Active),
            "revoked" => Ok(Self::Revoked),
            "expired" => Ok(Self::Expired),
            _ => Err(StoreError::InvalidValue {
                field: "permission_grants.status",
                value: value.to_string(),
                reason: "invalid permission grant status",
            }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionRequestDraft {
    pub requested_tools: Vec<String>,
    pub max_operation_kind: String,
    pub state_effects: Vec<String>,
    pub scope: serde_json::Value,
    pub duration: String,
    pub reason: String,
    pub requester_ref: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PermissionRequestRecord {
    pub id: String,
    pub requested_tools: Vec<String>,
    pub max_operation_kind: String,
    pub state_effects: Vec<String>,
    pub scope: serde_json::Value,
    pub duration: String,
    pub reason: String,
    pub requester_ref: String,
    pub status: PermissionRequestStatus,
    pub created_at: String,
    pub updated_at: String,
    pub resolved_at: Option<String>,
    pub resolution_ref: Option<String>,
    pub resolution_note: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionGrantDraft {
    pub request_id: Option<String>,
    pub tool_id: String,
    pub max_operation_kind: String,
    pub state_effects: Vec<String>,
    pub scope: serde_json::Value,
    pub duration: String,
    pub granted_by_ref: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PermissionGrantRecord {
    pub id: String,
    pub request_id: Option<String>,
    pub tool_id: String,
    pub max_operation_kind: String,
    pub state_effects: Vec<String>,
    pub scope: serde_json::Value,
    pub duration: String,
    pub granted_by_ref: String,
    pub status: PermissionGrantStatus,
    pub created_at: String,
    pub updated_at: String,
    pub revoked_at: Option<String>,
    pub revoke_ref: Option<String>,
    pub admitted_at: Option<String>,
    pub last_admitted_run_id: Option<String>,
    pub consumed_at: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Turn,
    Cron,
}

impl RunKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Cron => "cron",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "turn" => Ok(Self::Turn),
            "cron" => Ok(Self::Cron),
            _ => invalid_run_value("runs.kind", value, "invalid run kind"),
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
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "waiting" => Ok(Self::Waiting),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => invalid_run_value("runs.state", value, "invalid run state"),
        }
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

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "outcome_unknown" => Ok(Self::OutcomeUnknown),
            _ => invalid_run_value("run_steps.state", value, "invalid run step state"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectDeliveryClass {
    ReplaySafe,
    Idempotent,
    AtMostOnce,
}

impl EffectDeliveryClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReplaySafe => "replay_safe",
            Self::Idempotent => "idempotent",
            Self::AtMostOnce => "at_most_once",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "replay_safe" => Ok(Self::ReplaySafe),
            "idempotent" => Ok(Self::Idempotent),
            "at_most_once" => Ok(Self::AtMostOnce),
            _ => invalid_run_value(
                "run_steps.delivery_class",
                value,
                "invalid effect delivery class",
            ),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunBudget {
    pub wall_time_ms: u64,
    pub model_input_tokens: u64,
    pub model_output_tokens: u64,
    pub model_attempts: u32,
    pub capability_calls: u32,
}

impl Default for RunBudget {
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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunUsage {
    pub wall_time_ms: u64,
    pub model_input_tokens: u64,
    pub model_output_tokens: u64,
    pub model_attempts: u32,
    pub capability_calls: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DurableRunDraft {
    pub run_id: RunId,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub kind: RunKind,
    pub priority: i32,
    pub input: serde_json::Value,
    pub checkpoint: Option<serde_json::Value>,
    pub effective_policy_hash: Option<String>,
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
    pub priority: i32,
    pub input: serde_json::Value,
    pub checkpoint: Option<serde_json::Value>,
    pub effective_policy_hash: Option<String>,
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
}

#[derive(Clone, Debug, PartialEq)]
pub struct DurableRunAdmission {
    pub run: DurableRunRecord,
    pub replayed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SafeRunStatus {
    pub run_id: RunId,
    pub session_id: Option<SessionId>,
    pub turn_id: Option<TurnId>,
    pub kind: RunKind,
    pub state: RunState,
    pub priority: i32,
    pub usage: RunUsage,
    pub cancellation_requested: bool,
    pub attempts: u32,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub error_code: Option<String>,
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
    pub turn_id: Option<TurnId>,
    pub effect_sequence: u64,
    pub effect_kind: String,
    pub delivery_class: EffectDeliveryClass,
    pub request: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunStepRecord {
    pub step_id: StepId,
    pub run_id: RunId,
    pub turn_id: Option<TurnId>,
    pub effect_sequence: u64,
    pub effect_kind: String,
    pub delivery_class: EffectDeliveryClass,
    pub request: serde_json::Value,
    pub result: Option<serde_json::Value>,
    pub state: RunStepState,
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

fn invalid_run_value<T>(field: &'static str, value: &str, reason: &'static str) -> Result<T> {
    Err(StoreError::InvalidValue {
        field,
        value: value.to_string(),
        reason,
    })
}
