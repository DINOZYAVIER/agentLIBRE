use std::time::{SystemTime, UNIX_EPOCH};

use agl_memory::{
    MemoryDraft, MemoryEntry, MemoryError, MemoryKind, MemoryRepository, MemoryScope,
    MemoryScopeKind, MemorySearchQuery, MemorySuggestion, MemorySuggestionDraft,
    MemorySuggestionQuery, MemorySuggestionStatus, validate_memory_draft,
    validate_memory_suggestion_draft, validate_non_blank,
};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::{AglStore, StoreHandle};

impl MemoryRepository for StoreHandle {
    fn add(&self, draft: MemoryDraft) -> agl_memory::Result<MemoryEntry> {
        memory_add(&*self.lock().map_err(repository_error)?, draft)
    }

    fn list(&self, query: &MemorySearchQuery) -> agl_memory::Result<Vec<MemoryEntry>> {
        memory_list(&*self.lock().map_err(repository_error)?, query)
    }

    fn search(&self, query: &MemorySearchQuery) -> agl_memory::Result<Vec<MemoryEntry>> {
        memory_search(&*self.lock().map_err(repository_error)?, query)
    }

    fn get(&self, id: &str) -> agl_memory::Result<Option<MemoryEntry>> {
        memory_get(&*self.lock().map_err(repository_error)?, id)
    }

    fn delete(&self, id: &str) -> agl_memory::Result<MemoryEntry> {
        memory_delete(&*self.lock().map_err(repository_error)?, id)
    }

    fn suggest(&self, draft: MemorySuggestionDraft) -> agl_memory::Result<MemorySuggestion> {
        memory_suggest(&*self.lock().map_err(repository_error)?, draft)
    }

    fn list_suggestions(
        &self,
        query: &MemorySuggestionQuery,
    ) -> agl_memory::Result<Vec<MemorySuggestion>> {
        memory_list_suggestions(&*self.lock().map_err(repository_error)?, query)
    }

    fn get_suggestion(&self, id: &str) -> agl_memory::Result<Option<MemorySuggestion>> {
        memory_get_suggestion(&*self.lock().map_err(repository_error)?, id)
    }

    fn approve_suggestion(&self, id: &str) -> agl_memory::Result<(MemorySuggestion, MemoryEntry)> {
        memory_approve_suggestion(&*self.lock().map_err(repository_error)?, id)
    }

    fn reject_suggestion(
        &self,
        id: &str,
        resolution_note: Option<&str>,
    ) -> agl_memory::Result<MemorySuggestion> {
        memory_reject_suggestion(
            &*self.lock().map_err(repository_error)?,
            id,
            resolution_note,
        )
    }
}

fn memory_add(store: &AglStore, draft: MemoryDraft) -> agl_memory::Result<MemoryEntry> {
    let tx = store
        .connection()
        .unchecked_transaction()
        .map_err(repository_error)?;
    let entry = insert_memory_on_connection(&tx, draft)?;
    tx.commit().map_err(repository_error)?;
    Ok(entry)
}

pub(crate) fn insert_memory_on_connection(
    conn: &Connection,
    draft: MemoryDraft,
) -> agl_memory::Result<MemoryEntry> {
    validate_memory_draft(&draft)?;
    let now = timestamp();
    let entry = MemoryEntry {
        id: memory_id(),
        scope: draft.scope,
        kind: draft.kind,
        title: draft.title,
        body: draft.body,
        source_ref: draft.source_ref,
        confidence: draft.confidence,
        created_at: now.clone(),
        updated_at: now,
        deleted_at: None,
    };
    conn.execute(
        "INSERT INTO memory_entries
         (id, scope_kind, scope_key, kind, title, body, source_ref, confidence,
          created_at, updated_at, deleted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, NULL)",
        params![
            entry.id,
            entry.scope.kind.as_str(),
            entry.scope.key,
            entry.kind.as_str(),
            entry.title,
            entry.body,
            entry.source_ref,
            entry.confidence,
            entry.created_at,
            entry.updated_at,
        ],
    )
    .map_err(repository_error)?;
    conn.execute(
        "INSERT INTO memory_entries_fts(id, title, body) VALUES (?1, ?2, ?3)",
        params![entry.id, entry.title, entry.body],
    )
    .map_err(repository_error)?;
    Ok(entry)
}

