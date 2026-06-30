use std::fmt;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agl_runtime::AgentLibrePaths;
use rusqlite::{Connection, OptionalExtension, params, types::Type};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const DEFAULT_DATABASE_FILE: &str = "agentlibre.sqlite3";
pub const CURRENT_SCHEMA_VERSION: u32 = 9;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreMigration {
    pub version: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

pub const STORE_MIGRATIONS: &[StoreMigration] = &[
    StoreMigration {
        version: 1,
        name: "001_foundation",
        sql: r#"
            CREATE TABLE IF NOT EXISTS idempotency_keys (
                namespace TEXT NOT NULL,
                key TEXT NOT NULL,
                fingerprint TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('in_progress', 'completed', 'failed')),
                result_ref TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (namespace, key)
            );
        "#,
    },
    StoreMigration {
        version: 2,
        name: "002_idempotency_skipped_status",
        sql: r#"
            ALTER TABLE idempotency_keys RENAME TO idempotency_keys_v1;
            CREATE TABLE idempotency_keys (
                namespace TEXT NOT NULL,
                key TEXT NOT NULL,
                fingerprint TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('in_progress', 'completed', 'failed', 'skipped')),
                result_ref TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (namespace, key)
            );
            INSERT INTO idempotency_keys
                (namespace, key, fingerprint, status, result_ref, created_at, updated_at)
            SELECT namespace, key, fingerprint, status, result_ref, created_at, updated_at
            FROM idempotency_keys_v1;
            DROP TABLE idempotency_keys_v1;
        "#,
    },
    StoreMigration {
        version: 3,
        name: "003_memory_entries",
        sql: r#"
            CREATE TABLE memory_entries (
                id TEXT PRIMARY KEY,
                scope_kind TEXT NOT NULL,
                scope_key TEXT NOT NULL,
                kind TEXT NOT NULL,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                source_ref TEXT,
                confidence INTEGER NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                deleted_at TEXT
            );
            CREATE INDEX memory_entries_scope_idx
                ON memory_entries(scope_kind, scope_key, deleted_at);
            CREATE VIRTUAL TABLE memory_entries_fts
                USING fts5(id UNINDEXED, title, body);
        "#,
    },
    StoreMigration {
        version: 4,
        name: "004_notes",
        sql: r#"
            CREATE TABLE notes (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                deleted_at TEXT
            );
            CREATE INDEX notes_deleted_idx
                ON notes(deleted_at, updated_at);
            CREATE TABLE note_links (
                id TEXT PRIMARY KEY,
                note_id TEXT NOT NULL,
                target_ref TEXT NOT NULL,
                label TEXT,
                created_at TEXT NOT NULL,
                FOREIGN KEY(note_id) REFERENCES notes(id)
            );
            CREATE INDEX note_links_note_idx
                ON note_links(note_id, created_at);
        "#,
    },
    StoreMigration {
        version: 5,
        name: "005_cron_jobs",
        sql: r#"
            CREATE TABLE cron_jobs (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                enabled INTEGER NOT NULL,
                target_kind TEXT NOT NULL,
                target_ref TEXT NOT NULL,
                schedule_expr TEXT NOT NULL,
                timezone TEXT NOT NULL,
                notify_ref TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                deleted_at TEXT
            );
            CREATE INDEX cron_jobs_enabled_idx
                ON cron_jobs(enabled, deleted_at, updated_at);
            CREATE TABLE cron_runs (
                id TEXT PRIMARY KEY,
                job_id TEXT NOT NULL,
                scheduled_for TEXT NOT NULL,
                started_at TEXT,
                finished_at TEXT,
                status TEXT NOT NULL,
                result_ref TEXT,
                error TEXT,
                FOREIGN KEY(job_id) REFERENCES cron_jobs(id)
            );
            CREATE INDEX cron_runs_job_idx
                ON cron_runs(job_id, scheduled_for);
        "#,
    },
    StoreMigration {
        version: 6,
        name: "006_memory_suggestions",
        sql: r#"
            CREATE TABLE memory_suggestions (
                id TEXT PRIMARY KEY,
                scope_kind TEXT NOT NULL,
                scope_key TEXT NOT NULL,
                kind TEXT NOT NULL,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                source_ref TEXT NOT NULL,
                confidence INTEGER NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('pending', 'approved', 'rejected')),
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                resolved_at TEXT,
                resolution_ref TEXT,
                resolution_note TEXT
            );
            CREATE INDEX memory_suggestions_status_idx
                ON memory_suggestions(status, updated_at);
            CREATE INDEX memory_suggestions_scope_idx
                ON memory_suggestions(scope_kind, scope_key, status);
        "#,
    },
    StoreMigration {
        version: 7,
        name: "007_cron_prompt_input_and_matrix_outbox",
        sql: r#"
            ALTER TABLE cron_jobs ADD COLUMN prompt TEXT;
            ALTER TABLE cron_jobs ADD COLUMN input TEXT;
            UPDATE cron_jobs SET timezone = 'UTC' WHERE timezone = 'local';
            CREATE TABLE matrix_notification_outbox (
                id TEXT PRIMARY KEY,
                notify_ref TEXT NOT NULL,
                source_kind TEXT NOT NULL,
                source_id TEXT NOT NULL,
                dedupe_key TEXT NOT NULL UNIQUE,
                body TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('queued', 'sent', 'failed')),
                error TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                delivered_at TEXT
            );
            CREATE INDEX matrix_notification_outbox_status_idx
                ON matrix_notification_outbox(status, updated_at);
        "#,
    },
    StoreMigration {
        version: 8,
        name: "008_permission_requests_and_grants",
        sql: r#"
            CREATE TABLE permission_requests (
                id TEXT PRIMARY KEY,
                requested_tools_json TEXT NOT NULL,
                max_operation_kind TEXT NOT NULL,
                state_effects_json TEXT NOT NULL,
                scope_json TEXT NOT NULL,
                duration TEXT NOT NULL,
                reason TEXT NOT NULL,
                requester_ref TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('pending', 'granted', 'denied', 'revoked')),
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                resolved_at TEXT,
                resolution_ref TEXT,
                resolution_note TEXT
            );
            CREATE INDEX permission_requests_status_idx
                ON permission_requests(status, updated_at);

            CREATE TABLE permission_grants (
                id TEXT PRIMARY KEY,
                request_id TEXT,
                tool_id TEXT NOT NULL,
                max_operation_kind TEXT NOT NULL,
                state_effects_json TEXT NOT NULL,
                scope_json TEXT NOT NULL,
                duration TEXT NOT NULL,
                granted_by_ref TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('active', 'revoked', 'expired')),
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                revoked_at TEXT,
                revoke_ref TEXT,
                FOREIGN KEY(request_id) REFERENCES permission_requests(id)
            );
            CREATE INDEX permission_grants_status_idx
                ON permission_grants(status, updated_at);
            CREATE INDEX permission_grants_tool_idx
                ON permission_grants(tool_id, status);
        "#,
    },
    StoreMigration {
        version: 9,
        name: "009_permission_grant_admission_lifecycle",
        sql: r#"
            ALTER TABLE permission_grants ADD COLUMN admitted_at TEXT;
            ALTER TABLE permission_grants ADD COLUMN last_admitted_run_id TEXT;
            ALTER TABLE permission_grants ADD COLUMN consumed_at TEXT;
        "#,
    },
];

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug)]
pub enum StoreError {
    InvalidPath {
        path: PathBuf,
        reason: &'static str,
    },
    InvalidValue {
        field: &'static str,
        value: String,
        reason: &'static str,
    },
    NotFound {
        resource: String,
    },
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    UnsupportedSchemaVersion {
        found: u32,
        supported: u32,
    },
    MigrationGap {
        missing: u32,
    },
    IdempotencyConflict {
        namespace: String,
        key: String,
        existing_fingerprint: String,
        requested_fingerprint: String,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath { path, reason } => {
                write!(f, "invalid store path {}: {reason}", path.display())
            }
            Self::InvalidValue {
                field,
                value,
                reason,
            } => {
                write!(f, "invalid {field} value {value:?}: {reason}")
            }
            Self::NotFound { resource } => write!(f, "{resource} not found"),
            Self::Io(err) => write!(f, "{err}"),
            Self::Sqlite(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "{err}"),
            Self::UnsupportedSchemaVersion { found, supported } => write!(
                f,
                "unsupported store schema version {found}; this build supports up to {supported}"
            ),
            Self::MigrationGap { missing } => {
                write!(f, "store migration history is missing version {missing}")
            }
            Self::IdempotencyConflict {
                namespace,
                key,
                existing_fingerprint,
                requested_fingerprint,
            } => write!(
                f,
                "idempotency conflict for {namespace}/{key}: existing fingerprint {existing_fingerprint}, requested {requested_fingerprint}"
            ),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sqlite(err)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

#[derive(Debug)]
pub struct AglStore {
    conn: Connection,
    database_path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreHealth {
    pub database_path: PathBuf,
    pub migration_version: u32,
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

    fn parse(value: &str) -> Result<Self> {
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

    fn parse(value: &str) -> Result<Self> {
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

    fn parse(value: &str) -> Result<Self> {
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

    fn parse(value: &str) -> Result<Self> {
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

impl AglStore {
    pub fn open_default(paths: &AgentLibrePaths) -> Result<Self> {
        Self::open_at(default_store_root(paths))
    }

    pub fn open_at(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let database_path = database_path(root, DEFAULT_DATABASE_FILE)?;
        ensure_private_dir(root)?;
        let conn = Connection::open(&database_path)?;
        set_private_file_permissions(&database_path)?;
        let store = Self {
            conn,
            database_path,
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn transaction<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let tx = self.conn.unchecked_transaction()?;
        match f(&tx) {
            Ok(value) => {
                tx.commit()?;
                Ok(value)
            }
            Err(err) => {
                let _ = tx.rollback();
                Err(err)
            }
        }
    }

    pub fn health(&self) -> Result<StoreHealth> {
        Ok(StoreHealth {
            database_path: self.database_path.clone(),
            migration_version: self.schema_version()?,
        })
    }

    pub fn status(&self) -> Result<StoreStatus> {
        Ok(StoreStatus {
            database_path: self.database_path.clone(),
            schema_version: self.schema_version()?,
            domains: StoreDomain::all()
                .into_iter()
                .map(|domain| self.domain_health(domain))
                .collect::<Result<Vec<_>>>()?,
            idempotency: self.idempotency_health()?,
        })
    }

    pub fn domain_health(&self, domain: StoreDomain) -> Result<StoreDomainHealth> {
        let (total_rows, active_rows) = self.domain_counts(domain)?;
        Ok(StoreDomainHealth {
            domain,
            status: StoreDomainStatus::Ok,
            total_rows,
            active_rows,
        })
    }

    pub fn idempotency_health(&self) -> Result<StoreIdempotencyHealth> {
        let stale_in_progress = self.stale_in_progress_idempotency_records()?;
        Ok(StoreIdempotencyHealth {
            in_progress: stale_in_progress.len() as u64,
            stale_in_progress,
        })
    }

    pub fn stale_in_progress_idempotency_records(
        &self,
    ) -> Result<Vec<StoreStaleIdempotencyRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT namespace, key, created_at, updated_at
             FROM idempotency_keys
             WHERE status = 'in_progress'
             ORDER BY updated_at ASC, namespace ASC, key ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(StoreStaleIdempotencyRecord {
                namespace: row.get(0)?,
                key: row.get(1)?,
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
            })
        })?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row?);
        }
        Ok(records)
    }

    pub fn export_domain_jsonl<W: Write>(
        &self,
        options: &StoreExportOptions,
        writer: W,
    ) -> Result<usize> {
        match options.domain {
            StoreDomain::Memory => self.export_memory_jsonl(options.include_deleted, writer),
            StoreDomain::Notes => self.export_notes_jsonl(options.include_deleted, writer),
            StoreDomain::Cron => self.export_cron_jsonl(options.include_deleted, writer),
            StoreDomain::Permissions => {
                self.export_permissions_jsonl(options.include_deleted, writer)
            }
        }
    }

    pub fn begin_idempotency(
        &self,
        namespace: &str,
        key: &str,
        fingerprint: &str,
    ) -> Result<IdempotencyOutcome> {
        validate_idempotency_part(namespace, "namespace")?;
        validate_idempotency_part(key, "key")?;
        validate_idempotency_part(fingerprint, "fingerprint")?;

        if let Some(existing) = self.idempotency_record(namespace, key)? {
            if existing.fingerprint == fingerprint {
                return Ok(IdempotencyOutcome::Replayed(existing));
            }
            return Err(StoreError::IdempotencyConflict {
                namespace: namespace.to_string(),
                key: key.to_string(),
                existing_fingerprint: existing.fingerprint,
                requested_fingerprint: fingerprint.to_string(),
            });
        }

        let now = timestamp();
        self.conn.execute(
            "INSERT INTO idempotency_keys
             (namespace, key, fingerprint, status, result_ref, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5)",
            params![
                namespace,
                key,
                fingerprint,
                IdempotencyStatus::InProgress.as_str(),
                now
            ],
        )?;
        let record = self
            .idempotency_record(namespace, key)?
            .expect("inserted idempotency key should be readable");
        Ok(IdempotencyOutcome::Inserted(record))
    }

    pub fn complete_idempotency(
        &self,
        namespace: &str,
        key: &str,
        result_ref: Option<&str>,
    ) -> Result<IdempotencyRecord> {
        self.finish_idempotency(namespace, key, IdempotencyStatus::Completed, result_ref)
    }

    pub fn fail_idempotency(
        &self,
        namespace: &str,
        key: &str,
        result_ref: Option<&str>,
    ) -> Result<IdempotencyRecord> {
        self.finish_idempotency(namespace, key, IdempotencyStatus::Failed, result_ref)
    }

    pub fn skip_idempotency(
        &self,
        namespace: &str,
        key: &str,
        result_ref: Option<&str>,
    ) -> Result<IdempotencyRecord> {
        self.finish_idempotency(namespace, key, IdempotencyStatus::Skipped, result_ref)
    }

    fn finish_idempotency(
        &self,
        namespace: &str,
        key: &str,
        status: IdempotencyStatus,
        result_ref: Option<&str>,
    ) -> Result<IdempotencyRecord> {
        validate_idempotency_part(namespace, "namespace")?;
        validate_idempotency_part(key, "key")?;
        let now = timestamp();
        self.conn.execute(
            "UPDATE idempotency_keys
             SET status = ?3, result_ref = ?4, updated_at = ?5
             WHERE namespace = ?1 AND key = ?2",
            params![namespace, key, status.as_str(), result_ref, now],
        )?;
        self.idempotency_record(namespace, key)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("idempotency key {namespace}/{key}"),
            })
    }

    pub fn idempotency_record(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<IdempotencyRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT namespace, key, fingerprint, status, result_ref, created_at, updated_at
             FROM idempotency_keys
             WHERE namespace = ?1 AND key = ?2",
        )?;
        stmt.query_row(params![namespace, key], |row| {
            let status: String = row.get(3)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                status,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })
        .optional()?
        .map(
            |(namespace, key, fingerprint, status, result_ref, created_at, updated_at)| {
                Ok(IdempotencyRecord {
                    namespace,
                    key,
                    fingerprint,
                    status: IdempotencyStatus::parse(&status)?,
                    result_ref,
                    created_at,
                    updated_at,
                })
            },
        )
        .transpose()
    }

    pub fn enqueue_matrix_notification(
        &self,
        draft: MatrixNotificationOutboxDraft,
    ) -> Result<MatrixNotificationOutboxItem> {
        validate_non_blank(&draft.notify_ref, "matrix_notification_outbox.notify_ref")?;
        validate_non_blank(&draft.source_kind, "matrix_notification_outbox.source_kind")?;
        validate_non_blank(&draft.source_id, "matrix_notification_outbox.source_id")?;
        validate_non_blank(&draft.dedupe_key, "matrix_notification_outbox.dedupe_key")?;
        validate_non_blank(&draft.body, "matrix_notification_outbox.body")?;

        let id = store_id("matrix_outbox");
        let now = timestamp();
        self.conn.execute(
            "INSERT INTO matrix_notification_outbox
             (id, notify_ref, source_kind, source_id, dedupe_key, body, status, error, created_at, updated_at, delivered_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued', NULL, ?7, ?7, NULL)
             ON CONFLICT(dedupe_key) DO NOTHING",
            params![
                &id,
                &draft.notify_ref,
                &draft.source_kind,
                &draft.source_id,
                &draft.dedupe_key,
                &draft.body,
                &now
            ],
        )?;
        self.matrix_notification_by_dedupe_key(&draft.dedupe_key)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("matrix notification outbox {}", draft.dedupe_key),
            })
    }

