mod admissions;
mod executions;
mod terminals;

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, Transaction, TransactionBehavior};

pub(crate) use admissions::{PersistedAdmission, SqliteAdmissionRepository};
pub use executions::SqliteExecutionRepository;
pub use terminals::SqliteTerminalRepository;

const SCHEMA_VERSION: u32 = 1;
const DATABASE_FILE: &str = "terminal.sqlite3";

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
    TransitionRejected {
        resource: String,
        from: String,
        to: String,
    },
    LeaseLost {
        resource: String,
    },
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    UnsupportedSchemaVersion {
        found: u32,
        supported: u32,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath { path, reason } => write!(
                formatter,
                "invalid terminal store path {}: {reason}",
                path.display()
            ),
            Self::InvalidValue {
                field,
                value,
                reason,
            } => write!(formatter, "invalid {field} value {value:?}: {reason}"),
            Self::NotFound { resource } => write!(formatter, "{resource} not found"),
            Self::TransitionRejected { resource, from, to } => write!(
                formatter,
                "cannot transition {resource} from {from} to {to}"
            ),
            Self::LeaseLost { resource } => write!(
                formatter,
                "lease for {resource} is no longer owned by this terminal service"
            ),
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Sqlite(error) => write!(formatter, "{error}"),
            Self::Json(error) => write!(formatter, "{error}"),
            Self::UnsupportedSchemaVersion { found, supported } => write!(
                formatter,
                "unsupported terminal schema version {found}; this build supports {supported}"
            ),
        }
    }
}

impl std::error::Error for StoreError {}
impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<StoreError> for agl_exec::ProcessError {
    fn from(error: StoreError) -> Self {
        let code = match &error {
            StoreError::LeaseLost { .. } | StoreError::TransitionRejected { .. } => {
                agl_exec::ProcessErrorCode::StateConflict
            }
            StoreError::NotFound { .. } => agl_exec::ProcessErrorCode::ExecutionNotFound,
            _ => agl_exec::ProcessErrorCode::StoreCorrupt,
        };
        Self::new(code, error.to_string())
    }
}

struct TerminalStore {
    connection: Connection,
    database_path: PathBuf,
}

impl TerminalStore {
    fn open_at(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        ensure_private_dir(root)?;
        let database_path = root.join(DATABASE_FILE);
        let connection = Connection::open(&database_path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL;",
        )?;
        let found = connection.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))?;
        if found == 0 {
            apply_initial_schema(&connection)?;
        } else if found != SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchemaVersion {
                found,
                supported: SCHEMA_VERSION,
            });
        }
        secure_database_files(&database_path)?;
        Ok(Self {
            connection,
            database_path,
        })
    }

    fn connection(&self) -> &Connection {
        &self.connection
    }

    fn transaction<T>(&self, operation: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        match operation(&transaction) {
            Ok(value) => {
                transaction.commit()?;
                secure_database_files(&self.database_path)?;
                Ok(value)
            }
            Err(error) => {
                let _ = transaction.rollback();
                Err(error)
            }
        }
    }
}

fn apply_initial_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(include_str!("schema-v1.sql"))?;
    Ok(())
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
fn secure_database_files(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        if candidate.exists() {
            std::fs::set_permissions(candidate, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn secure_database_files(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temporary_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "agl-terminal-store-{label}-{}-{nonce}",
            std::process::id()
        ))
    }

    #[test]
    fn initial_schema_contains_only_terminal_owned_state() {
        let root = temporary_root("schema");
        let store = TerminalStore::open_at(&root).unwrap();
        let mut statement = store
            .connection()
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let tables = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            tables,
            [
                "execution_events",
                "executions",
                "service_admissions",
                "terminal_sessions"
            ]
        );
        assert!(
            !tables
                .iter()
                .any(|name| name.contains("run") || name.contains("session_transcript"))
        );
        assert_eq!(
            store
                .connection()
                .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        drop(statement);
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unknown_schema_fails_closed_without_migration() {
        let root = temporary_root("unknown");
        ensure_private_dir(&root).unwrap();
        let path = root.join(DATABASE_FILE);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA user_version=99; CREATE TABLE obsolete_agent_state(id TEXT);")
            .unwrap();
        drop(connection);
        assert!(matches!(
            TerminalStore::open_at(&root),
            Err(StoreError::UnsupportedSchemaVersion {
                found: 99,
                supported: SCHEMA_VERSION
            })
        ));
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM obsolete_agent_state", [], |row| row
                    .get::<_, u64>(
                    0
                ))
                .unwrap(),
            0
        );
        drop(connection);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn database_and_root_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = temporary_root("permissions");
        let store = TerminalStore::open_at(&root).unwrap();
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&store.database_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(store);
        let _ = std::fs::remove_dir_all(root);
    }
}
