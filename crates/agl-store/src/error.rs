use std::fmt;
use std::path::PathBuf;

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
            Self::TransitionRejected { resource, from, to } => {
                write!(f, "cannot transition {resource} from {from} to {to}")
            }
            Self::LeaseLost { resource } => {
                write!(f, "lease for {resource} is no longer owned by this worker")
            }
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