    pub fn queued_matrix_notifications(
        &self,
        limit: usize,
    ) -> Result<Vec<MatrixNotificationOutboxItem>> {
        let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare(
            "SELECT id, notify_ref, source_kind, source_id, dedupe_key, body, status, error, created_at, updated_at, delivered_at
             FROM matrix_notification_outbox
             WHERE status = 'queued'
             ORDER BY created_at ASC, id ASC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], matrix_notification_from_row)?;
        let mut notifications = Vec::new();
        for row in rows {
            notifications.push(row??);
        }
        Ok(notifications)
    }

    pub fn mark_matrix_notification_sent(&self, id: &str) -> Result<MatrixNotificationOutboxItem> {
        validate_non_blank(id, "matrix_notification_outbox.id")?;
        let now = timestamp();
        self.conn.execute(
            "UPDATE matrix_notification_outbox
             SET status = 'sent', error = NULL, updated_at = ?2, delivered_at = ?2
             WHERE id = ?1",
            params![id, now],
        )?;
        self.matrix_notification(id)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("matrix notification outbox {id}"),
            })
    }

    pub fn mark_matrix_notification_failed(
        &self,
        id: &str,
        error: &str,
    ) -> Result<MatrixNotificationOutboxItem> {
        validate_non_blank(id, "matrix_notification_outbox.id")?;
        validate_non_blank(error, "matrix_notification_outbox.error")?;
        let now = timestamp();
        self.conn.execute(
            "UPDATE matrix_notification_outbox
             SET status = 'failed', error = ?2, updated_at = ?3
             WHERE id = ?1",
            params![id, error, now],
        )?;
        self.matrix_notification(id)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("matrix notification outbox {id}"),
            })
    }

    pub fn matrix_notification(&self, id: &str) -> Result<Option<MatrixNotificationOutboxItem>> {
        validate_non_blank(id, "matrix_notification_outbox.id")?;
        self.conn
            .query_row(
                "SELECT id, notify_ref, source_kind, source_id, dedupe_key, body, status, error, created_at, updated_at, delivered_at
                 FROM matrix_notification_outbox
                 WHERE id = ?1",
                params![id],
                matrix_notification_from_row,
            )
            .optional()?
            .transpose()
    }

    pub fn matrix_notification_by_dedupe_key(
        &self,
        dedupe_key: &str,
    ) -> Result<Option<MatrixNotificationOutboxItem>> {
        validate_non_blank(dedupe_key, "matrix_notification_outbox.dedupe_key")?;
        self.conn
            .query_row(
                "SELECT id, notify_ref, source_kind, source_id, dedupe_key, body, status, error, created_at, updated_at, delivered_at
                 FROM matrix_notification_outbox
                 WHERE dedupe_key = ?1",
                params![dedupe_key],
                matrix_notification_from_row,
            )
            .optional()?
            .transpose()
    }

    pub fn create_permission_request(
        &self,
        draft: PermissionRequestDraft,
    ) -> Result<PermissionRequestRecord> {
        validate_non_empty_list(
            &draft.requested_tools,
            "permission_requests.requested_tools",
        )?;
        validate_non_blank(
            &draft.max_operation_kind,
            "permission_requests.max_operation_kind",
        )?;
        validate_non_blank(&draft.duration, "permission_requests.duration")?;
        validate_non_blank(&draft.reason, "permission_requests.reason")?;
        validate_non_blank(&draft.requester_ref, "permission_requests.requester_ref")?;

        let id = store_id("permission_request");
        let now = timestamp();
        let requested_tools_json = serde_json::to_string(&draft.requested_tools)?;
        let state_effects_json = serde_json::to_string(&draft.state_effects)?;
        let scope_json = serde_json::to_string(&draft.scope)?;
        self.conn.execute(
            "INSERT INTO permission_requests
             (id, requested_tools_json, max_operation_kind, state_effects_json, scope_json, duration, reason, requester_ref, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9, ?9, NULL, NULL, NULL)",
            params![
                &id,
                &requested_tools_json,
                &draft.max_operation_kind,
                &state_effects_json,
                &scope_json,
                &draft.duration,
                &draft.reason,
                &draft.requester_ref,
                &now
            ],
        )?;
        self.permission_request(&id)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("permission request {id}"),
            })
    }

    pub fn permission_request(&self, id: &str) -> Result<Option<PermissionRequestRecord>> {
        validate_non_blank(id, "permission_requests.id")?;
        self.conn
            .query_row(
                "SELECT id, requested_tools_json, max_operation_kind, state_effects_json, scope_json, duration, reason, requester_ref, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
                 FROM permission_requests
                 WHERE id = ?1",
                params![id],
                permission_request_from_row,
            )
            .optional()?
            .transpose()
    }

    pub fn pending_permission_requests(&self) -> Result<Vec<PermissionRequestRecord>> {
        self.permission_requests_by_status(PermissionRequestStatus::Pending)
    }

    pub fn permission_requests_by_status(
        &self,
        status: PermissionRequestStatus,
    ) -> Result<Vec<PermissionRequestRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, requested_tools_json, max_operation_kind, state_effects_json, scope_json, duration, reason, requester_ref, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
             FROM permission_requests
             WHERE status = ?1
             ORDER BY updated_at ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![status.as_str()], permission_request_from_row)?;
        let mut requests = Vec::new();
        for row in rows {
            requests.push(row??);
        }
        Ok(requests)
    }

    pub fn create_permission_grant(
        &self,
        draft: PermissionGrantDraft,
    ) -> Result<PermissionGrantRecord> {
        validate_non_blank(&draft.tool_id, "permission_grants.tool_id")?;
        validate_non_blank(
            &draft.max_operation_kind,
            "permission_grants.max_operation_kind",
        )?;
        validate_non_blank(&draft.duration, "permission_grants.duration")?;
        validate_non_blank(&draft.granted_by_ref, "permission_grants.granted_by_ref")?;
        if let Some(request_id) = &draft.request_id {
            validate_non_blank(request_id, "permission_grants.request_id")?;
        }

        let id = store_id("permission_grant");
        let now = timestamp();
        let state_effects_json = serde_json::to_string(&draft.state_effects)?;
        let scope_json = serde_json::to_string(&draft.scope)?;
        self.conn.execute(
            "INSERT INTO permission_grants
             (id, request_id, tool_id, max_operation_kind, state_effects_json, scope_json, duration, granted_by_ref, status, created_at, updated_at, revoked_at, revoke_ref)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'active', ?9, ?9, NULL, NULL)",
            params![
                &id,
                &draft.request_id,
                &draft.tool_id,
                &draft.max_operation_kind,
                &state_effects_json,
                &scope_json,
                &draft.duration,
                &draft.granted_by_ref,
                &now
            ],
        )?;
        self.permission_grant(&id)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("permission grant {id}"),
            })
    }

    pub fn grant_permission_request(
        &self,
        request_id: &str,
        granted_by_ref: &str,
        resolution_ref: Option<&str>,
    ) -> Result<Vec<PermissionGrantRecord>> {
        validate_non_blank(request_id, "permission_requests.id")?;
        validate_non_blank(granted_by_ref, "permission_grants.granted_by_ref")?;
        let request = self
            .permission_request(request_id)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("permission request {request_id}"),
            })?;
        if request.status != PermissionRequestStatus::Pending {
            return Err(StoreError::InvalidValue {
                field: "permission_requests.status",
                value: request.status.as_str().to_string(),
                reason: "permission request is not pending",
            });
        }

        let mut grants = Vec::with_capacity(request.requested_tools.len());
        for tool_id in &request.requested_tools {
            grants.push(self.create_permission_grant(PermissionGrantDraft {
                request_id: Some(request.id.clone()),
                tool_id: tool_id.clone(),
                max_operation_kind: request.max_operation_kind.clone(),
                state_effects: request.state_effects.clone(),
                scope: request.scope.clone(),
                duration: request.duration.clone(),
                granted_by_ref: granted_by_ref.to_string(),
            })?);
        }
        self.resolve_permission_request(
            request_id,
            PermissionRequestStatus::Granted,
            resolution_ref,
            None,
        )?;
        Ok(grants)
    }

    pub fn deny_permission_request(
        &self,
        request_id: &str,
        resolution_ref: Option<&str>,
        note: Option<&str>,
    ) -> Result<PermissionRequestRecord> {
        self.resolve_permission_request(
            request_id,
            PermissionRequestStatus::Denied,
            resolution_ref,
            note,
        )
    }

    pub fn revoke_permission_request(
        &self,
        request_id: &str,
        resolution_ref: Option<&str>,
        note: Option<&str>,
    ) -> Result<PermissionRequestRecord> {
        self.resolve_permission_request(
            request_id,
            PermissionRequestStatus::Revoked,
            resolution_ref,
            note,
        )
    }

    fn resolve_permission_request(
        &self,
        request_id: &str,
        status: PermissionRequestStatus,
        resolution_ref: Option<&str>,
        note: Option<&str>,
    ) -> Result<PermissionRequestRecord> {
        validate_non_blank(request_id, "permission_requests.id")?;
        let now = timestamp();
        self.conn.execute(
            "UPDATE permission_requests
             SET status = ?2, updated_at = ?3, resolved_at = ?3, resolution_ref = ?4, resolution_note = ?5
             WHERE id = ?1",
            params![request_id, status.as_str(), now, resolution_ref, note],
        )?;
        self.permission_request(request_id)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("permission request {request_id}"),
            })
    }

    pub fn permission_grant(&self, id: &str) -> Result<Option<PermissionGrantRecord>> {
        validate_non_blank(id, "permission_grants.id")?;
        self.conn
            .query_row(
                "SELECT id, request_id, tool_id, max_operation_kind, state_effects_json, scope_json, duration, granted_by_ref, status, created_at, updated_at, revoked_at, revoke_ref, admitted_at, last_admitted_run_id, consumed_at
                 FROM permission_grants
                 WHERE id = ?1",
                params![id],
                permission_grant_from_row,
            )
            .optional()?
            .transpose()
    }

    pub fn active_permission_grants(&self) -> Result<Vec<PermissionGrantRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, request_id, tool_id, max_operation_kind, state_effects_json, scope_json, duration, granted_by_ref, status, created_at, updated_at, revoked_at, revoke_ref, admitted_at, last_admitted_run_id, consumed_at
             FROM permission_grants
             WHERE status = 'active'
             ORDER BY updated_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], permission_grant_from_row)?;
        let mut grants = Vec::new();
        for row in rows {
            grants.push(row??);
        }
        Ok(grants)
    }

    pub fn revoke_permission_grant(
        &self,
        grant_id: &str,
        revoke_ref: Option<&str>,
    ) -> Result<PermissionGrantRecord> {
        validate_non_blank(grant_id, "permission_grants.id")?;
        let now = timestamp();
        self.conn.execute(
            "UPDATE permission_grants
             SET status = 'revoked', updated_at = ?2, revoked_at = ?2, revoke_ref = ?3
             WHERE id = ?1",
            params![grant_id, now, revoke_ref],
        )?;
        self.permission_grant(grant_id)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("permission grant {grant_id}"),
            })
    }

    pub fn admit_permission_grant(
        &self,
        grant_id: &str,
        run_id: &str,
    ) -> Result<PermissionGrantRecord> {
        validate_non_blank(grant_id, "permission_grants.id")?;
        validate_non_blank(run_id, "permission_grants.last_admitted_run_id")?;
        let now = timestamp();
        self.conn.execute(
            "UPDATE permission_grants
             SET status = 'expired',
                 updated_at = ?2,
                 admitted_at = COALESCE(admitted_at, ?2),
                 last_admitted_run_id = ?3,
                 consumed_at = ?2
             WHERE id = ?1 AND status = 'active'",
            params![grant_id, now, run_id],
        )?;
        self.permission_grant(grant_id)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("permission grant {grant_id}"),
            })
    }

    fn migrate(&self) -> Result<()> {
        self.ensure_migration_table()?;
        let current_version = self.schema_version()?;
        if current_version > CURRENT_SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchemaVersion {
                found: current_version,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }
        let applied_versions = self.applied_migration_versions()?;
        for version in &applied_versions {
            if *version > CURRENT_SCHEMA_VERSION {
                return Err(StoreError::UnsupportedSchemaVersion {
                    found: *version,
                    supported: CURRENT_SCHEMA_VERSION,
                });
            }
        }
        validate_migration_sequence(&applied_versions)?;
        for migration in STORE_MIGRATIONS {
            if !self.migration_applied(migration.version)? {
                self.apply_migration(migration)?;
            }
        }
        Ok(())
    }

    fn ensure_migration_table(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    fn applied_migration_versions(&self) -> Result<Vec<u32>> {
        let mut stmt = self
            .conn
            .prepare("SELECT version FROM schema_migrations ORDER BY version")?;
        let rows = stmt.query_map([], |row| row.get::<_, u32>(0))?;
        let mut versions = Vec::new();
        for row in rows {
            versions.push(row?);
        }
        Ok(versions)
    }

    fn migration_applied(&self, version: u32) -> Result<bool> {
        let applied = self
            .conn
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE version = ?1",
                params![version],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(applied)
    }

    fn apply_migration(&self, migration: &StoreMigration) -> Result<()> {
        let batch = format!(
            r#"
            BEGIN;
            {sql}
            INSERT INTO schema_migrations(version, applied_at)
            VALUES ({version}, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'));
            PRAGMA user_version = {version};
            COMMIT;
            "#,
            sql = migration.sql,
            version = migration.version
        );
        self.conn.execute_batch(&batch)?;
        Ok(())
    }

    fn schema_version(&self) -> Result<u32> {
        let version = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))?;
        Ok(version)
    }

    fn domain_counts(&self, domain: StoreDomain) -> Result<(u64, u64)> {
        match domain {
            StoreDomain::Memory => Ok((
                self.query_count("SELECT COUNT(*) FROM memory_entries")?
                    + self.query_count("SELECT COUNT(*) FROM memory_suggestions")?,
                self.query_count("SELECT COUNT(*) FROM memory_entries WHERE deleted_at IS NULL")?
                    + self.query_count(
                        "SELECT COUNT(*) FROM memory_suggestions WHERE status = 'pending'",
                    )?,
            )),
            StoreDomain::Notes => Ok((
                self.query_count("SELECT COUNT(*) FROM notes")?,
                self.query_count("SELECT COUNT(*) FROM notes WHERE deleted_at IS NULL")?,
            )),
            StoreDomain::Cron => Ok((
                self.query_count("SELECT COUNT(*) FROM cron_jobs")?
                    + self.query_count("SELECT COUNT(*) FROM cron_runs")?
                    + self.query_count("SELECT COUNT(*) FROM matrix_notification_outbox")?,
                self.query_count("SELECT COUNT(*) FROM cron_jobs WHERE deleted_at IS NULL")?
                    + self.query_count(
                        "SELECT COUNT(*) FROM matrix_notification_outbox WHERE status = 'queued'",
                    )?,
            )),
            StoreDomain::Permissions => Ok((
                self.query_count("SELECT COUNT(*) FROM permission_requests")?
                    + self.query_count("SELECT COUNT(*) FROM permission_grants")?,
                self.query_count(
                    "SELECT COUNT(*) FROM permission_requests WHERE status = 'pending'",
                )? + self.query_count(
                    "SELECT COUNT(*) FROM permission_grants WHERE status = 'active'",
                )?,
            )),
        }
    }

    fn query_count(&self, sql: &str) -> Result<u64> {
        let count = self.conn.query_row(sql, [], |row| row.get::<_, i64>(0))?;
        Ok(u64::try_from(count).unwrap_or(0))
    }

    fn export_memory_jsonl<W: Write>(&self, include_deleted: bool, mut writer: W) -> Result<usize> {
        let sql = if include_deleted {
            "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at
             FROM memory_entries
             ORDER BY updated_at ASC, id ASC"
        } else {
            "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at
             FROM memory_entries
             WHERE deleted_at IS NULL
             ORDER BY updated_at ASC, id ASC"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| {
            Ok(json!({
                "domain": StoreDomain::Memory.as_str(),
                "record_type": "memory_entry",
                "id": row.get::<_, String>(0)?,
                "scope_kind": row.get::<_, String>(1)?,
                "scope_key": row.get::<_, String>(2)?,
                "kind": row.get::<_, String>(3)?,
                "title": row.get::<_, String>(4)?,
                "body": row.get::<_, String>(5)?,
                "source_ref": row.get::<_, Option<String>>(6)?,
                "confidence": row.get::<_, i64>(7)?,
                "created_at": row.get::<_, String>(8)?,
                "updated_at": row.get::<_, String>(9)?,
                "deleted_at": row.get::<_, Option<String>>(10)?,
            }))
        })?;
        let mut count = write_jsonl_rows(&mut writer, rows)?;

        let suggestions_sql = if include_deleted {
            "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
             FROM memory_suggestions
             ORDER BY updated_at ASC, id ASC"
        } else {
            "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
             FROM memory_suggestions
             WHERE status = 'pending'
             ORDER BY updated_at ASC, id ASC"
        };
        let mut suggestions_stmt = self.conn.prepare(suggestions_sql)?;
        let suggestions = suggestions_stmt.query_map([], |row| {
            Ok(json!({
                "domain": StoreDomain::Memory.as_str(),
                "record_type": "memory_suggestion",
                "id": row.get::<_, String>(0)?,
                "scope_kind": row.get::<_, String>(1)?,
                "scope_key": row.get::<_, String>(2)?,
                "kind": row.get::<_, String>(3)?,
                "title": row.get::<_, String>(4)?,
                "body": row.get::<_, String>(5)?,
                "source_ref": row.get::<_, String>(6)?,
                "confidence": row.get::<_, i64>(7)?,
                "status": row.get::<_, String>(8)?,
                "created_at": row.get::<_, String>(9)?,
                "updated_at": row.get::<_, String>(10)?,
                "resolved_at": row.get::<_, Option<String>>(11)?,
                "resolution_ref": row.get::<_, Option<String>>(12)?,
                "resolution_note": row.get::<_, Option<String>>(13)?,
            }))
        })?;
        count += write_jsonl_rows(&mut writer, suggestions)?;
        Ok(count)
    }

    fn export_notes_jsonl<W: Write>(&self, include_deleted: bool, mut writer: W) -> Result<usize> {
        let notes_sql = if include_deleted {
            "SELECT id, title, body, created_at, updated_at, deleted_at
             FROM notes
             ORDER BY updated_at ASC, id ASC"
        } else {
            "SELECT id, title, body, created_at, updated_at, deleted_at
             FROM notes
             WHERE deleted_at IS NULL
             ORDER BY updated_at ASC, id ASC"
        };
        let mut notes_stmt = self.conn.prepare(notes_sql)?;
        let notes = notes_stmt.query_map([], |row| {
            Ok(json!({
                "domain": StoreDomain::Notes.as_str(),
                "record_type": "note",
                "id": row.get::<_, String>(0)?,
                "title": row.get::<_, String>(1)?,
                "body": row.get::<_, String>(2)?,
                "created_at": row.get::<_, String>(3)?,
                "updated_at": row.get::<_, String>(4)?,
                "deleted_at": row.get::<_, Option<String>>(5)?,
            }))
        })?;
        let mut count = write_jsonl_rows(&mut writer, notes)?;

        let links_sql = if include_deleted {
            "SELECT id, note_id, target_ref, label, created_at
             FROM note_links
             ORDER BY created_at ASC, id ASC"
        } else {
            "SELECT l.id, l.note_id, l.target_ref, l.label, l.created_at
             FROM note_links l
             JOIN notes n ON n.id = l.note_id
             WHERE n.deleted_at IS NULL
             ORDER BY l.created_at ASC, l.id ASC"
        };
        let mut links_stmt = self.conn.prepare(links_sql)?;
        let links = links_stmt.query_map([], |row| {
            Ok(json!({
                "domain": StoreDomain::Notes.as_str(),
                "record_type": "note_link",
                "id": row.get::<_, String>(0)?,
                "note_id": row.get::<_, String>(1)?,
                "target_ref": row.get::<_, String>(2)?,
                "label": row.get::<_, Option<String>>(3)?,
                "created_at": row.get::<_, String>(4)?,
            }))
        })?;
        count += write_jsonl_rows(&mut writer, links)?;
        Ok(count)
    }

    fn export_cron_jsonl<W: Write>(&self, include_deleted: bool, mut writer: W) -> Result<usize> {
        let jobs_sql = if include_deleted {
            "SELECT id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, prompt, input, created_at, updated_at, deleted_at
             FROM cron_jobs
             ORDER BY updated_at ASC, id ASC"
        } else {
            "SELECT id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, prompt, input, created_at, updated_at, deleted_at
             FROM cron_jobs
             WHERE deleted_at IS NULL
             ORDER BY updated_at ASC, id ASC"
        };
        let mut jobs_stmt = self.conn.prepare(jobs_sql)?;
        let jobs = jobs_stmt.query_map([], |row| {
            Ok(json!({
                "domain": StoreDomain::Cron.as_str(),
                "record_type": "cron_job",
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "enabled": row.get::<_, bool>(2)?,
                "target_kind": row.get::<_, String>(3)?,
                "target_ref": row.get::<_, String>(4)?,
                "schedule_expr": row.get::<_, String>(5)?,
                "timezone": row.get::<_, String>(6)?,
                "notify_ref": row.get::<_, Option<String>>(7)?,
                "prompt": row.get::<_, Option<String>>(8)?,
                "input": row.get::<_, Option<String>>(9)?,
                "created_at": row.get::<_, String>(10)?,
                "updated_at": row.get::<_, String>(11)?,
                "deleted_at": row.get::<_, Option<String>>(12)?,
            }))
        })?;
        let mut count = write_jsonl_rows(&mut writer, jobs)?;

        let runs_sql = if include_deleted {
            "SELECT id, job_id, scheduled_for, started_at, finished_at, status, result_ref, error
             FROM cron_runs
             ORDER BY scheduled_for ASC, id ASC"
        } else {
            "SELECT r.id, r.job_id, r.scheduled_for, r.started_at, r.finished_at, r.status, r.result_ref, r.error
             FROM cron_runs r
             JOIN cron_jobs j ON j.id = r.job_id
             WHERE j.deleted_at IS NULL
             ORDER BY r.scheduled_for ASC, r.id ASC"
        };
        let mut runs_stmt = self.conn.prepare(runs_sql)?;
        let runs = runs_stmt.query_map([], |row| {
            Ok(json!({
                "domain": StoreDomain::Cron.as_str(),
                "record_type": "cron_run",
                "id": row.get::<_, String>(0)?,
                "job_id": row.get::<_, String>(1)?,
                "scheduled_for": row.get::<_, String>(2)?,
                "started_at": row.get::<_, Option<String>>(3)?,
                "finished_at": row.get::<_, Option<String>>(4)?,
                "status": row.get::<_, String>(5)?,
                "result_ref": row.get::<_, Option<String>>(6)?,
                "error": row.get::<_, Option<String>>(7)?,
            }))
        })?;
        count += write_jsonl_rows(&mut writer, runs)?;

        let mut outbox_stmt = self.conn.prepare(
            "SELECT id, notify_ref, source_kind, source_id, dedupe_key, body, status, error, created_at, updated_at, delivered_at
             FROM matrix_notification_outbox
             ORDER BY created_at ASC, id ASC",
        )?;
        let outbox = outbox_stmt.query_map([], |row| {
            Ok(json!({
                "domain": StoreDomain::Cron.as_str(),
                "record_type": "matrix_notification_outbox",
                "id": row.get::<_, String>(0)?,
                "notify_ref": row.get::<_, String>(1)?,
                "source_kind": row.get::<_, String>(2)?,
                "source_id": row.get::<_, String>(3)?,
                "dedupe_key": row.get::<_, String>(4)?,
                "body": row.get::<_, String>(5)?,
                "status": row.get::<_, String>(6)?,
                "error": row.get::<_, Option<String>>(7)?,
                "created_at": row.get::<_, String>(8)?,
                "updated_at": row.get::<_, String>(9)?,
                "delivered_at": row.get::<_, Option<String>>(10)?,
            }))
        })?;
        count += write_jsonl_rows(&mut writer, outbox)?;
        Ok(count)
    }

    fn export_permissions_jsonl<W: Write>(
        &self,
        include_deleted: bool,
        mut writer: W,
    ) -> Result<usize> {
        let request_sql = if include_deleted {
            "SELECT id, requested_tools_json, max_operation_kind, state_effects_json, scope_json, duration, reason, requester_ref, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
             FROM permission_requests
             ORDER BY updated_at ASC, id ASC"
        } else {
            "SELECT id, requested_tools_json, max_operation_kind, state_effects_json, scope_json, duration, reason, requester_ref, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
             FROM permission_requests
             WHERE status = 'pending'
             ORDER BY updated_at ASC, id ASC"
        };
        let mut request_stmt = self.conn.prepare(request_sql)?;
        let requests = request_stmt.query_map([], |row| {
            Ok(json!({
                "domain": StoreDomain::Permissions.as_str(),
                "record_type": "permission_request",
                "id": row.get::<_, String>(0)?,
                "requested_tools": parse_json_cell::<Vec<String>>(row.get::<_, String>(1)?)?,
                "max_operation_kind": row.get::<_, String>(2)?,
                "state_effects": parse_json_cell::<Vec<String>>(row.get::<_, String>(3)?)?,
                "scope": parse_json_cell::<serde_json::Value>(row.get::<_, String>(4)?)?,
                "duration": row.get::<_, String>(5)?,
                "reason": row.get::<_, String>(6)?,
                "requester_ref": row.get::<_, String>(7)?,
                "status": row.get::<_, String>(8)?,
                "created_at": row.get::<_, String>(9)?,
                "updated_at": row.get::<_, String>(10)?,
                "resolved_at": row.get::<_, Option<String>>(11)?,
                "resolution_ref": row.get::<_, Option<String>>(12)?,
                "resolution_note": row.get::<_, Option<String>>(13)?,
            }))
        })?;
        let mut count = write_jsonl_rows(&mut writer, requests)?;

        let grant_sql = if include_deleted {
            "SELECT id, request_id, tool_id, max_operation_kind, state_effects_json, scope_json, duration, granted_by_ref, status, created_at, updated_at, revoked_at, revoke_ref, admitted_at, last_admitted_run_id, consumed_at
             FROM permission_grants
             ORDER BY updated_at ASC, id ASC"
        } else {
            "SELECT id, request_id, tool_id, max_operation_kind, state_effects_json, scope_json, duration, granted_by_ref, status, created_at, updated_at, revoked_at, revoke_ref, admitted_at, last_admitted_run_id, consumed_at
             FROM permission_grants
             WHERE status = 'active'
             ORDER BY updated_at ASC, id ASC"
        };
        let mut grant_stmt = self.conn.prepare(grant_sql)?;
        let grants = grant_stmt.query_map([], |row| {
            Ok(json!({
                "domain": StoreDomain::Permissions.as_str(),
                "record_type": "permission_grant",
                "id": row.get::<_, String>(0)?,
                "request_id": row.get::<_, Option<String>>(1)?,
                "tool_id": row.get::<_, String>(2)?,
                "max_operation_kind": row.get::<_, String>(3)?,
                "state_effects": parse_json_cell::<Vec<String>>(row.get::<_, String>(4)?)?,
                "scope": parse_json_cell::<serde_json::Value>(row.get::<_, String>(5)?)?,
                "duration": row.get::<_, String>(6)?,
                "granted_by_ref": row.get::<_, String>(7)?,
                "status": row.get::<_, String>(8)?,
                "created_at": row.get::<_, String>(9)?,
                "updated_at": row.get::<_, String>(10)?,
                "revoked_at": row.get::<_, Option<String>>(11)?,
                "revoke_ref": row.get::<_, Option<String>>(12)?,
                "admitted_at": row.get::<_, Option<String>>(13)?,
                "last_admitted_run_id": row.get::<_, Option<String>>(14)?,
                "consumed_at": row.get::<_, Option<String>>(15)?,
            }))
        })?;
        count += write_jsonl_rows(&mut writer, grants)?;
        Ok(count)
    }
}