fn memory_list(
    store: &AglStore,
    query: &MemorySearchQuery,
) -> agl_memory::Result<Vec<MemoryEntry>> {
    if query
        .text
        .as_ref()
        .is_some_and(|text| !text.trim().is_empty())
    {
        return memory_search(store, query);
    }
    let entries = all_memory_entries(store)?;
    Ok(filter_memory(entries, query, None))
}

fn memory_search(
    store: &AglStore,
    query: &MemorySearchQuery,
) -> agl_memory::Result<Vec<MemoryEntry>> {
    let Some(text) = query.text.as_ref().filter(|text| !text.trim().is_empty()) else {
        return memory_list(store, query);
    };
    let entries = all_memory_entries(store)?;
    Ok(filter_memory(entries, query, Some(&text.to_lowercase())))
}

fn all_memory_entries(store: &AglStore) -> agl_memory::Result<Vec<MemoryEntry>> {
    let mut statement = store
        .connection()
        .prepare(
            "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence,
                    created_at, updated_at, deleted_at
             FROM memory_entries
             ORDER BY updated_at DESC, id DESC",
        )
        .map_err(repository_error)?;
    let rows = statement
        .query_map([], memory_entry_from_row)
        .map_err(repository_error)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(repository_error)
}

fn filter_memory(
    entries: Vec<MemoryEntry>,
    query: &MemorySearchQuery,
    text: Option<&str>,
) -> Vec<MemoryEntry> {
    entries
        .into_iter()
        .filter(|entry| query.include_deleted || entry.deleted_at.is_none())
        .filter(|entry| {
            query
                .scope
                .as_ref()
                .is_none_or(|scope| entry.scope.kind == scope.kind && entry.scope.key == scope.key)
        })
        .filter(|entry| {
            text.is_none_or(|text| {
                entry.title.to_lowercase().contains(text)
                    || entry.body.to_lowercase().contains(text)
            })
        })
        .take(query.limit.max(1))
        .collect()
}

fn memory_get(store: &AglStore, id: &str) -> agl_memory::Result<Option<MemoryEntry>> {
    validate_non_blank("id", id)?;
    memory_get_on_connection(store.connection(), id)
}

pub(crate) fn memory_get_on_connection(
    conn: &Connection,
    id: &str,
) -> agl_memory::Result<Option<MemoryEntry>> {
    conn.query_row(
        "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence,
                created_at, updated_at, deleted_at
         FROM memory_entries WHERE id = ?1",
        params![id],
        memory_entry_from_row,
    )
    .optional()
    .map_err(repository_error)
}

fn memory_delete(store: &AglStore, id: &str) -> agl_memory::Result<MemoryEntry> {
    validate_non_blank("id", id)?;
    let now = timestamp();
    let tx = store
        .connection()
        .unchecked_transaction()
        .map_err(repository_error)?;
    tx.execute(
        "UPDATE memory_entries
         SET deleted_at = COALESCE(deleted_at, ?2), updated_at = ?2
         WHERE id = ?1",
        params![id, now],
    )
    .map_err(repository_error)?;
    tx.execute("DELETE FROM memory_entries_fts WHERE id = ?1", params![id])
        .map_err(repository_error)?;
    let entry = memory_get_on_connection(&tx, id)?
        .ok_or_else(|| MemoryError::NotFound { id: id.to_owned() })?;
    tx.commit().map_err(repository_error)?;
    Ok(entry)
}

