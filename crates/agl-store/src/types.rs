use std::path::PathBuf;

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
