use std::fmt;

use agl_memory::{MemoryEntry, MemoryKind, MemoryScope};
use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, NoteError>;

#[derive(Debug)]
pub enum NoteError {
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

impl fmt::Display for NoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidValue {
                field,
                value,
                reason,
            } => write!(f, "invalid note {field} value {value:?}: {reason}"),
            Self::NotFound { id } => write!(f, "note not found: {id}"),
            Self::Repository { reason } => write!(f, "note repository failed: {reason}"),
        }
    }
}

impl std::error::Error for NoteError {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: String,
    pub title: String,
    pub body: String,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoteDraft {
    pub title: String,
    pub body: String,
}

impl NoteDraft {
    pub fn new(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NoteUpdate {
    pub title: Option<String>,
    pub body: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NoteLink {
    pub id: String,
    pub note_id: String,
    pub target_ref: String,
    pub label: Option<String>,
    pub created_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NoteMemoryPromotion {
    pub note: Note,
    pub memory: MemoryEntry,
    pub link: NoteLink,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoteSearchQuery {
    pub text: Option<String>,
    pub include_deleted: bool,
    pub limit: usize,
}

impl Default for NoteSearchQuery {
    fn default() -> Self {
        Self {
            text: None,
            include_deleted: false,
            limit: 50,
        }
    }
}

pub trait NoteRepository: Send + Sync {
    fn add(&self, draft: NoteDraft) -> Result<Note>;
    fn list(&self, query: &NoteSearchQuery) -> Result<Vec<Note>>;
    fn search(&self, query: &NoteSearchQuery) -> Result<Vec<Note>>;
    fn get(&self, id: &str) -> Result<Option<Note>>;
    fn update(&self, id: &str, update: NoteUpdate) -> Result<Note>;
    fn delete(&self, id: &str) -> Result<Note>;
    fn link(&self, note_id: &str, target_ref: &str, label: Option<String>) -> Result<NoteLink>;
    fn remember(
        &self,
        note_id: &str,
        scope: MemoryScope,
        kind: MemoryKind,
    ) -> Result<NoteMemoryPromotion>;
    fn links(&self, note_id: &str) -> Result<Vec<NoteLink>>;
}

pub fn validate_note_draft(draft: &NoteDraft) -> Result<()> {
    validate_non_blank("title", &draft.title)?;
    validate_non_blank("body", &draft.body)
}

pub fn validate_note_update(update: &NoteUpdate) -> Result<()> {
    if update.title.is_none() && update.body.is_none() {
        return Err(NoteError::InvalidValue {
            field: "update",
            value: String::new(),
            reason: "update must change title or body",
        });
    }
    if let Some(title) = &update.title {
        validate_non_blank("title", title)?;
    }
    if let Some(body) = &update.body {
        validate_non_blank("body", body)?;
    }
    Ok(())
}

pub fn validate_non_blank(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(NoteError::InvalidValue {
            field,
            value: value.to_owned(),
            reason: "value cannot be blank",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_note_without_sqlite() {
        assert!(validate_note_draft(&NoteDraft::new("title", "body")).is_ok());
        assert!(matches!(
            validate_note_draft(&NoteDraft::new(" ", "body")),
            Err(NoteError::InvalidValue { field: "title", .. })
        ));
        assert!(validate_note_update(&NoteUpdate::default()).is_err());
    }
}
