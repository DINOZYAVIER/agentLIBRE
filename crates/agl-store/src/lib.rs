use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agl_runtime::AgentLibrePaths;
use rusqlite::{Connection, OptionalExtension, params};

pub const DEFAULT_DATABASE_FILE: &str = "agentlibre.sqlite3";
pub const CURRENT_SCHEMA_VERSION: u32 = 5;

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
    UnsupportedSchemaVersion {
        found: u32,
        supported: u32,
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
            Self::UnsupportedSchemaVersion { found, supported } => write!(
                f,
                "unsupported store schema version {found}; this build supports up to {supported}"
            ),
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

    pub fn health(&self) -> Result<StoreHealth> {
        Ok(StoreHealth {
            database_path: self.database_path.clone(),
            migration_version: self.schema_version()?,
        })
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

    fn migrate(&self) -> Result<()> {
        self.ensure_migration_table()?;
        let current_version = self.schema_version()?;
        if current_version > CURRENT_SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchemaVersion {
                found: current_version,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }
        for version in self.applied_migration_versions()? {
            if version > CURRENT_SCHEMA_VERSION {
                return Err(StoreError::UnsupportedSchemaVersion {
                    found: version,
                    supported: CURRENT_SCHEMA_VERSION,
                });
            }
        }
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

fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("unix:{seconds}")
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

        assert_eq!(health.migration_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(
            health.database_path,
            root.join("data/store/agentlibre.sqlite3")
        );

        let _ = std::fs::remove_dir_all(root);
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
