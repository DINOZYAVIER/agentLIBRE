use std::fmt;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agl_runtime::AgentLibrePaths;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const DEFAULT_DATABASE_FILE: &str = "agentlibre.sqlite3";
pub const CURRENT_SCHEMA_VERSION: u32 = 7;

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
}

impl StoreDomain {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Notes => "notes",
            Self::Cron => "cron",
        }
    }

    pub fn all() -> [Self; 3] {
        [Self::Memory, Self::Notes, Self::Cron]
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
    fn as_str(self) -> &'static str {
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
            assert_eq!(domain.total_rows, 2, "domain={}", domain.domain.as_str());
            assert_eq!(domain.active_rows, 1, "domain={}", domain.domain.as_str());
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