pub fn default_store_root(paths: &AgentLibrePaths) -> PathBuf {
    paths.data_dir.join("store")
}

fn database_path(root: &Path, file_name: &str) -> Result<PathBuf> {
    validate_database_file_name(file_name)?;
    Ok(root.join(file_name))
}

fn validate_database_file_name(file_name: &str) -> Result<()> {
    let path = Path::new(file_name);
    if path.as_os_str().is_empty() {
        return Err(StoreError::InvalidPath {
            path: path.to_path_buf(),
            reason: "database file name cannot be empty",
        });
    }
    if path.is_absolute() || path.components().count() != 1 {
        return Err(StoreError::InvalidPath {
            path: path.to_path_buf(),
            reason: "database file name must be a single relative path segment",
        });
    }
    match path.components().next() {
        Some(Component::Normal(_)) => Ok(()),
        _ => Err(StoreError::InvalidPath {
            path: path.to_path_buf(),
            reason: "database file name must be normal path segment",
        }),
    }
}

fn validate_idempotency_part(value: &str, field: &'static str) -> Result<()> {
    validate_non_blank(value, field)
}

fn validate_non_empty_list(values: &[String], field: &'static str) -> Result<()> {
    if values.is_empty() {
        return Err(StoreError::InvalidValue {
            field,
            value: "[]".to_string(),
            reason: "list cannot be empty",
        });
    }
    for value in values {
        validate_non_blank(value, field)?;
    }
    Ok(())
}

