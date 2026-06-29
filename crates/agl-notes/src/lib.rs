use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use agl_store::{AglStore, StoreError};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, NotesError>;

#[derive(Debug)]
pub enum NotesError {
    InvalidValue {
        field: &'static str,
        value: String,
        reason: &'static str,
    },
    NotFound {
        id: String,
    },
    Store(StoreError),
    Sqlite(rusqlite::Error),
}

impl fmt::Display for NotesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidValue {
                field,
                value,
                reason,
            } => write!(f, "invalid note {field} value {value:?}: {reason}"),
            Self::NotFound { id } => write!(f, "note not found: {id}"),
            Self::Store(err) => write!(f, "{err}"),
            Self::Sqlite(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for NotesError {}

impl From<StoreError> for NotesError {
    fn from(err: StoreError) -> Self {
        Self::Store(err)
    }
}

impl From<rusqlite::Error> for NotesError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sqlite(err)
    }
}

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

pub struct NoteRepository<'a> {
    store: &'a AglStore,
}

impl<'a> NoteRepository<'a> {
    pub fn new(store: &'a AglStore) -> Self {
        Self { store }
    }

    pub fn add(&self, draft: NoteDraft) -> Result<Note> {
        validate_draft(&draft)?;
        let id = note_id();
        let now = timestamp();
        self.store.connection().execute(
            "INSERT INTO notes
             (id, title, body, created_at, updated_at, deleted_at)
             VALUES (?1, ?2, ?3, ?4, ?4, NULL)",
            params![id, draft.title, draft.body, now],
        )?;
        self.get(&id)?
            .ok_or_else(|| NotesError::NotFound { id: id.to_string() })
    }

    pub fn list(&self, query: &NoteSearchQuery) -> Result<Vec<Note>> {
        if query
            .text
            .as_ref()
            .is_some_and(|text| !text.trim().is_empty())
        {
            return self.search(query);
        }

        let limit = limit_i64(query.limit);
        if query.include_deleted {
            self.query_notes(
                "SELECT id, title, body, created_at, updated_at, deleted_at
                 FROM notes
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?1",
                params![limit],
            )
        } else {
            self.query_notes(
                "SELECT id, title, body, created_at, updated_at, deleted_at
                 FROM notes
                 WHERE deleted_at IS NULL
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?1",
                params![limit],
            )
        }
    }

    pub fn search(&self, query: &NoteSearchQuery) -> Result<Vec<Note>> {
        let Some(text) = query.text.as_ref().filter(|text| !text.trim().is_empty()) else {
            return self.list(query);
        };
        let escaped = format!("%{}%", escape_like(text));
        let limit = limit_i64(query.limit);
        if query.include_deleted {
            self.query_notes(
                "SELECT id, title, body, created_at, updated_at, deleted_at
                 FROM notes
                 WHERE title LIKE ?1 ESCAPE '\\' OR body LIKE ?1 ESCAPE '\\'
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?2",
                params![escaped, limit],
            )
        } else {
            self.query_notes(
                "SELECT id, title, body, created_at, updated_at, deleted_at
                 FROM notes
                 WHERE deleted_at IS NULL AND (title LIKE ?1 ESCAPE '\\' OR body LIKE ?1 ESCAPE '\\')
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?2",
                params![escaped, limit],
            )
        }
    }

    pub fn get(&self, id: &str) -> Result<Option<Note>> {
        validate_non_blank("id", id)?;
        self.store
            .connection()
            .query_row(
                "SELECT id, title, body, created_at, updated_at, deleted_at
                 FROM notes
                 WHERE id = ?1",
                params![id],
                note_from_row,
            )
            .optional()
            .map_err(NotesError::from)
    }

