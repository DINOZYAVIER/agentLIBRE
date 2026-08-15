use std::time::{SystemTime, UNIX_EPOCH};

use agl_memory::{MemoryDraft, MemoryKind, MemoryScope};
use agl_note::{
    Note, NoteDraft, NoteError, NoteLink, NoteMemoryPromotion, NoteRepository, NoteSearchQuery,
    NoteUpdate, validate_non_blank, validate_note_draft, validate_note_update,
};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::memory::insert_memory_on_connection;
use crate::{AglStore, StoreHandle};

impl NoteRepository for StoreHandle {
    fn add(&self, draft: NoteDraft) -> agl_note::Result<Note> {
        note_add(&*self.lock().map_err(repository_error)?, draft)
    }

    fn list(&self, query: &NoteSearchQuery) -> agl_note::Result<Vec<Note>> {
        note_list(&*self.lock().map_err(repository_error)?, query)
    }

    fn search(&self, query: &NoteSearchQuery) -> agl_note::Result<Vec<Note>> {
        note_search(&*self.lock().map_err(repository_error)?, query)
    }

    fn get(&self, id: &str) -> agl_note::Result<Option<Note>> {
        note_get(&*self.lock().map_err(repository_error)?, id)
    }

    fn update(&self, id: &str, update: NoteUpdate) -> agl_note::Result<Note> {
        note_update(&*self.lock().map_err(repository_error)?, id, update)
    }

    fn delete(&self, id: &str) -> agl_note::Result<Note> {
        note_delete(&*self.lock().map_err(repository_error)?, id)
    }

    fn link(
        &self,
        note_id: &str,
        target_ref: &str,
        label: Option<String>,
    ) -> agl_note::Result<NoteLink> {
        note_link(
            &*self.lock().map_err(repository_error)?,
            note_id,
            target_ref,
            label,
        )
    }

    fn remember(
        &self,
        note_id: &str,
        scope: MemoryScope,
        kind: MemoryKind,
    ) -> agl_note::Result<NoteMemoryPromotion> {
        note_remember(
            &*self.lock().map_err(repository_error)?,
            note_id,
            scope,
            kind,
        )
    }

    fn links(&self, note_id: &str) -> agl_note::Result<Vec<NoteLink>> {
        note_links(&*self.lock().map_err(repository_error)?, note_id)
    }
}

fn note_add(store: &AglStore, draft: NoteDraft) -> agl_note::Result<Note> {
    validate_note_draft(&draft)?;
    let id = unique_id("note");
    let now = timestamp();
    store
        .connection()
        .execute(
            "INSERT INTO notes (id, title, body, created_at, updated_at, deleted_at)
             VALUES (?1, ?2, ?3, ?4, ?4, NULL)",
            params![id, draft.title, draft.body, now],
        )
        .map_err(repository_error)?;
    note_get(store, &id)?.ok_or(NoteError::NotFound { id })
}

fn note_list(store: &AglStore, query: &NoteSearchQuery) -> agl_note::Result<Vec<Note>> {
    if query
        .text
        .as_ref()
        .is_some_and(|text| !text.trim().is_empty())
    {
        return note_search(store, query);
    }
    filter_notes(all_notes(store)?, query, None)
}

fn note_search(store: &AglStore, query: &NoteSearchQuery) -> agl_note::Result<Vec<Note>> {
    let Some(text) = query.text.as_ref().filter(|text| !text.trim().is_empty()) else {
        return note_list(store, query);
    };
    filter_notes(all_notes(store)?, query, Some(&text.to_lowercase()))
}

fn all_notes(store: &AglStore) -> agl_note::Result<Vec<Note>> {
    let mut statement = store
        .connection()
        .prepare(
            "SELECT id, title, body, created_at, updated_at, deleted_at
             FROM notes ORDER BY updated_at DESC, id DESC",
        )
        .map_err(repository_error)?;
    let rows = statement
        .query_map([], note_from_row)
        .map_err(repository_error)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(repository_error)
}

fn filter_notes(
    notes: Vec<Note>,
    query: &NoteSearchQuery,
    text: Option<&str>,
) -> agl_note::Result<Vec<Note>> {
    Ok(notes
        .into_iter()
        .filter(|note| query.include_deleted || note.deleted_at.is_none())
        .filter(|note| {
            text.is_none_or(|text| {
                note.title.to_lowercase().contains(text) || note.body.to_lowercase().contains(text)
            })
        })
        .take(query.limit.max(1))
        .collect())
}

fn note_get(store: &AglStore, id: &str) -> agl_note::Result<Option<Note>> {
    validate_non_blank("id", id)?;
    note_get_on_connection(store.connection(), id)
}

fn note_get_on_connection(conn: &Connection, id: &str) -> agl_note::Result<Option<Note>> {
    conn.query_row(
        "SELECT id, title, body, created_at, updated_at, deleted_at
         FROM notes WHERE id = ?1",
        params![id],
        note_from_row,
    )
    .optional()
    .map_err(repository_error)
}

