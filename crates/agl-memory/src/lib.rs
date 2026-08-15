use std::fmt;

use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, MemoryError>;

const DEFAULT_USER_SCOPE_KEY: &str = "default";
const DEFAULT_CONFIDENCE: u8 = 100;

#[derive(Debug)]
pub enum MemoryError {
    InvalidValue {
        field: &'static str,
        value: String,
        reason: &'static str,
    },
    NotFound {
        id: String,
    },
    Repository {
        reason: String,
    },
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidValue {
                field,
                value,
                reason,
            } => write!(f, "invalid memory {field} value {value:?}: {reason}"),
            Self::NotFound { id } => write!(f, "memory entry not found: {id}"),
            Self::Repository { reason } => write!(f, "memory repository failed: {reason}"),
        }
    }
}

impl std::error::Error for MemoryError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryScopeKind {
    User,
    Repo,
    MatrixRoom,
    MatrixUser,
}

impl MemoryScopeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Repo => "repo",
            Self::MatrixRoom => "matrix_room",
            Self::MatrixUser => "matrix_user",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "user" => Ok(Self::User),
            "repo" => Ok(Self::Repo),
            "matrix_room" => Ok(Self::MatrixRoom),
            "matrix_user" => Ok(Self::MatrixUser),
            _ => Err(MemoryError::InvalidValue {
                field: "scope_kind",
                value: value.to_owned(),
                reason: "unknown memory scope kind",
            }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MemoryScope {
    pub kind: MemoryScopeKind,
    pub key: String,
}

impl MemoryScope {
    pub fn user() -> Self {
        Self {
            kind: MemoryScopeKind::User,
            key: DEFAULT_USER_SCOPE_KEY.to_owned(),
        }
    }

    pub fn new(kind: MemoryScopeKind, key: impl Into<String>) -> Result<Self> {
        let key = key.into();
        validate_non_blank("scope_key", &key)?;
        Ok(Self { kind, key })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Fact,
    Preference,
    Summary,
    Decision,
    WorkingNote,
}

impl MemoryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Preference => "preference",
            Self::Summary => "summary",
            Self::Decision => "decision",
            Self::WorkingNote => "working_note",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "fact" => Ok(Self::Fact),
            "preference" => Ok(Self::Preference),
            "summary" => Ok(Self::Summary),
            "decision" => Ok(Self::Decision),
            "working_note" => Ok(Self::WorkingNote),
            _ => Err(MemoryError::InvalidValue {
                field: "kind",
                value: value.to_owned(),
                reason: "unknown memory kind",
            }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub id: String,
    pub scope: MemoryScope,
    pub kind: MemoryKind,
    pub title: String,
    pub body: String,
    pub source_ref: Option<String>,
    pub confidence: u8,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySuggestionStatus {
    Pending,
    Approved,
    Rejected,
}

impl MemorySuggestionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "rejected" => Ok(Self::Rejected),
            _ => Err(MemoryError::InvalidValue {
                field: "suggestion_status",
                value: value.to_owned(),
                reason: "unknown memory suggestion status",
            }),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MemorySuggestion {
    pub id: String,
    pub scope: MemoryScope,
    pub kind: MemoryKind,
    pub title: String,
    pub body: String,
    pub source_ref: String,
    pub confidence: u8,
    pub status: MemorySuggestionStatus,
    pub created_at: String,
    pub updated_at: String,
    pub resolved_at: Option<String>,
    pub resolution_ref: Option<String>,
    pub resolution_note: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryDraft {
    pub scope: MemoryScope,
    pub kind: MemoryKind,
    pub title: String,
    pub body: String,
    pub source_ref: Option<String>,
    pub confidence: u8,
}

impl MemoryDraft {
    pub fn new(
        scope: MemoryScope,
        kind: MemoryKind,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            scope,
            kind,
            title: title.into(),
            body: body.into(),
            source_ref: None,
            confidence: DEFAULT_CONFIDENCE,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemorySuggestionDraft {
    pub scope: MemoryScope,
    pub kind: MemoryKind,
    pub title: String,
    pub body: String,
    pub source_ref: String,
    pub confidence: u8,
}

impl MemorySuggestionDraft {
    pub fn new(
        scope: MemoryScope,
        kind: MemoryKind,
        title: impl Into<String>,
        body: impl Into<String>,
        source_ref: impl Into<String>,
    ) -> Self {
        Self {
            scope,
            kind,
            title: title.into(),
            body: body.into(),
            source_ref: source_ref.into(),
            confidence: DEFAULT_CONFIDENCE,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemorySearchQuery {
    pub scope: Option<MemoryScope>,
    pub text: Option<String>,
    pub include_deleted: bool,
    pub limit: usize,
}

impl MemorySearchQuery {
    pub fn scoped(scope: MemoryScope) -> Self {
        Self {
            scope: Some(scope),
            text: None,
            include_deleted: false,
            limit: 50,
        }
    }

    pub fn text(scope: Option<MemoryScope>, text: impl Into<String>) -> Self {
        Self {
            scope,
            text: Some(text.into()),
            include_deleted: false,
            limit: 50,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemorySuggestionQuery {
    pub scope: Option<MemoryScope>,
    pub status: Option<MemorySuggestionStatus>,
    pub limit: usize,
}

impl MemorySuggestionQuery {
    pub fn pending(scope: Option<MemoryScope>) -> Self {
        Self {
            scope,
            status: Some(MemorySuggestionStatus::Pending),
            limit: 50,
        }
    }
}

pub trait MemoryRepository: Send + Sync {
    fn add(&self, draft: MemoryDraft) -> Result<MemoryEntry>;
    fn list(&self, query: &MemorySearchQuery) -> Result<Vec<MemoryEntry>>;
    fn search(&self, query: &MemorySearchQuery) -> Result<Vec<MemoryEntry>>;
    fn get(&self, id: &str) -> Result<Option<MemoryEntry>>;
    fn delete(&self, id: &str) -> Result<MemoryEntry>;
    fn suggest(&self, draft: MemorySuggestionDraft) -> Result<MemorySuggestion>;
    fn list_suggestions(&self, query: &MemorySuggestionQuery) -> Result<Vec<MemorySuggestion>>;
    fn get_suggestion(&self, id: &str) -> Result<Option<MemorySuggestion>>;
    fn approve_suggestion(&self, id: &str) -> Result<(MemorySuggestion, MemoryEntry)>;
    fn reject_suggestion(
        &self,
        id: &str,
        resolution_note: Option<&str>,
    ) -> Result<MemorySuggestion>;
}

pub fn validate_memory_draft(draft: &MemoryDraft) -> Result<()> {
    validate_non_blank("scope_key", &draft.scope.key)?;
    validate_non_blank("title", &draft.title)?;
    validate_non_blank("body", &draft.body)?;
    validate_confidence(draft.confidence)
}

pub fn validate_memory_suggestion_draft(draft: &MemorySuggestionDraft) -> Result<()> {
    validate_non_blank("scope_key", &draft.scope.key)?;
    validate_non_blank("title", &draft.title)?;
    validate_non_blank("body", &draft.body)?;
    validate_non_blank("source_ref", &draft.source_ref)?;
    validate_confidence(draft.confidence)
}

pub fn validate_non_blank(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(MemoryError::InvalidValue {
            field,
            value: value.to_owned(),
            reason: "value cannot be blank",
        });
    }
    Ok(())
}

fn validate_confidence(confidence: u8) -> Result<()> {
    if confidence > 100 {
        return Err(MemoryError::InvalidValue {
            field: "confidence",
            value: confidence.to_string(),
            reason: "confidence must be between 0 and 100",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests;