fn memory_suggest(
    store: &AglStore,
    draft: MemorySuggestionDraft,
) -> agl_memory::Result<MemorySuggestion> {
    validate_memory_suggestion_draft(&draft)?;
    let id = suggestion_id();
    let now = timestamp();
    store
        .connection()
        .execute(
            "INSERT INTO memory_suggestions
             (id, scope_kind, scope_key, kind, title, body, source_ref, confidence, status,
              created_at, updated_at, resolved_at, resolution_ref, resolution_note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9, ?9, NULL, NULL, NULL)",
            params![
                id,
                draft.scope.kind.as_str(),
                draft.scope.key,
                draft.kind.as_str(),
                draft.title,
                draft.body,
                draft.source_ref,
                draft.confidence,
                now,
            ],
        )
        .map_err(repository_error)?;
    memory_get_suggestion(store, &id)?.ok_or(MemoryError::NotFound { id })
}

fn memory_list_suggestions(
    store: &AglStore,
    query: &MemorySuggestionQuery,
) -> agl_memory::Result<Vec<MemorySuggestion>> {
    let mut statement = store
        .connection()
        .prepare(
            "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence,
                    status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
             FROM memory_suggestions ORDER BY updated_at DESC, id DESC",
        )
        .map_err(repository_error)?;
    let rows = statement
        .query_map([], memory_suggestion_from_row)
        .map_err(repository_error)?;
    let suggestions = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(repository_error)?;
    Ok(suggestions
        .into_iter()
        .filter(|suggestion| {
            query.scope.as_ref().is_none_or(|scope| {
                suggestion.scope.kind == scope.kind && suggestion.scope.key == scope.key
            })
        })
        .filter(|suggestion| {
            query
                .status
                .is_none_or(|status| suggestion.status == status)
        })
        .take(query.limit.max(1))
        .collect())
}

fn memory_get_suggestion(
    store: &AglStore,
    id: &str,
) -> agl_memory::Result<Option<MemorySuggestion>> {
    validate_non_blank("id", id)?;
    memory_get_suggestion_on_connection(store.connection(), id)
}

fn memory_get_suggestion_on_connection(
    conn: &Connection,
    id: &str,
) -> agl_memory::Result<Option<MemorySuggestion>> {
    conn.query_row(
        "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence,
                status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
         FROM memory_suggestions WHERE id = ?1",
        params![id],
        memory_suggestion_from_row,
    )
    .optional()
    .map_err(repository_error)
}

fn memory_approve_suggestion(
    store: &AglStore,
    id: &str,
) -> agl_memory::Result<(MemorySuggestion, MemoryEntry)> {
    validate_non_blank("id", id)?;
    let tx = store
        .connection()
        .unchecked_transaction()
        .map_err(repository_error)?;
    let suggestion = memory_get_suggestion_on_connection(&tx, id)?
        .ok_or_else(|| MemoryError::NotFound { id: id.to_owned() })?;
    if suggestion.status != MemorySuggestionStatus::Pending {
        return Err(MemoryError::InvalidValue {
            field: "suggestion_status",
            value: suggestion.status.as_str().to_owned(),
            reason: "only pending suggestions can be approved",
        });
    }
    let mut draft = MemoryDraft::new(
        suggestion.scope.clone(),
        suggestion.kind,
        suggestion.title.clone(),
        suggestion.body.clone(),
    );
    draft.source_ref = Some(suggestion.source_ref.clone());
    draft.confidence = suggestion.confidence;
    let entry = insert_memory_on_connection(&tx, draft)?;
    let now = timestamp();
    let changed = tx
        .execute(
            "UPDATE memory_suggestions
             SET status = 'approved', updated_at = ?2, resolved_at = ?2,
                 resolution_ref = ?3, resolution_note = NULL
             WHERE id = ?1 AND status = 'pending'",
            params![id, now, format!("memory:{}", entry.id)],
        )
        .map_err(repository_error)?;
    if changed != 1 {
        return Err(MemoryError::Repository {
            reason: "memory suggestion changed concurrently".to_owned(),
        });
    }
    let updated = memory_get_suggestion_on_connection(&tx, id)?
        .ok_or_else(|| MemoryError::NotFound { id: id.to_owned() })?;
    tx.commit().map_err(repository_error)?;
    Ok((updated, entry))
}