fn validate_non_blank(value: &str, field: &'static str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(StoreError::InvalidValue {
            field,
            value: value.to_string(),
            reason: match field {
                "namespace" => "namespace cannot be blank",
                "key" => "key cannot be blank",
                "fingerprint" => "fingerprint cannot be blank",
                _ => "value cannot be blank",
            },
        });
    }
    Ok(())
}

fn matrix_notification_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<MatrixNotificationOutboxItem>> {
    let status: String = row.get(6)?;
    Ok((|| {
        Ok(MatrixNotificationOutboxItem {
            id: row.get(0)?,
            notify_ref: row.get(1)?,
            source_kind: row.get(2)?,
            source_id: row.get(3)?,
            dedupe_key: row.get(4)?,
            body: row.get(5)?,
            status: MatrixNotificationOutboxStatus::parse(&status)?,
            error: row.get(7)?,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
            delivered_at: row.get(10)?,
        })
    })())
}

fn permission_request_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<PermissionRequestRecord>> {
    let requested_tools_json: String = row.get(1)?;
    let state_effects_json: String = row.get(3)?;
    let scope_json: String = row.get(4)?;
    let status: String = row.get(8)?;
    Ok((|| {
        Ok(PermissionRequestRecord {
            id: row.get(0)?,
            requested_tools: parse_json_store(&requested_tools_json)?,
            max_operation_kind: row.get(2)?,
            state_effects: parse_json_store(&state_effects_json)?,
            scope: parse_json_store(&scope_json)?,
            duration: row.get(5)?,
            reason: row.get(6)?,
            requester_ref: row.get(7)?,
            status: PermissionRequestStatus::parse(&status)?,
            created_at: row.get(9)?,
            updated_at: row.get(10)?,
            resolved_at: row.get(11)?,
            resolution_ref: row.get(12)?,
            resolution_note: row.get(13)?,
        })
    })())
}

