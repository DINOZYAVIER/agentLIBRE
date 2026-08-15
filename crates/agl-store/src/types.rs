use std::path::PathBuf;

use agl_ids::RunId;
use serde::{Deserialize, Serialize};

pub use agl_kernel::{
    ChildRunAdmission, ChildRunDraft, DelegationTreeBudget, DurableRunAdmission, DurableRunDraft,
    DurableRunRecord, RecoveryReport, RunBudget, RunConcurrencyKey, RunDelivery, RunEventRecord,
    RunKind, RunLease, RunRequest, RunRequestResult, RunRevision, RunState, RunStepDraft,
    RunStepRecord, RunStepRevision, RunStepState, RunUsage, SafeRunStatus, StepLease,
};

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
