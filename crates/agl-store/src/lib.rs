use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agl_runtime::AgentLibrePaths;
use rusqlite::{Connection, OptionalExtension, params};

pub const DEFAULT_DATABASE_FILE: &str = "agentlibre.sqlite3";
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

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
}

impl IdempotencyStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "in_progress" => Ok(Self::InProgress),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
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
        validate_idempotency_part(namespace, "namespace")?;
        validate_idempotency_part(key, "key")?;
        let now = timestamp();
        self.conn.execute(
            "UPDATE idempotency_keys
             SET status = ?3, result_ref = ?4, updated_at = ?5
             WHERE namespace = ?1 AND key = ?2",
            params![
                namespace,
                key,
                IdempotencyStatus::Completed.as_str(),
                result_ref,
                now
            ],
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
        self.conn.execute_batch(
            r#"
            BEGIN;
            CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );
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
            INSERT OR IGNORE INTO schema_migrations(version, applied_at)
            VALUES (1, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'));
            PRAGMA user_version = 1;
            COMMIT;
            "#,
        )?;
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
        assert_eq!(first.health().unwrap().migration_version, 1);
        drop(first);

        let second = AglStore::open_at(&root).unwrap();
        assert_eq!(second.health().unwrap().migration_version, 1);

        let _ = std::fs::remove_dir_all(root);
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
