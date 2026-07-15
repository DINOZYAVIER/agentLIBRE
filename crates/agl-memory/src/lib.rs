use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use agl_store::{AglStore, StoreError};
use rusqlite::{OptionalExtension, params};
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
    Store(StoreError),
    Sqlite(rusqlite::Error),
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
            Self::Store(err) => write!(f, "{err}"),
            Self::Sqlite(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for MemoryError {}

impl From<StoreError> for MemoryError {
    fn from(err: StoreError) -> Self {
        Self::Store(err)
    }
}

impl From<rusqlite::Error> for MemoryError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sqlite(err)
    }
}

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

    fn parse(value: &str) -> Result<Self> {
        match value {
            "user" => Ok(Self::User),
            "repo" => Ok(Self::Repo),
            "matrix_room" => Ok(Self::MatrixRoom),
            "matrix_user" => Ok(Self::MatrixUser),
            _ => Err(MemoryError::InvalidValue {
                field: "scope_kind",
                value: value.to_string(),
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
            key: DEFAULT_USER_SCOPE_KEY.to_string(),
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

    fn parse(value: &str) -> Result<Self> {
        match value {
            "fact" => Ok(Self::Fact),
            "preference" => Ok(Self::Preference),
            "summary" => Ok(Self::Summary),
            "decision" => Ok(Self::Decision),
            "working_note" => Ok(Self::WorkingNote),
            _ => Err(MemoryError::InvalidValue {
                field: "kind",
                value: value.to_string(),
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

    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "rejected" => Ok(Self::Rejected),
            _ => Err(MemoryError::InvalidValue {
                field: "suggestion_status",
                value: value.to_string(),
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

pub struct MemoryRepository<'a> {
    store: &'a AglStore,
}

impl<'a> MemoryRepository<'a> {
    pub fn new(store: &'a AglStore) -> Self {
        Self { store }
    }

    pub fn add(&self, draft: MemoryDraft) -> Result<MemoryEntry> {
        let tx = self.store.connection().unchecked_transaction()?;
        let entry = Self::insert_on_connection(&tx, draft)?;
        tx.commit()?;
        Ok(entry)
    }

    pub fn insert_on_connection(
        conn: &rusqlite::Connection,
        draft: MemoryDraft,
    ) -> Result<MemoryEntry> {
        validate_draft(&draft)?;
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
             (id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at)
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
                entry.updated_at
            ],
        )?;
        index_entry(conn, &entry)?;
        Ok(entry)
    }

    pub fn list(&self, query: &MemorySearchQuery) -> Result<Vec<MemoryEntry>> {
        if query
            .text
            .as_ref()
            .is_some_and(|text| !text.trim().is_empty())
        {
            return self.search(query);
        }

        let limit = limit_i64(query.limit);
        match &query.scope {
            Some(scope) if query.include_deleted => self.query_entries(
                "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at
                 FROM memory_entries
                 WHERE scope_kind = ?1 AND scope_key = ?2
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?3",
                params![scope.kind.as_str(), scope.key, limit],
            ),
            Some(scope) => self.query_entries(
                "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at
                 FROM memory_entries
                 WHERE scope_kind = ?1 AND scope_key = ?2 AND deleted_at IS NULL
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?3",
                params![scope.kind.as_str(), scope.key, limit],
            ),
            None if query.include_deleted => self.query_entries(
                "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at
                 FROM memory_entries
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?1",
                params![limit],
            ),
            None => self.query_entries(
                "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at
                 FROM memory_entries
                 WHERE deleted_at IS NULL
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?1",
                params![limit],
            ),
        }
    }

    pub fn search(&self, query: &MemorySearchQuery) -> Result<Vec<MemoryEntry>> {
        let Some(text) = query.text.as_ref().filter(|text| !text.trim().is_empty()) else {
            return self.list(query);
        };
        match self.search_fts(query, text) {
            Ok(entries) => Ok(entries),
            Err(MemoryError::Sqlite(err)) if should_fallback_to_like(&err) => {
                self.search_like(query, text)
            }
            Err(err) => Err(err),
        }
    }

    pub fn get(&self, id: &str) -> Result<Option<MemoryEntry>> {
        validate_non_blank("id", id)?;
        self.store
            .connection()
            .query_row(
                "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at
                 FROM memory_entries
                 WHERE id = ?1",
                params![id],
                memory_entry_from_row,
            )
            .optional()?
            .transpose()
    }

    pub fn delete(&self, id: &str) -> Result<MemoryEntry> {
        validate_non_blank("id", id)?;
        let now = timestamp();
        let tx = self.store.connection().unchecked_transaction()?;
        tx.execute(
            "UPDATE memory_entries
             SET deleted_at = COALESCE(deleted_at, ?2), updated_at = ?2
             WHERE id = ?1",
            params![id, now],
        )?;
        tx.execute("DELETE FROM memory_entries_fts WHERE id = ?1", params![id])?;
        tx.commit()?;
        self.get(id)?
            .ok_or_else(|| MemoryError::NotFound { id: id.to_string() })
    }

    pub fn suggest(&self, draft: MemorySuggestionDraft) -> Result<MemorySuggestion> {
        validate_suggestion_draft(&draft)?;
        let id = suggestion_id();
        let now = timestamp();
        self.store.connection().execute(
            "INSERT INTO memory_suggestions
             (id, scope_kind, scope_key, kind, title, body, source_ref, confidence, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, NULL, NULL, NULL)",
            params![
                id,
                draft.scope.kind.as_str(),
                draft.scope.key,
                draft.kind.as_str(),
                draft.title,
                draft.body,
                draft.source_ref,
                draft.confidence,
                MemorySuggestionStatus::Pending.as_str(),
                now
            ],
        )?;
        self.get_suggestion(&id)?
            .ok_or_else(|| MemoryError::NotFound { id })
    }

    pub fn list_suggestions(&self, query: &MemorySuggestionQuery) -> Result<Vec<MemorySuggestion>> {
        let limit = limit_i64(query.limit);
        match (&query.scope, query.status) {
            (Some(scope), Some(status)) => self.query_suggestions(
                "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
                 FROM memory_suggestions
                 WHERE scope_kind = ?1 AND scope_key = ?2 AND status = ?3
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?4",
                params![scope.kind.as_str(), scope.key, status.as_str(), limit],
            ),
            (Some(scope), None) => self.query_suggestions(
                "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
                 FROM memory_suggestions
                 WHERE scope_kind = ?1 AND scope_key = ?2
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?3",
                params![scope.kind.as_str(), scope.key, limit],
            ),
            (None, Some(status)) => self.query_suggestions(
                "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
                 FROM memory_suggestions
                 WHERE status = ?1
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?2",
                params![status.as_str(), limit],
            ),
            (None, None) => self.query_suggestions(
                "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
                 FROM memory_suggestions
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?1",
                params![limit],
            ),
        }
    }

    pub fn get_suggestion(&self, id: &str) -> Result<Option<MemorySuggestion>> {
        validate_non_blank("id", id)?;
        self.store
            .connection()
            .query_row(
                "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
                 FROM memory_suggestions
                 WHERE id = ?1",
                params![id],
                memory_suggestion_from_row,
            )
            .optional()?
            .transpose()
    }

    pub fn approve_suggestion(&self, id: &str) -> Result<(MemorySuggestion, MemoryEntry)> {
        validate_non_blank("id", id)?;
        let suggestion = self
            .get_suggestion(id)?
            .ok_or_else(|| MemoryError::NotFound { id: id.to_string() })?;
        if suggestion.status != MemorySuggestionStatus::Pending {
            return Err(MemoryError::InvalidValue {
                field: "suggestion_status",
                value: suggestion.status.as_str().to_string(),
                reason: "only pending suggestions can be approved",
            });
        }

        let tx = self.store.connection().unchecked_transaction()?;
        let mut draft = MemoryDraft::new(
            suggestion.scope.clone(),
            suggestion.kind,
            suggestion.title.clone(),
            suggestion.body.clone(),
        );
        draft.source_ref = Some(suggestion.source_ref.clone());
        draft.confidence = suggestion.confidence;
        let entry = Self::insert_on_connection(&tx, draft)?;
        let now = timestamp();
        tx.execute(
            "UPDATE memory_suggestions
             SET status = ?2, updated_at = ?3, resolved_at = ?3, resolution_ref = ?4, resolution_note = NULL
             WHERE id = ?1 AND status = ?5",
            params![
                id,
                MemorySuggestionStatus::Approved.as_str(),
                now,
                format!("memory:{}", entry.id),
                MemorySuggestionStatus::Pending.as_str()
            ],
        )?;
        tx.commit()?;
        let updated = self
            .get_suggestion(id)?
            .ok_or_else(|| MemoryError::NotFound { id: id.to_string() })?;
        Ok((updated, entry))
    }

    pub fn reject_suggestion(
        &self,
        id: &str,
        resolution_note: Option<&str>,
    ) -> Result<MemorySuggestion> {
        validate_non_blank("id", id)?;
        if let Some(note) = resolution_note {
            validate_non_blank("resolution_note", note)?;
        }
        let suggestion = self
            .get_suggestion(id)?
            .ok_or_else(|| MemoryError::NotFound { id: id.to_string() })?;
        if suggestion.status != MemorySuggestionStatus::Pending {
            return Err(MemoryError::InvalidValue {
                field: "suggestion_status",
                value: suggestion.status.as_str().to_string(),
                reason: "only pending suggestions can be rejected",
            });
        }
        let now = timestamp();
        self.store.connection().execute(
            "UPDATE memory_suggestions
             SET status = ?2, updated_at = ?3, resolved_at = ?3, resolution_ref = NULL, resolution_note = ?4
             WHERE id = ?1 AND status = ?5",
            params![
                id,
                MemorySuggestionStatus::Rejected.as_str(),
                now,
                resolution_note,
                MemorySuggestionStatus::Pending.as_str()
            ],
        )?;
        self.get_suggestion(id)?
            .ok_or_else(|| MemoryError::NotFound { id: id.to_string() })
    }

    fn search_fts(&self, query: &MemorySearchQuery, text: &str) -> Result<Vec<MemoryEntry>> {
        let limit = limit_i64(query.limit);
        match &query.scope {
            Some(scope) if query.include_deleted => self.query_entries(
                "SELECT e.id, e.scope_kind, e.scope_key, e.kind, e.title, e.body, e.source_ref, e.confidence, e.created_at, e.updated_at, e.deleted_at
                 FROM memory_entries e
                 JOIN memory_entries_fts f ON f.id = e.id
                 WHERE memory_entries_fts MATCH ?1 AND e.scope_kind = ?2 AND e.scope_key = ?3
                 ORDER BY rank
                 LIMIT ?4",
                params![text, scope.kind.as_str(), scope.key, limit],
            ),
            Some(scope) => self.query_entries(
                "SELECT e.id, e.scope_kind, e.scope_key, e.kind, e.title, e.body, e.source_ref, e.confidence, e.created_at, e.updated_at, e.deleted_at
                 FROM memory_entries e
                 JOIN memory_entries_fts f ON f.id = e.id
                 WHERE memory_entries_fts MATCH ?1 AND e.scope_kind = ?2 AND e.scope_key = ?3 AND e.deleted_at IS NULL
                 ORDER BY rank
                 LIMIT ?4",
                params![text, scope.kind.as_str(), scope.key, limit],
            ),
            None if query.include_deleted => self.query_entries(
                "SELECT e.id, e.scope_kind, e.scope_key, e.kind, e.title, e.body, e.source_ref, e.confidence, e.created_at, e.updated_at, e.deleted_at
                 FROM memory_entries e
                 JOIN memory_entries_fts f ON f.id = e.id
                 WHERE memory_entries_fts MATCH ?1
                 ORDER BY rank
                 LIMIT ?2",
                params![text, limit],
            ),
            None => self.query_entries(
                "SELECT e.id, e.scope_kind, e.scope_key, e.kind, e.title, e.body, e.source_ref, e.confidence, e.created_at, e.updated_at, e.deleted_at
                 FROM memory_entries e
                 JOIN memory_entries_fts f ON f.id = e.id
                 WHERE memory_entries_fts MATCH ?1 AND e.deleted_at IS NULL
                 ORDER BY rank
                 LIMIT ?2",
                params![text, limit],
            ),
        }
    }

    fn search_like(&self, query: &MemorySearchQuery, text: &str) -> Result<Vec<MemoryEntry>> {
        let escaped = format!("%{}%", escape_like(text));
        let limit = limit_i64(query.limit);
        match &query.scope {
            Some(scope) if query.include_deleted => self.query_entries(
                "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at
                 FROM memory_entries
                 WHERE scope_kind = ?1 AND scope_key = ?2 AND (title LIKE ?3 ESCAPE '\\' OR body LIKE ?3 ESCAPE '\\')
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?4",
                params![scope.kind.as_str(), scope.key, escaped, limit],
            ),
            Some(scope) => self.query_entries(
                "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at
                 FROM memory_entries
                 WHERE scope_kind = ?1 AND scope_key = ?2 AND deleted_at IS NULL AND (title LIKE ?3 ESCAPE '\\' OR body LIKE ?3 ESCAPE '\\')
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?4",
                params![scope.kind.as_str(), scope.key, escaped, limit],
            ),
            None if query.include_deleted => self.query_entries(
                "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at
                 FROM memory_entries
                 WHERE title LIKE ?1 ESCAPE '\\' OR body LIKE ?1 ESCAPE '\\'
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?2",
                params![escaped, limit],
            ),
            None => self.query_entries(
                "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at
                 FROM memory_entries
                 WHERE deleted_at IS NULL AND (title LIKE ?1 ESCAPE '\\' OR body LIKE ?1 ESCAPE '\\')
                 ORDER BY updated_at DESC, id DESC
                 LIMIT ?2",
                params![escaped, limit],
            ),
        }
    }

    fn query_entries<P>(&self, sql: &str, params: P) -> Result<Vec<MemoryEntry>>
    where
        P: rusqlite::Params,
    {
        let mut stmt = self.store.connection().prepare(sql)?;
        let rows = stmt.query_map(params, memory_entry_from_row)?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row??);
        }
        Ok(entries)
    }

    fn query_suggestions<P>(&self, sql: &str, params: P) -> Result<Vec<MemorySuggestion>>
    where
        P: rusqlite::Params,
    {
        let mut stmt = self.store.connection().prepare(sql)?;
        let rows = stmt.query_map(params, memory_suggestion_from_row)?;
        let mut suggestions = Vec::new();
        for row in rows {
            suggestions.push(row??);
        }
        Ok(suggestions)
    }
}

fn memory_entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<MemoryEntry>> {
    let scope_kind: String = row.get(1)?;
    let kind: String = row.get(3)?;
    let confidence: i64 = row.get(7)?;
    Ok((|| {
        let confidence = u8::try_from(confidence).map_err(|_| MemoryError::InvalidValue {
            field: "confidence",
            value: confidence.to_string(),
            reason: "confidence must be between 0 and 100",
        })?;
        Ok(MemoryEntry {
            id: row.get(0)?,
            scope: MemoryScope {
                kind: MemoryScopeKind::parse(&scope_kind)?,
                key: row.get(2)?,
            },
            kind: MemoryKind::parse(&kind)?,
            title: row.get(4)?,
            body: row.get(5)?,
            source_ref: row.get(6)?,
            confidence,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
            deleted_at: row.get(10)?,
        })
    })())
}

fn memory_suggestion_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<MemorySuggestion>> {
    let scope_kind: String = row.get(1)?;
    let kind: String = row.get(3)?;
    let confidence: i64 = row.get(7)?;
    let status: String = row.get(8)?;
    Ok((|| {
        let confidence = u8::try_from(confidence).map_err(|_| MemoryError::InvalidValue {
            field: "confidence",
            value: confidence.to_string(),
            reason: "confidence must be between 0 and 100",
        })?;
        Ok(MemorySuggestion {
            id: row.get(0)?,
            scope: MemoryScope {
                kind: MemoryScopeKind::parse(&scope_kind)?,
                key: row.get(2)?,
            },
            kind: MemoryKind::parse(&kind)?,
            title: row.get(4)?,
            body: row.get(5)?,
            source_ref: row.get(6)?,
            confidence,
            status: MemorySuggestionStatus::parse(&status)?,
            created_at: row.get(9)?,
            updated_at: row.get(10)?,
            resolved_at: row.get(11)?,
            resolution_ref: row.get(12)?,
            resolution_note: row.get(13)?,
        })
    })())
}

fn index_entry(conn: &rusqlite::Connection, entry: &MemoryEntry) -> Result<()> {
    conn.execute(
        "INSERT INTO memory_entries_fts(id, title, body) VALUES (?1, ?2, ?3)",
        params![entry.id, entry.title, entry.body],
    )?;
    Ok(())
}

fn validate_draft(draft: &MemoryDraft) -> Result<()> {
    validate_non_blank("scope_key", &draft.scope.key)?;
    validate_non_blank("title", &draft.title)?;
    validate_non_blank("body", &draft.body)?;
    if draft.confidence > 100 {
        return Err(MemoryError::InvalidValue {
            field: "confidence",
            value: draft.confidence.to_string(),
            reason: "confidence must be between 0 and 100",
        });
    }
    Ok(())
}

fn validate_suggestion_draft(draft: &MemorySuggestionDraft) -> Result<()> {
    validate_non_blank("scope_key", &draft.scope.key)?;
    validate_non_blank("title", &draft.title)?;
    validate_non_blank("body", &draft.body)?;
    validate_non_blank("source_ref", &draft.source_ref)?;
    if draft.confidence > 100 {
        return Err(MemoryError::InvalidValue {
            field: "confidence",
            value: draft.confidence.to_string(),
            reason: "confidence must be between 0 and 100",
        });
    }
    Ok(())
}

fn validate_non_blank(field: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(MemoryError::InvalidValue {
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

fn should_fallback_to_like(err: &rusqlite::Error) -> bool {
    let rusqlite::Error::SqliteFailure(_, Some(message)) = err else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    message.contains("fts5:")
        || message.contains("malformed match expression")
        || (message.contains("match") && message.contains("syntax error"))
}

fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("unix:{seconds}")
}

fn memory_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("mem_{}_{}", std::process::id(), nanos)
}

fn suggestion_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("mem_suggestion_{}_{}", std::process::id(), nanos)
}

#[cfg(test)]
mod tests;