fn memory_reject_suggestion(
    store: &AglStore,
    id: &str,
    resolution_note: Option<&str>,
) -> agl_memory::Result<MemorySuggestion> {
    validate_non_blank("id", id)?;
    if let Some(note) = resolution_note {
        validate_non_blank("resolution_note", note)?;
    }
    let suggestion = memory_get_suggestion(store, id)?
        .ok_or_else(|| MemoryError::NotFound { id: id.to_owned() })?;
    if suggestion.status != MemorySuggestionStatus::Pending {
        return Err(MemoryError::InvalidValue {
            field: "suggestion_status",
            value: suggestion.status.as_str().to_owned(),
            reason: "only pending suggestions can be rejected",
        });
    }
    let now = timestamp();
    let changed = store
        .connection()
        .execute(
            "UPDATE memory_suggestions
             SET status = 'rejected', updated_at = ?2, resolved_at = ?2,
                 resolution_ref = NULL, resolution_note = ?3
             WHERE id = ?1 AND status = 'pending'",
            params![id, now, resolution_note],
        )
        .map_err(repository_error)?;
    if changed != 1 {
        return Err(MemoryError::Repository {
            reason: "memory suggestion changed concurrently".to_owned(),
        });
    }
    memory_get_suggestion(store, id)?.ok_or_else(|| MemoryError::NotFound { id: id.to_owned() })
}

fn memory_entry_from_row(row: &Row<'_>) -> rusqlite::Result<MemoryEntry> {
    let scope_kind = parse_row(MemoryScopeKind::parse(&row.get::<_, String>(1)?), 1)?;
    let kind = parse_row(MemoryKind::parse(&row.get::<_, String>(3)?), 3)?;
    let raw_confidence = row.get::<_, i64>(7)?;
    let confidence = u8::try_from(raw_confidence)
        .ok()
        .filter(|value| *value <= 100)
        .ok_or_else(|| invalid_row(7, "memory confidence is outside 0..=100"))?;
    Ok(MemoryEntry {
        id: row.get(0)?,
        scope: MemoryScope {
            kind: scope_kind,
            key: row.get(2)?,
        },
        kind,
        title: row.get(4)?,
        body: row.get(5)?,
        source_ref: row.get(6)?,
        confidence,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
        deleted_at: row.get(10)?,
    })
}

fn memory_suggestion_from_row(row: &Row<'_>) -> rusqlite::Result<MemorySuggestion> {
    let scope_kind = parse_row(MemoryScopeKind::parse(&row.get::<_, String>(1)?), 1)?;
    let kind = parse_row(MemoryKind::parse(&row.get::<_, String>(3)?), 3)?;
    let raw_confidence = row.get::<_, i64>(7)?;
    let confidence = u8::try_from(raw_confidence)
        .ok()
        .filter(|value| *value <= 100)
        .ok_or_else(|| invalid_row(7, "memory confidence is outside 0..=100"))?;
    let status = parse_row(MemorySuggestionStatus::parse(&row.get::<_, String>(8)?), 8)?;
    Ok(MemorySuggestion {
        id: row.get(0)?,
        scope: MemoryScope {
            kind: scope_kind,
            key: row.get(2)?,
        },
        kind,
        title: row.get(4)?,
        body: row.get(5)?,
        source_ref: row.get(6)?,
        confidence,
        status,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
        resolved_at: row.get(11)?,
        resolution_ref: row.get(12)?,
        resolution_note: row.get(13)?,
    })
}

fn parse_row<T>(result: agl_memory::Result<T>, column: usize) -> rusqlite::Result<T> {
    result.map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn invalid_row(column: usize, reason: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Integer,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            reason.to_owned(),
        )),
    )
}

fn repository_error(error: impl std::fmt::Display) -> MemoryError {
    MemoryError::Repository {
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

fn memory_id() -> String {
    unique_id("mem")
}

fn suggestion_id() -> String {
    unique_id("mem_suggestion")
}

fn unique_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{prefix}_{}_{}", std::process::id(), nanos)
}