    pub fn update(&self, id: &str, update: NoteUpdate) -> Result<Note> {
        validate_non_blank("id", id)?;
        if update.title.is_none() && update.body.is_none() {
            return Err(NotesError::InvalidValue {
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
        let current = self
            .get(id)?
            .ok_or_else(|| NotesError::NotFound { id: id.to_string() })?;
        let title = update.title.unwrap_or(current.title);
        let body = update.body.unwrap_or(current.body);
        let now = timestamp();
        self.store.connection().execute(
            "UPDATE notes
             SET title = ?2, body = ?3, updated_at = ?4
             WHERE id = ?1",
            params![id, title, body, now],
        )?;
        self.get(id)?
            .ok_or_else(|| NotesError::NotFound { id: id.to_string() })
    }

    pub fn delete(&self, id: &str) -> Result<Note> {
        validate_non_blank("id", id)?;
        let now = timestamp();
        self.store.connection().execute(
            "UPDATE notes
             SET deleted_at = COALESCE(deleted_at, ?2), updated_at = ?2
             WHERE id = ?1",
            params![id, now],
        )?;
        self.get(id)?
            .ok_or_else(|| NotesError::NotFound { id: id.to_string() })
    }

    pub fn link(&self, note_id: &str, target_ref: &str, label: Option<String>) -> Result<NoteLink> {
        validate_non_blank("note_id", note_id)?;
        validate_non_blank("target_ref", target_ref)?;
        if let Some(label) = &label {
            validate_non_blank("label", label)?;
        }
        let note = self.get(note_id)?.ok_or_else(|| NotesError::NotFound {
            id: note_id.to_string(),
        })?;
        if note.deleted_at.is_some() {
            return Err(NotesError::InvalidValue {
                field: "note_id",
                value: note_id.to_string(),
                reason: "cannot link a deleted note",
            });
        }
        let id = link_id();
        let now = timestamp();
        self.store.connection().execute(
            "INSERT INTO note_links
             (id, note_id, target_ref, label, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, note_id, target_ref, label, now],
        )?;
        self.link_by_id(&id)?
            .ok_or_else(|| NotesError::NotFound { id })
    }

    pub fn links(&self, note_id: &str) -> Result<Vec<NoteLink>> {
        validate_non_blank("note_id", note_id)?;
        let mut stmt = self.store.connection().prepare(
            "SELECT id, note_id, target_ref, label, created_at
             FROM note_links
             WHERE note_id = ?1
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![note_id], note_link_from_row)?;
        let mut links = Vec::new();
        for row in rows {
            links.push(row?);
        }
        Ok(links)
    }

    fn link_by_id(&self, id: &str) -> Result<Option<NoteLink>> {
        self.store
            .connection()
            .query_row(
                "SELECT id, note_id, target_ref, label, created_at
                 FROM note_links
                 WHERE id = ?1",
                params![id],
                note_link_from_row,
            )
            .optional()
            .map_err(NotesError::from)
    }

    fn query_notes<P>(&self, sql: &str, params: P) -> Result<Vec<Note>>
    where
        P: rusqlite::Params,
    {
        let mut stmt = self.store.connection().prepare(sql)?;
        let rows = stmt.query_map(params, note_from_row)?;
        let mut notes = Vec::new();
        for row in rows {
            notes.push(row?);
        }
        Ok(notes)
    }
}

fn note_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Note> {
    Ok(Note {
        id: row.get(0)?,
        title: row.get(1)?,
        body: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        deleted_at: row.get(5)?,
    })
}

fn note_link_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<NoteLink> {
    Ok(NoteLink {
        id: row.get(0)?,
        note_id: row.get(1)?,
        target_ref: row.get(2)?,
        label: row.get(3)?,
        created_at: row.get(4)?,
    })
}

fn validate_draft(draft: &NoteDraft) -> Result<()> {
    validate_non_blank("title", &draft.title)?;
    validate_non_blank("body", &draft.body)
}

fn validate_non_blank(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(NotesError::InvalidValue {
            field,
            value: value.to_string(),
            reason: "value cannot be blank",
        });
    }
    Ok(())
}

fn limit_i64(limit: usize) -> i64 {
    i64::try_from(limit.max(1)).unwrap_or(i64::MAX)
}

fn escape_like(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '%' | '_' | '\\' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("unix:{seconds}")
}

fn note_id() -> String {
    stable_id("note")
}

fn link_id() -> String {
    stable_id("note_link")
}

fn stable_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{prefix}_{}_{}", std::process::id(), nanos)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn adds_and_searches_notes() {
        let root = temp_root("search");
        let store = AglStore::open_at(&root).unwrap();
        let repo = NoteRepository::new(&store);
        let note = repo
            .add(NoteDraft::new("Repository workflow", "Use pinned skills."))
            .unwrap();

        let results = repo
            .search(&NoteSearchQuery {
                text: Some("pinned".to_string()),
                ..NoteSearchQuery::default()
            })
            .unwrap();

        assert_eq!(results, vec![note]);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn update_changes_title_and_body() {
        let root = temp_root("update");
        let store = AglStore::open_at(&root).unwrap();
        let repo = NoteRepository::new(&store);
        let note = repo.add(NoteDraft::new("Old", "Old body")).unwrap();

        let updated = repo
            .update(
                &note.id,
                NoteUpdate {
                    title: Some("New".to_string()),
                    body: Some("New body".to_string()),
                },
            )
            .unwrap();

        assert_eq!(updated.title, "New");
        assert_eq!(updated.body, "New body");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn delete_hides_note_by_default() {
        let root = temp_root("delete");
        let store = AglStore::open_at(&root).unwrap();
        let repo = NoteRepository::new(&store);
        let note = repo.add(NoteDraft::new("Temporary", "Delete me.")).unwrap();

        let deleted = repo.delete(&note.id).unwrap();
        let hidden = repo.list(&NoteSearchQuery::default()).unwrap();
        let visible = repo
            .list(&NoteSearchQuery {
                include_deleted: true,
                ..NoteSearchQuery::default()
            })
            .unwrap();

        assert!(deleted.deleted_at.is_some());
        assert!(hidden.is_empty());
        assert_eq!(visible.len(), 1);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn links_notes_to_target_refs() {
        let root = temp_root("link");
        let store = AglStore::open_at(&root).unwrap();
        let repo = NoteRepository::new(&store);
        let note = repo.add(NoteDraft::new("Memory", "Promote this.")).unwrap();

        let link = repo
            .link(&note.id, "memory:mem_001", Some("remembered".to_string()))
            .unwrap();
        let links = repo.links(&note.id).unwrap();

        assert_eq!(links, vec![link]);
        assert_eq!(links[0].target_ref, "memory:mem_001");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_blank_note_body() {
        let root = temp_root("blank");
        let store = AglStore::open_at(&root).unwrap();
        let repo = NoteRepository::new(&store);

        let err = repo.add(NoteDraft::new("Blank", " ")).unwrap_err();

        assert!(matches!(
            err,
            NotesError::InvalidValue { field: "body", .. }
        ));

        std::fs::remove_dir_all(root).unwrap();
    }

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("agl-notes-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }
}