fn note_update(store: &AglStore, id: &str, update: NoteUpdate) -> agl_note::Result<Note> {
    validate_non_blank("id", id)?;
    validate_note_update(&update)?;
    let current = note_get(store, id)?.ok_or_else(|| NoteError::NotFound { id: id.to_owned() })?;
    if current.deleted_at.is_some() {
        return Err(NoteError::InvalidValue {
            field: "id",
            value: id.to_owned(),
            reason: "cannot update a deleted note",
        });
    }
    let title = update.title.unwrap_or(current.title);
    let body = update.body.unwrap_or(current.body);
    store
        .connection()
        .execute(
            "UPDATE notes SET title = ?2, body = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, title, body, timestamp()],
        )
        .map_err(repository_error)?;
    note_get(store, id)?.ok_or_else(|| NoteError::NotFound { id: id.to_owned() })
}

fn note_delete(store: &AglStore, id: &str) -> agl_note::Result<Note> {
    validate_non_blank("id", id)?;
    store
        .connection()
        .execute(
            "UPDATE notes
             SET deleted_at = COALESCE(deleted_at, ?2), updated_at = ?2
             WHERE id = ?1",
            params![id, timestamp()],
        )
        .map_err(repository_error)?;
    note_get(store, id)?.ok_or_else(|| NoteError::NotFound { id: id.to_owned() })
}

fn note_link(
    store: &AglStore,
    note_id: &str,
    target_ref: &str,
    label: Option<String>,
) -> agl_note::Result<NoteLink> {
    validate_non_blank("note_id", note_id)?;
    validate_non_blank("target_ref", target_ref)?;
    if let Some(label) = &label {
        validate_non_blank("label", label)?;
    }
    let note = note_get(store, note_id)?.ok_or_else(|| NoteError::NotFound {
        id: note_id.to_owned(),
    })?;
    if note.deleted_at.is_some() {
        return Err(NoteError::InvalidValue {
            field: "note_id",
            value: note_id.to_owned(),
            reason: "cannot link a deleted note",
        });
    }
    insert_link(store.connection(), note_id, target_ref, label)
}

fn note_remember(
    store: &AglStore,
    note_id: &str,
    scope: MemoryScope,
    kind: MemoryKind,
) -> agl_note::Result<NoteMemoryPromotion> {
    validate_non_blank("note_id", note_id)?;
    let tx = store
        .connection()
        .unchecked_transaction()
        .map_err(repository_error)?;
    let note = note_get_on_connection(&tx, note_id)?.ok_or_else(|| NoteError::NotFound {
        id: note_id.to_owned(),
    })?;
    if note.deleted_at.is_some() {
        return Err(NoteError::InvalidValue {
            field: "note_id",
            value: note_id.to_owned(),
            reason: "cannot promote a deleted note",
        });
    }
    let mut draft = MemoryDraft::new(scope, kind, note.title.clone(), note.body.clone());
    draft.source_ref = Some(format!("note:{}", note.id));
    let memory =
        insert_memory_on_connection(&tx, draft).map_err(|error| NoteError::Repository {
            reason: error.to_string(),
        })?;
    let link = insert_link(
        &tx,
        &note.id,
        &format!("memory:{}", memory.id),
        Some("remembered".to_owned()),
    )?;
    tx.commit().map_err(repository_error)?;
    Ok(NoteMemoryPromotion { note, memory, link })
}

fn note_links(store: &AglStore, note_id: &str) -> agl_note::Result<Vec<NoteLink>> {
    validate_non_blank("note_id", note_id)?;
    let mut statement = store
        .connection()
        .prepare(
            "SELECT id, note_id, target_ref, label, created_at
             FROM note_links WHERE note_id = ?1 ORDER BY created_at ASC, id ASC",
        )
        .map_err(repository_error)?;
    let rows = statement
        .query_map(params![note_id], note_link_from_row)
        .map_err(repository_error)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(repository_error)
}

fn insert_link(
    conn: &Connection,
    note_id: &str,
    target_ref: &str,
    label: Option<String>,
) -> agl_note::Result<NoteLink> {
    let link = NoteLink {
        id: unique_id("note_link"),
        note_id: note_id.to_owned(),
        target_ref: target_ref.to_owned(),
        label,
        created_at: timestamp(),
    };
    conn.execute(
        "INSERT INTO note_links (id, note_id, target_ref, label, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            link.id,
            link.note_id,
            link.target_ref,
            link.label,
            link.created_at,
        ],
    )
    .map_err(repository_error)?;
    Ok(link)
}

fn note_from_row(row: &Row<'_>) -> rusqlite::Result<Note> {
    Ok(Note {
        id: row.get(0)?,
        title: row.get(1)?,
        body: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        deleted_at: row.get(5)?,
    })
}

fn note_link_from_row(row: &Row<'_>) -> rusqlite::Result<NoteLink> {
    Ok(NoteLink {
        id: row.get(0)?,
        note_id: row.get(1)?,
        target_ref: row.get(2)?,
        label: row.get(3)?,
        created_at: row.get(4)?,
    })
}

fn repository_error(error: impl std::fmt::Display) -> NoteError {
    NoteError::Repository {
        reason: error.to_string(),
    }
}

fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("unix:{seconds}")
}

fn unique_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{prefix}_{}_{}", std::process::id(), nanos)
}
