use std::fmt;
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug)]
pub enum StoreError {
    ConversationBusy {
        conversation_id: agl_core::ConversationId,
    },
    Busy {
        path: PathBuf,
    },
    IncompatibleDatabase {
        path: PathBuf,
        application_id: u32,
        version: u32,
        required_application_id: u32,
        required_version: u32,
    },
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
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    Json(serde_json::Error),
    Content(agl_core::ContentError),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConversationBusy { conversation_id } => write!(
                f,
                "Conversation {conversation_id} is busy with an unfinished Run"
            ),
            Self::Busy { path } => write!(
                f,
                "Store {} is busy; stop its daemon before rotating or opening another owner",
                path.display()
            ),
            Self::IncompatibleDatabase {
                path,
                application_id,
                version,
                required_application_id,
                required_version,
            } => write!(
                f,
                "incompatible Store {}: application {application_id:#x}, version {version}; required application {required_application_id:#x}, version {required_version}; database preserved; run `agl store rotate` to retain it and start an empty Store",
                path.display()
            ),
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
            Self::Io(err) => write!(f, "{err}"),
            Self::Sqlite(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "{err}"),
            Self::Content(error) => write!(f, "{error}"),
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

impl From<agl_core::ContentError> for StoreError {
    fn from(error: agl_core::ContentError) -> Self {
        Self::Content(error)
    }
}