fn permission_grant_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<PermissionGrantRecord>> {
    let state_effects_json: String = row.get(4)?;
    let scope_json: String = row.get(5)?;
    let status: String = row.get(8)?;
    Ok((|| {
        Ok(PermissionGrantRecord {
            id: row.get(0)?,
            request_id: row.get(1)?,
            tool_id: row.get(2)?,
            max_operation_kind: row.get(3)?,
            state_effects: parse_json_store(&state_effects_json)?,
            scope: parse_json_store(&scope_json)?,
            duration: row.get(6)?,
            granted_by_ref: row.get(7)?,
            status: PermissionGrantStatus::parse(&status)?,
            created_at: row.get(9)?,
            updated_at: row.get(10)?,
            revoked_at: row.get(11)?,
            revoke_ref: row.get(12)?,
            admitted_at: row.get(13)?,
            last_admitted_run_id: row.get(14)?,
            consumed_at: row.get(15)?,
        })
    })())
}

fn parse_json_store<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T> {
    serde_json::from_str(value).map_err(StoreError::from)
}

fn parse_json_cell<T: for<'de> Deserialize<'de>>(value: String) -> rusqlite::Result<T> {
    serde_json::from_str(&value)
        .map_err(|err| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(err)))
}

fn validate_migration_sequence(versions: &[u32]) -> Result<()> {
    let mut expected = 1;
    for version in versions {
        if *version != expected {
            return Err(StoreError::MigrationGap { missing: expected });
        }
        expected += 1;
    }
    Ok(())
}

fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("unix:{seconds}")
}

fn store_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{prefix}_{}_{}", std::process::id(), nanos)
}

fn write_jsonl_rows<W, I>(writer: &mut W, rows: I) -> Result<usize>
where
    W: Write,
    I: IntoIterator<Item = rusqlite::Result<serde_json::Value>>,
{
    let mut count = 0;
    for row in rows {
        let value = row?;
        serde_json::to_writer(&mut *writer, &value)?;
        writer.write_all(b"\n")?;
        count += 1;
    }
    Ok(count)
}

#[cfg(unix)]
fn ensure_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_schema_version_matches_last_migration() {
        assert_eq!(
            STORE_MIGRATIONS.last().map(|migration| migration.version),
            Some(CURRENT_SCHEMA_VERSION)
        );
        for window in STORE_MIGRATIONS.windows(2) {
            assert!(
                window[0].version < window[1].version,
                "store migrations must be ordered"
            );
        }
    }

    #[test]
    fn opens_default_store_and_reports_health() {
        let root = temp_root("health");
        let paths = AgentLibrePaths::from_agl_home(&root);

        let store = AglStore::open_default(&paths).unwrap();
        let health = store.health().unwrap();
        let status = store.status().unwrap();

        assert_eq!(health.migration_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(
            health.database_path,
            root.join("data/store/agentlibre.sqlite3")
        );
        assert_eq!(status.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(status.domains.len(), StoreDomain::all().len());
        assert!(status.domains.iter().all(|domain| domain.total_rows == 0));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn transaction_commits_and_rolls_back() {
        let root = temp_root("transaction");
        let store = AglStore::open_at(&root).unwrap();
        store
            .transaction(|tx| {
                tx.execute("CREATE TABLE tx_probe(value TEXT NOT NULL)", [])?;
                Ok(())
            })
            .unwrap();

        let err = store
            .transaction(|tx| {
                tx.execute("INSERT INTO tx_probe(value) VALUES ('rolled_back')", [])?;
                Err::<(), StoreError>(StoreError::InvalidValue {
                    field: "tx",
                    value: "rollback".to_string(),
                    reason: "test rollback",
                })
            })
            .unwrap_err();
        assert!(matches!(err, StoreError::InvalidValue { field: "tx", .. }));
        let count: u64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM tx_probe", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        store
            .transaction(|tx| {
                tx.execute("INSERT INTO tx_probe(value) VALUES ('committed')", [])?;
                Ok(())
            })
            .unwrap();
        let count: u64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM tx_probe", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn migration_history_gaps_fail_clearly() {
        let root = temp_root("migration-gap");
        std::fs::create_dir_all(&root).unwrap();
        let db_path = database_path(&root, DEFAULT_DATABASE_FILE).unwrap();
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );
            INSERT INTO schema_migrations(version, applied_at)
            VALUES (1, 'unix:1'), (3, 'unix:3');
            PRAGMA user_version = 3;
            "#,
        )
        .unwrap();
        drop(conn);

        let err = AglStore::open_at(&root).unwrap_err();
        assert!(matches!(err, StoreError::MigrationGap { missing: 2 }));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn store_status_counts_domain_rows() {
        let root = temp_root("domain-status");
        let store = AglStore::open_at(&root).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO memory_entries
                 (id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at)
                 VALUES ('mem_active', 'user', 'default', 'fact', 'Active', 'Body', NULL, 100, 'unix:1', 'unix:1', NULL),
                        ('mem_deleted', 'user', 'default', 'fact', 'Deleted', 'Body', NULL, 100, 'unix:1', 'unix:2', 'unix:3')",
                [],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO notes
                 (id, title, body, created_at, updated_at, deleted_at)
                 VALUES ('note_active', 'Active', 'Body', 'unix:1', 'unix:1', NULL),
                        ('note_deleted', 'Deleted', 'Body', 'unix:1', 'unix:2', 'unix:3')",
                [],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO cron_jobs
                 (id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, created_at, updated_at, deleted_at)
                 VALUES ('cron_active', 'Active', 1, 'builtin', 'store-status', 'hourly', 'UTC', NULL, 'unix:1', 'unix:1', NULL),
                        ('cron_deleted', 'Deleted', 0, 'builtin', 'store-status', 'hourly', 'UTC', NULL, 'unix:1', 'unix:2', 'unix:3')",
                [],
            )
            .unwrap();

        let status = store.status().unwrap();

        for domain in status.domains {
            assert_eq!(domain.status, StoreDomainStatus::Ok);
            if domain.domain == StoreDomain::Permissions {
                assert_eq!(domain.total_rows, 0, "domain={}", domain.domain.as_str());
                assert_eq!(domain.active_rows, 0, "domain={}", domain.domain.as_str());
            } else {
                assert_eq!(domain.total_rows, 2, "domain={}", domain.domain.as_str());
                assert_eq!(domain.active_rows, 1, "domain={}", domain.domain.as_str());
            }
        }

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn status_reports_in_progress_idempotency_without_recovering_it() {
        let root = temp_root("stale-idempotency");
        let store = AglStore::open_at(&root).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO idempotency_keys
                 (namespace, key, fingerprint, status, result_ref, created_at, updated_at)
                 VALUES ('cron.run', 'job-1:unix:60', 'sha256:abc', 'in_progress', NULL, 'unix:1', 'unix:2')",
                [],
            )
            .unwrap();

        let status = store.status().unwrap();
        let record = store
            .idempotency_record("cron.run", "job-1:unix:60")
            .unwrap()
            .expect("idempotency record should remain present");

        assert_eq!(status.idempotency.in_progress, 1);
        assert_eq!(status.idempotency.stale_in_progress.len(), 1);
        assert_eq!(status.idempotency.stale_in_progress[0].key, "job-1:unix:60");
        assert_eq!(record.status, IdempotencyStatus::InProgress);
        assert!(record.result_ref.is_none());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn export_memory_jsonl_respects_tombstones() {
        let root = temp_root("export-memory");
        let store = AglStore::open_at(&root).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO memory_entries
                 (id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at)
                 VALUES ('mem_active', 'user', 'default', 'fact', 'Active', 'Body', NULL, 100, 'unix:1', 'unix:1', NULL),
                        ('mem_deleted', 'user', 'default', 'fact', 'Deleted', 'Body', NULL, 100, 'unix:1', 'unix:2', 'unix:3')",
                [],
            )
            .unwrap();
        let mut active = Vec::new();
        let mut all = Vec::new();

        let active_count = store
            .export_domain_jsonl(
                &StoreExportOptions {
                    domain: StoreDomain::Memory,
                    include_deleted: false,
                },
                &mut active,
            )
            .unwrap();
        let all_count = store
            .export_domain_jsonl(
                &StoreExportOptions {
                    domain: StoreDomain::Memory,
                    include_deleted: true,
                },
                &mut all,
            )
            .unwrap();

        let active = String::from_utf8(active).unwrap();
        let all = String::from_utf8(all).unwrap();
        assert_eq!(active_count, 1);
        assert!(active.contains("\"id\":\"mem_active\""));
        assert!(!active.contains("mem_deleted"));
        assert_eq!(all_count, 2);
        assert!(all.contains("\"id\":\"mem_deleted\""));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn export_memory_jsonl_includes_pending_suggestions() {
        let root = temp_root("export-memory-suggestions");
        let store = AglStore::open_at(&root).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO memory_suggestions
                 (id, scope_kind, scope_key, kind, title, body, source_ref, confidence, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note)
                 VALUES ('suggest_pending', 'user', 'default', 'decision', 'Pending', 'Body', 'chat:1', 95, 'pending', 'unix:1', 'unix:1', NULL, NULL, NULL),
                        ('suggest_rejected', 'user', 'default', 'fact', 'Rejected', 'Body', 'chat:2', 90, 'rejected', 'unix:1', 'unix:2', 'unix:2', NULL, 'not durable')",
                [],
            )
            .unwrap();
        let mut active = Vec::new();
        let mut all = Vec::new();

        let active_count = store
            .export_domain_jsonl(
                &StoreExportOptions {
                    domain: StoreDomain::Memory,
                    include_deleted: false,
                },
                &mut active,
            )
            .unwrap();
        let all_count = store
            .export_domain_jsonl(
                &StoreExportOptions {
                    domain: StoreDomain::Memory,
                    include_deleted: true,
                },
                &mut all,
            )
            .unwrap();

        let active = String::from_utf8(active).unwrap();
        let all = String::from_utf8(all).unwrap();
        assert_eq!(active_count, 1);
        assert!(active.contains("\"record_type\":\"memory_suggestion\""));
        assert!(active.contains("\"id\":\"suggest_pending\""));
        assert!(!active.contains("suggest_rejected"));
        assert_eq!(all_count, 2);
        assert!(all.contains("\"id\":\"suggest_rejected\""));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn export_notes_and_cron_include_related_rows() {
        let root = temp_root("export-related");
        let store = AglStore::open_at(&root).unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO notes
                 (id, title, body, created_at, updated_at, deleted_at)
                 VALUES ('note_active', 'Active', 'Body', 'unix:1', 'unix:1', NULL)",
                [],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO note_links
                 (id, note_id, target_ref, label, created_at)
                 VALUES ('link_1', 'note_active', 'memory:mem_1', 'remembered', 'unix:2')",
                [],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO cron_jobs
                 (id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, created_at, updated_at, deleted_at)
                 VALUES ('cron_active', 'Active', 1, 'builtin', 'store-status', 'hourly', 'UTC', NULL, 'unix:1', 'unix:1', NULL)",
                [],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO cron_runs
                 (id, job_id, scheduled_for, started_at, finished_at, status, result_ref, error)
                 VALUES ('run_1', 'cron_active', 'unix:2', 'unix:2', 'unix:2', 'succeeded', 'builtin:store-status', NULL)",
                [],
            )
            .unwrap();
        let mut notes = Vec::new();
        let mut cron = Vec::new();

        let notes_count = store
            .export_domain_jsonl(
                &StoreExportOptions {
                    domain: StoreDomain::Notes,
                    include_deleted: false,
                },
                &mut notes,
            )
            .unwrap();
        let cron_count = store
            .export_domain_jsonl(
                &StoreExportOptions {
                    domain: StoreDomain::Cron,
                    include_deleted: false,
                },
                &mut cron,
            )
            .unwrap();

        let notes = String::from_utf8(notes).unwrap();
        let cron = String::from_utf8(cron).unwrap();
        assert_eq!(notes_count, 2);
        assert!(notes.contains("\"record_type\":\"note\""));
        assert!(notes.contains("\"record_type\":\"note_link\""));
        assert_eq!(cron_count, 2);
        assert!(cron.contains("\"record_type\":\"cron_job\""));
        assert!(cron.contains("\"record_type\":\"cron_run\""));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn matrix_notification_outbox_enqueues_once_and_exports_with_cron() {
        let root = temp_root("matrix-outbox");
        let store = AglStore::open_at(&root).unwrap();

        let first = store
            .enqueue_matrix_notification(MatrixNotificationOutboxDraft::new(
                "matrix-room:!room",
                "cron",
                "run_1",
                "cron:run_1:matrix-room:!room",
                "Cron job completed.",
            ))
            .unwrap();
        let second = store
            .enqueue_matrix_notification(MatrixNotificationOutboxDraft::new(
                "matrix-room:!room",
                "cron",
                "run_1",
                "cron:run_1:matrix-room:!room",
                "Cron job completed.",
            ))
            .unwrap();
        let queued = store.queued_matrix_notifications(10).unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(queued, vec![first.clone()]);
        assert_eq!(first.status, MatrixNotificationOutboxStatus::Queued);

        let sent = store.mark_matrix_notification_sent(&first.id).unwrap();
        assert_eq!(sent.status, MatrixNotificationOutboxStatus::Sent);
        assert!(sent.delivered_at.is_some());
        assert!(store.queued_matrix_notifications(10).unwrap().is_empty());

        let mut cron = Vec::new();
        let count = store
            .export_domain_jsonl(
                &StoreExportOptions {
                    domain: StoreDomain::Cron,
                    include_deleted: false,
                },
                &mut cron,
            )
            .unwrap();
        let cron = String::from_utf8(cron).unwrap();
        assert_eq!(count, 1);
        assert!(cron.contains("\"record_type\":\"matrix_notification_outbox\""));
        assert!(cron.contains("\"status\":\"sent\""));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn permission_requests_grants_and_revokes_are_persisted() {
        let root = temp_root("permission-requests");
        let store = AglStore::open_at(&root).unwrap();

        let request = store
            .create_permission_request(PermissionRequestDraft {
                requested_tools: vec!["cron.add".to_string(), "matrix.outbox.enqueue".to_string()],
                max_operation_kind: "write".to_string(),
                state_effects: vec!["store_cron".to_string(), "matrix_outbox".to_string()],
                scope: json!({"repo": "/tmp/repo", "matrix_room": "!room:server"}),
                duration: "one_turn".to_string(),
                reason: "Schedule a daily Matrix greeting.".to_string(),
                requester_ref: "chat:session-1:turn-1".to_string(),
            })
            .unwrap();

        assert_eq!(request.status, PermissionRequestStatus::Pending);
        assert_eq!(request.requested_tools.len(), 2);
        assert_eq!(
            store.pending_permission_requests().unwrap(),
            vec![request.clone()]
        );

        let grants = store
            .grant_permission_request(&request.id, "cli:operator", Some("chat:session-1:turn-2"))
            .unwrap();
        let resolved = store.permission_request(&request.id).unwrap().unwrap();
        let active = store.active_permission_grants().unwrap();

        assert_eq!(resolved.status, PermissionRequestStatus::Granted);
        assert_eq!(
            resolved.resolution_ref.as_deref(),
            Some("chat:session-1:turn-2")
        );
        assert_eq!(grants.len(), 2);
        assert_eq!(active.len(), 2);
        assert!(
            active
                .iter()
                .all(|grant| grant.status == PermissionGrantStatus::Active)
        );
        assert!(active.iter().all(|grant| grant.duration == "one_turn"));

        let revoked = store
            .revoke_permission_grant(&active[0].id, Some("chat:session-1:turn-3"))
            .unwrap();
        assert_eq!(revoked.status, PermissionGrantStatus::Revoked);
        assert_eq!(revoked.revoke_ref.as_deref(), Some("chat:session-1:turn-3"));
        assert_eq!(store.active_permission_grants().unwrap().len(), 1);

        let status = store.status().unwrap();
        let permissions = status
            .domains
            .iter()
            .find(|domain| domain.domain == StoreDomain::Permissions)
            .unwrap();
        assert_eq!(permissions.total_rows, 3);
        assert_eq!(permissions.active_rows, 1);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn permission_export_reports_pending_and_historical_records() {
        let root = temp_root("permission-export");
        let store = AglStore::open_at(&root).unwrap();
        let request = store
            .create_permission_request(PermissionRequestDraft {
                requested_tools: vec!["notes.add".to_string()],
                max_operation_kind: "write".to_string(),
                state_effects: vec!["store_notes".to_string()],
                scope: json!({"repo": "/tmp/repo"}),
                duration: "one_turn".to_string(),
                reason: "Create one explicit note.".to_string(),
                requester_ref: "chat:turn-1".to_string(),
            })
            .unwrap();
        let grants = store
            .grant_permission_request(&request.id, "cli:operator", Some("chat:turn-2"))
            .unwrap();
        store
            .revoke_permission_grant(&grants[0].id, Some("chat:turn-3"))
            .unwrap();

        let mut active = Vec::new();
        let active_count = store
            .export_domain_jsonl(
                &StoreExportOptions {
                    domain: StoreDomain::Permissions,
                    include_deleted: false,
                },
                &mut active,
            )
            .unwrap();
        let mut all = Vec::new();
        let all_count = store
            .export_domain_jsonl(
                &StoreExportOptions {
                    domain: StoreDomain::Permissions,
                    include_deleted: true,
                },
                &mut all,
            )
            .unwrap();

        let active = String::from_utf8(active).unwrap();
        let all = String::from_utf8(all).unwrap();
        assert_eq!(active_count, 0);
        assert_eq!(all_count, 2);
        assert!(active.is_empty());
        assert!(all.contains("\"record_type\":\"permission_request\""));
        assert!(all.contains("\"record_type\":\"permission_grant\""));
        assert!(all.contains("\"status\":\"revoked\""));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migrations_are_repeatable() {
        let root = temp_root("repeatable");
        let first = AglStore::open_at(&root).unwrap();
        assert_eq!(
            first.health().unwrap().migration_version,
            CURRENT_SCHEMA_VERSION
        );
        drop(first);

        let second = AglStore::open_at(&root).unwrap();
        assert_eq!(
            second.health().unwrap().migration_version,
            CURRENT_SCHEMA_VERSION
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn schema_v1_database_migrates_to_current() {
        let root = temp_root("migrate-v1");
        std::fs::create_dir_all(&root).unwrap();
        let db_path = database_path(&root, DEFAULT_DATABASE_FILE).unwrap();
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );
            CREATE TABLE idempotency_keys (
                namespace TEXT NOT NULL,
                key TEXT NOT NULL,
                fingerprint TEXT NOT NULL,
                status TEXT NOT NULL CHECK (status IN ('in_progress', 'completed', 'failed')),
                result_ref TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY (namespace, key)
            );
            INSERT INTO schema_migrations(version, applied_at)
            VALUES (1, 'unix:1');
            INSERT INTO idempotency_keys
                (namespace, key, fingerprint, status, result_ref, created_at, updated_at)
            VALUES ('cron.run', 'job-001:unix:1', 'sha256:abc', 'completed', 'run-001', 'unix:1', 'unix:1');
            PRAGMA user_version = 1;
            "#,
        )
        .unwrap();
        drop(conn);

        let store = AglStore::open_at(&root).unwrap();
        assert_eq!(
            store.health().unwrap().migration_version,
            CURRENT_SCHEMA_VERSION
        );
        let record = store
            .idempotency_record("cron.run", "job-001:unix:1")
            .unwrap()
            .expect("v1 idempotency record should migrate");
        assert_eq!(record.status, IdempotencyStatus::Completed);
        assert_eq!(record.result_ref.as_deref(), Some("run-001"));

        let skipped = store
            .begin_idempotency("cron.run", "job-002:unix:1", "sha256:def")
            .unwrap();
        assert!(matches!(skipped, IdempotencyOutcome::Inserted(_)));
        let skipped = store
            .skip_idempotency("cron.run", "job-002:unix:1", Some("no-op"))
            .unwrap();
        assert_eq!(skipped.status, IdempotencyStatus::Skipped);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn future_schema_version_is_rejected() {
        let root = temp_root("future-version");
        std::fs::create_dir_all(&root).unwrap();
        let db_path = database_path(&root, DEFAULT_DATABASE_FILE).unwrap();
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );
            INSERT INTO schema_migrations(version, applied_at)
            VALUES (999, 'unix:1');
            PRAGMA user_version = 999;
            "#,
        )
        .unwrap();
        drop(conn);

        let err = AglStore::open_at(&root).unwrap_err();

        assert!(matches!(
            err,
            StoreError::UnsupportedSchemaVersion {
                found: 999,
                supported: CURRENT_SCHEMA_VERSION
            }
        ));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn idempotency_replays_same_fingerprint() {
        let root = temp_root("idempotency-replay");
        let store = AglStore::open_at(&root).unwrap();

        let first = store
            .begin_idempotency("matrix", "event-001", "sha256:abc")
            .unwrap();
        let second = store
            .begin_idempotency("matrix", "event-001", "sha256:abc")
            .unwrap();

        assert!(matches!(first, IdempotencyOutcome::Inserted(_)));
        assert!(matches!(second, IdempotencyOutcome::Replayed(_)));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn idempotency_rejects_different_fingerprint() {
        let root = temp_root("idempotency-conflict");
        let store = AglStore::open_at(&root).unwrap();
        store
            .begin_idempotency("matrix", "event-001", "sha256:abc")
            .unwrap();

        let err = store
            .begin_idempotency("matrix", "event-001", "sha256:def")
            .unwrap_err();

        assert!(matches!(err, StoreError::IdempotencyConflict { .. }));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn complete_idempotency_records_result_ref() {
        let root = temp_root("idempotency-complete");
        let store = AglStore::open_at(&root).unwrap();
        store
            .begin_idempotency("matrix", "event-001", "sha256:abc")
            .unwrap();

        let record = store
            .complete_idempotency("matrix", "event-001", Some("session/turn-001"))
            .unwrap();

        assert_eq!(record.status, IdempotencyStatus::Completed);
        assert_eq!(record.result_ref.as_deref(), Some("session/turn-001"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fail_idempotency_records_failed_status() {
        let root = temp_root("idempotency-failed");
        let store = AglStore::open_at(&root).unwrap();
        store
            .begin_idempotency("cron.run", "job-001:unix:1", "sha256:abc")
            .unwrap();

        let record = store
            .fail_idempotency("cron.run", "job-001:unix:1", Some("error-001"))
            .unwrap();
        let replay = store
            .begin_idempotency("cron.run", "job-001:unix:1", "sha256:abc")
            .unwrap();

        assert_eq!(record.status, IdempotencyStatus::Failed);
        assert_eq!(record.result_ref.as_deref(), Some("error-001"));
        assert!(matches!(replay, IdempotencyOutcome::Replayed(_)));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skip_idempotency_records_skipped_status() {
        let root = temp_root("idempotency-skipped");
        let store = AglStore::open_at(&root).unwrap();
        store
            .begin_idempotency("cron.run", "job-001:unix:1", "sha256:abc")
            .unwrap();

        let record = store
            .skip_idempotency("cron.run", "job-001:unix:1", Some("not-due"))
            .unwrap();

        assert_eq!(record.status, IdempotencyStatus::Skipped);
        assert_eq!(record.result_ref.as_deref(), Some("not-due"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn database_file_rejects_path_traversal() {
        let root = temp_root("bad-path");

        let err = database_path(&root, "../agentlibre.sqlite3").unwrap_err();

        assert!(matches!(err, StoreError::InvalidPath { .. }));

        let _ = std::fs::remove_dir_all(root);
    }

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("agl-store-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }
}
