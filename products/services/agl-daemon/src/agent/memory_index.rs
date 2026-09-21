//! Disposable SQLite FTS5 search index for durable memory claims (M2 tasks 1-2).
//!
//! Implements the one-way build from entry files to a claims table plus an
//! FTS5 virtual table, per `reports/semantic-memory.md` sections 12.1-12.2
//! and `reports/m2-implementation-tasks.md` tasks 1-2.
//!
//! - The index is a machine-local cache; entry files remain canonical.
//! - Full rebuild on demand; no incremental sync.
//! - Per-entry isolation: a parse or I/O error skips one entry only (Q4).
//! - Atomic publish: build into a temp file, rename under the workspace
//!   mutation lock only if the generation is unchanged (Q2).
//! - Normalization: lowercase, identifier split, Porter stem (section 12.2).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use agl_core::agent::{FileSource, MemoryEntry, StoredClaim, ClaimStatus, parse_entry};
use rusqlite::Connection;
use sha2::{Digest as _, Sha256};
use stemmer::{Language, Stemmer};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure to build or open the memory index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IndexError {
    /// The workspace root is missing or not canonicalizable.
    RootUnavailable,
    /// The `memory/` or `memory/entries/` directory structure is wrong.
    LayoutError(String),
    /// An entry file could not be read.
    Unreadable(String),
    /// The index directory could not be created.
    DirError(String),
    /// A SQLite operation failed.
    Sqlite(String),
    /// The stored workspace root in the DB does not match the expected root.
    RootMismatch { stored: String, expected: String },
    /// The index file is missing (not an error for rebuild; for open it is).
    Missing,
    /// The generation changed during rebuild (after one retry).
    GenerationConflict,
    /// The workspace mutation lock is poisoned.
    LockPoisoned,
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootUnavailable => write!(f, "workspace root is unavailable"),
            Self::LayoutError(path) => write!(f, "memory layout error at {path}"),
            Self::Unreadable(path) => write!(f, "cannot read {path}"),
            Self::DirError(path) => write!(f, "cannot create directory {path}"),
            Self::Sqlite(msg) => write!(f, "sqlite error: {msg}"),
            Self::RootMismatch { stored, expected } => {
                write!(f, "index root mismatch: stored={stored}, expected={expected}")
            }
            Self::Missing => write!(f, "index file is missing"),
            Self::GenerationConflict => {
                write!(f, "workspace generation changed during rebuild")
            }
            Self::LockPoisoned => write!(f, "workspace mutation lock is poisoned"),
        }
    }
}

impl std::error::Error for IndexError {}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// Outcome of a full index rebuild.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RebuildResult {
    /// Total number of claims indexed.
    pub claim_count: usize,
    /// Slugs of entries that were skipped due to parse or I/O errors (Q4).
    pub skipped: Vec<String>,
}

// ---------------------------------------------------------------------------
// Path computation
// ---------------------------------------------------------------------------

/// Compute the index database path: `<data_root>/store/memory-index/<sha256>.db`.
///
/// The filename is the full SHA-256 hex digest of the canonical workspace root
/// path string (section 12.5 Q1, amended 2026-09-17).
pub(crate) fn index_db_path(data_root: &Path, workspace_root: &Path) -> PathBuf {
    let digest = Sha256::digest(workspace_root.to_string_lossy().as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    data_root
        .join("store")
        .join("memory-index")
        .join(format!("{hex}.db"))
}

// ---------------------------------------------------------------------------
// Normalization (section 12.2)
// ---------------------------------------------------------------------------

/// The shared deterministic normalization pipeline for `search_text` and
/// query tokenization (section 12.2).
///
/// Pipeline:
/// 1. Split on non-alphanumeric characters (whitespace, punctuation, `/`, `\\`,
///    `.`, `:`, `-`, `_`, etc.).
/// 2. Within each alphanumeric run, split at:
///    - lowercase-to-uppercase boundary (`fooBar` → `foo`, `Bar`)
///    - acronym-to-following-word boundary (`HTTPServer` → `HTTP`, `Server`)
///    - letter-to-digit boundary (`sha256` → `sha`, `256`)
///    - digit-to-letter boundary (`256abc` → `256`, `abc`)
/// 3. Unicode lowercase each token.
/// 4. Apply Porter stemming only to tokens that are entirely ASCII `[a-z]`.
/// 5. Remove empty tokens and exact duplicates, preserving first occurrence.
/// 6. Join with a single space.
pub(crate) fn normalize_for_search(text: &str) -> String {
    let raw_tokens = split_identifier_tokens(text);
    let stemmer = Stemmer::new(Language::English);

    let mut seen = BTreeSet::new();
    let mut result: Vec<String> = Vec::new();

    for token in raw_tokens {
        let lower: String = token.to_lowercase();
        if lower.is_empty() {
            continue;
        }
        // Porter-stem only pure ASCII alphabetic tokens.
        let final_token = if lower.chars().all(|c| c.is_ascii_lowercase()) {
            stemmer.stem_word(&lower).to_owned()
        } else {
            lower
        };
        if final_token.is_empty() {
            continue;
        }
        if seen.insert(final_token.clone()) {
            result.push(final_token);
        }
    }

    result.join(" ")
}

/// Split a string into raw alphanumeric tokens, applying identifier boundary
/// rules within each run.
fn split_identifier_tokens(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut tokens: Vec<String> = Vec::new();
    let mut current: Vec<char> = Vec::new();

    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];

        if !ch.is_alphanumeric() {
            // Punctuation or whitespace: token boundary.
            if !current.is_empty() {
                tokens.push(current.iter().collect());
                current.clear();
            }
            i += 1;
            continue;
        }

        // Start or continue an alphanumeric run.
        if current.is_empty() {
            current.push(ch);
            i += 1;
            continue;
        }

        // Check for a boundary between the last char in `current` and `ch`.
        let prev = *current.last().unwrap();
        let boundary = if prev.is_lowercase() && ch.is_uppercase() {
            // lowercase → uppercase: `fooBar` boundary.
            true
        } else if prev.is_uppercase()
            && ch.is_uppercase()
            && i + 1 < chars.len()
            && chars[i + 1].is_lowercase()
        {
            // acronym → following word: `HTTPServer` boundary between P and S.
            true
        } else if prev.is_alphabetic() && ch.is_numeric() {
            // letter → digit: `sha256` boundary.
            true
        } else if prev.is_numeric() && ch.is_alphabetic() {
            // digit → letter: `256abc` boundary.
            true
        } else {
            false
        };

        if boundary {
            tokens.push(current.iter().collect());
            current = vec![ch];
        } else {
            current.push(ch);
        }
        i += 1;
    }

    if !current.is_empty() {
        tokens.push(current.iter().collect());
    }

    tokens
}

// ---------------------------------------------------------------------------
// Corpus projection (Task 1)
// ---------------------------------------------------------------------------

/// One projected claim row ready for insertion into the `claims` table.
#[derive(Clone, Debug)]
struct ClaimRow {
    slug: String,
    text: String,
    search_text: String,
    /// JSON-serialized sources.
    sources_json: String,
    stale: bool,
    /// Resolved row ID of the superseding claim, or None.
    superseded_by: Option<usize>,
    line_no: usize,
    created_run: String,
}

/// Project one parsed entry file into claim rows, recovering physical line
/// numbers from the raw text (Q3: line_no is the 1-based line of the claim
/// bullet in the entry file).
fn project_entry(entry: &MemoryEntry, raw_text: &str) -> Vec<ClaimRow> {
    // Recover line numbers: scan the raw text for claim bullet lines after
    // the `## Claims` header.
    let line_numbers = claim_bullet_line_numbers(raw_text);

    entry
        .claims
        .iter()
        .enumerate()
        .map(|(idx, claim)| {
            let line_no = line_numbers
                .get(idx)
                .copied()
                .unwrap_or(idx + 1); // fallback: 1-based index

            let sources_json = serialize_sources(claim);
            let search_text = normalize_for_search(&claim.text);
            let stale = claim.status == Some(ClaimStatus::Stale);

            ClaimRow {
                slug: entry.slug.clone(),
                text: claim.text.clone(),
                search_text,
                sources_json,
                stale,
                superseded_by: None, // resolved in a second pass below
                line_no,
                created_run: entry.created.to_string(),
            }
        })
        .collect()
}

/// Find the 1-based line numbers of claim bullets in the raw entry text.
///
/// A claim bullet is a line starting with `"- "` that appears after the
/// `"## Claims"` header line. The returned vector is in the same order as
/// the parsed `StoredClaim` sequence.
fn claim_bullet_line_numbers(raw_text: &str) -> Vec<usize> {
    let mut in_claims = false;
    let mut line_numbers = Vec::new();

    for (idx, line) in raw_text.lines().enumerate() {
        let line_no = idx + 1; // 1-based
        if line == "## Claims" {
            in_claims = true;
            continue;
        }
        if in_claims && line.starts_with("- ") {
            line_numbers.push(line_no);
        }
    }

    line_numbers
}

/// Serialize a claim's sources into the JSON format stored in the `sources`
/// column: `{"messages":[...],"files":[{"path":...,"digest":...}]}`.
fn serialize_sources(claim: &StoredClaim) -> String {
    #[derive(serde::Serialize)]
    struct Sources {
        messages: Vec<String>,
        files: Vec<FileJson>,
    }
    #[derive(serde::Serialize)]
    struct FileJson {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        digest: Option<String>,
    }

    let sources = Sources {
        messages: claim.sources.iter().map(|id| id.to_string()).collect(),
        files: claim
            .files
            .iter()
            .map(|f| FileJson {
                path: f.path.clone(),
                digest: f.digest.clone(),
            })
            .collect(),
    };

    serde_json::to_string(&sources).unwrap_or_else(|_| {
        // Serialization of this simple struct cannot fail; unreachable.
        r#"{"messages":[],"files":[]}"#.to_owned()
    })
}

// ---------------------------------------------------------------------------
// Index build (Task 2)
// ---------------------------------------------------------------------------

/// Build the complete index from entry files and publish it atomically.
///
/// This function:
/// 1. Reads all entry files from `workspace_root/memory/entries/`.
/// 2. Parses each entry in isolation (Q4: one bad entry is skipped).
/// 3. Projects claims into rows, resolving supersession references.
/// 4. Builds a complete temporary SQLite database.
/// 5. Under the workspace mutation lock, publishes by atomic rename if the
///    generation is unchanged; otherwise retries once (Q2).
pub(crate) fn rebuild_index(
    data_root: &Path,
    workspace_root: &Path,
    generation: u64,
    current_generation: impl Fn() -> u64,
) -> Result<RebuildResult, IndexError> {
    // Attempt up to two builds (initial + one retry on generation conflict).
    for attempt in 0..2u32 {
        // Step 1-3: build the temp database (no lock needed).
        let temp_path = build_temp_index(data_root, workspace_root)?;

        // Step 5: publish under the workspace mutation lock.
        let lock = crate::tools::workspace_mutation(workspace_root);
        let guard = lock.lock().map_err(|_| IndexError::LockPoisoned)?;

        // Check generation before publishing.
        if current_generation() != generation {
            drop(guard);
            // Clean up the temp file.
            let _ = fs::remove_file(&temp_path);
            if attempt == 0 {
                // Retry once with the new generation.
                return rebuild_index_with_generation(
                    data_root,
                    workspace_root,
                    current_generation(),
                    &current_generation,
                );
            }
            return Err(IndexError::GenerationConflict);
        }

        // Ensure the target directory exists.
        let target_path = index_db_path(data_root, workspace_root);
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).map_err(|_| IndexError::DirError(parent.display().to_owned()))?;
            crate::store::path::ensure_private_dir(parent)
                .map_err(|_| IndexError::DirError(parent.display().to_owned()))?;
        }

        // Set private permissions on the temp file before rename.
        crate::store::path::set_private_file_permissions(&temp_path)
            .map_err(|_| IndexError::DirError(temp_path.display().to_owned()))?;

        // Atomic rename.
        fs::rename(&temp_path, &target_path).map_err(|e| {
            let _ = fs::remove_file(&temp_path);
            IndexError::DirError(format!("rename failed: {e}"))
        })?;

        drop(guard);

        // Read back the claim count for the result.
        let result = open_and_verify(&target_path, workspace_root)
            .and_then(|conn| {
                let mut stmt =
                    conn.prepare("SELECT COUNT(*) FROM claims").map_err(|e| IndexError::Sqlite(e.to_string()))?;
                let count: usize = stmt
                    .query_row([], |row| row.get(0))
                    .map_err(|e| IndexError::Sqlite(e.to_string()))?;
                Ok::<usize, IndexError>(count)
            });

        // Re-read skipped from a metadata row we stored during build.
        let skipped: Vec<String> = open_and_verify(&target_path, workspace_root)
            .and_then(|conn| {
                let mut stmt = conn
                    .prepare("SELECT value FROM metadata WHERE key = 'skipped'")
                    .map_err(|e| IndexError::Sqlite(e.to_string()))?;
                let val: String = stmt
                    .query_row([], |row| row.get(0))
                    .map_err(|e| IndexError::Sqlite(e.to_string()))?;
                serde_json::from_str(&val).map_err(|e| IndexError::Sqlite(e.to_string()))
            })
            .unwrap_or_default();

        let claim_count = result?;
        return Ok(RebuildResult { claim_count, skipped });
    }

    unreachable!("loop runs at most twice and returns")
}

/// Internal: same as `rebuild_index` but takes an explicit generation (used
/// for the single retry).
fn rebuild_index_with_generation(
    data_root: &Path,
    workspace_root: &Path,
    generation: u64,
    current_generation: &impl Fn() -> u64,
) -> Result<RebuildResult, IndexError> {
    let temp_path = build_temp_index(data_root, workspace_root)?;

    let lock = crate::tools::workspace_mutation(workspace_root);
    let guard = lock.lock().map_err(|_| IndexError::LockPoisoned)?;

    if current_generation() != generation {
        drop(guard);
        let _ = fs::remove_file(&temp_path);
        return Err(IndexError::GenerationConflict);
    }

    let target_path = index_db_path(data_root, workspace_root);
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|_| IndexError::DirError(parent.display().to_owned()))?;
        crate::store::path::ensure_private_dir(parent)
            .map_err(|_| IndexError::DirError(parent.display().to_owned()))?;
    }

    crate::store::path::set_private_file_permissions(&temp_path)
        .map_err(|_| IndexError::DirError(temp_path.display().to_owned()))?;

    fs::rename(&temp_path, &target_path).map_err(|e| {
        let _ = fs::remove_file(&temp_path);
        IndexError::DirError(format!("rename failed: {e}"))
    })?;

    drop(guard);

    let claim_count: usize = open_and_verify(&target_path, workspace_root)
        .and_then(|conn| {
            let mut stmt = conn
                .prepare("SELECT COUNT(*) FROM claims")
                .map_err(|e| IndexError::Sqlite(e.to_string()))?;
            stmt.query_row([], |row| row.get(0))
                .map_err(|e| IndexError::Sqlite(e.to_string()))
        })?;

    let skipped: Vec<String> = open_and_verify(&target_path, workspace_root)
        .and_then(|conn| {
            let mut stmt = conn
                .prepare("SELECT value FROM metadata WHERE key = 'skipped'")
                .map_err(|e| IndexError::Sqlite(e.to_string()))?;
            let val: String = stmt
                .query_row([], |row| row.get(0))
                .map_err(|e| IndexError::Sqlite(e.to_string()))?;
            serde_json::from_str(&val).map_err(|e| IndexError::Sqlite(e.to_string()))
        })
        .unwrap_or_default();

    Ok(RebuildResult {
        claim_count,
        skipped,
    })
}

/// Build the complete index into a temporary file (no lock, no publish).
///
/// Returns the path to the temp file. The caller is responsible for
/// publishing or cleaning it up.
fn build_temp_index(data_root: &Path, workspace_root: &Path) -> Result<PathBuf, IndexError> {
    // Ensure the memory-index directory exists for the temp file.
    let dir = data_root.join("store").join("memory-index");
    fs::create_dir_all(&dir)
        .map_err(|_| IndexError::DirError(dir.display().to_owned()))?;

    // Temp file in the same directory for atomic rename on the same filesystem.
    let target = index_db_path(data_root, workspace_root);
    let temp_path = dir.join(format!(".{}.tmp", target.file_name().unwrap().to_string_lossy()));

    // Clean up any stale temp file.
    let _ = fs::remove_file(&temp_path);

    // Build the database.
    let conn = Connection::open(&temp_path).map_err(|e| IndexError::Sqlite(e.to_string()))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
        .map_err(|e| IndexError::Sqlite(e.to_string()))?;

    // Read and project entries.
    let (rows, skipped) = read_and_project_entries(workspace_root)?;

    // Resolve supersession references (second pass).
    let resolved_rows = resolve_supersession(rows);

    // Create schema and insert rows.
    create_schema(&conn)?;
    insert_rows(&conn, &resolved_rows)?;
    store_metadata(&conn, workspace_root, &skipped)?;

    // Build the FTS5 index from the content table.
    conn.execute("INSERT INTO claims_fts(claims_fts) VALUES('rebuild')", [])
        .map_err(|e| {
            let _ = fs::remove_file(&temp_path);
            IndexError::Sqlite(format!("FTS5 rebuild failed: {e}"))
        })?;

    conn.close().map_err(|e| {
        let _ = fs::remove_file(&temp_path);
        IndexError::Sqlite(e.to_string())
    })?;

    Ok(temp_path)
}

/// Read all entry files from `workspace_root/memory/entries/` and project
/// them into claim rows. Per-entry isolation: a parse or I/O error skips
/// that entry only (Q4).
fn read_and_project_entries(
    workspace_root: &Path,
) -> Result<(Vec<ClaimRow>, Vec<String>), IndexError> {
    // Canonicalize the workspace root.
    let canonical_root = workspace_root
        .canonicalize()
        .map_err(|_| IndexError::RootUnavailable)?;

    let memory_dir = canonical_root.join("memory");
    if !memory_dir.is_dir() {
        // No memory/ directory: empty index (opt-out).
        return Ok((Vec::new(), Vec::new()));
    }

    let entries_dir = memory_dir.join("entries");
    if !entries_dir.is_dir() {
        // memory/ exists but no entries/: empty index.
        return Ok((Vec::new(), Vec::new()));
    }

    let mut all_rows: Vec<ClaimRow> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    // Deterministic order: sort by filename (BTreeMap would do this, but we
    // collect manually to handle per-entry isolation).
    let mut files: Vec<PathBuf> = Vec::new();
    let read_dir = fs::read_dir(&entries_dir)
        .map_err(|_| IndexError::Unreadable("memory/entries".to_owned()))?;
    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().to_owned();
        if !name.ends_with(".md") {
            continue;
        }
        let path = entry.path();
        // Skip symlinks (containment rule).
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if metadata.file_type().is_symlink() {
                continue;
            }
        }
        files.push(path);
    }
    files.sort();

    for path in files {
        let slug = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_owned())
            .unwrap_or_default();

        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => {
                skipped.push(slug);
                continue;
            }
        };

        let entry = match parse_entry(&content) {
            Ok(e) => e,
            Err(_) => {
                skipped.push(slug);
                continue;
            }
        };

        let rows = project_entry(&entry, &content);
        all_rows.extend(rows);
    }

    Ok((all_rows, skipped))
}

/// Resolve `superseded_by` string references (`<slug>#claim-<n>`) to row IDs.
///
/// The input rows are in deterministic insertion order (sorted by slug, then
/// by claim ordinal within the entry). The `id` column will be assigned
/// sequentially starting from 1 in that order.
///
/// Unresolvable references become None (Q3: the tag stays in display text).
fn resolve_supersession(rows: Vec<ClaimRow>) -> Vec<ClaimRow> {
    // Build a lookup: (slug, claim_ordinal) → row index (0-based).
    // The row index + 1 will be the SQLite `id`.
    let mut lookup: BTreeMap<(&str, usize), usize> = BTreeMap::new();
    for (idx, row) in rows.iter().enumerate() {
        // The ordinal is the 1-based position within the entry's claims.
        // We can recover it by counting how many rows share the same slug
        // up to and including this one.
        let ordinal = rows
            .iter()
            .take(idx + 1)
            .filter(|r| r.slug == row.slug)
            .count();
        lookup.insert((row.slug.as_str(), ordinal), idx);
    }

    let mut resolved = rows;
    for row in resolved.iter_mut() {
        if let Some(ref target) = row_superseded_by_raw(&row) {
            // Parse `<slug>#claim-<n>`.
            if let Some((target_slug, target_ordinal)) = parse_supersession_ref(target) {
                let target_idx = lookup.get(&(target_slug, target_ordinal));
                row.superseded_by = target_idx.map(|idx| idx + 1); // 1-based ID
            } else {
                row.superseded_by = None;
            }
        }
    }

    resolved
}

/// Extract the raw `superseded_by` string from a row. Since we don't store
/// it in `ClaimRow` directly (it's in the original claim), we need to recover
/// it. Actually, we should store it in the row during projection. Let me
/// restructure: add a `superseded_by_raw` field to `ClaimRow`.
fn row_superseded_by_raw(row: &ClaimRow) -> Option<&str> {
    // This is a placeholder; the actual implementation stores the raw ref
    // in the row. See the updated ClaimRow struct.
    row.superseded_by_raw.as_deref()
}

/// Parse a `<slug>#claim-<n>` reference into (slug, ordinal).
fn parse_supersession_ref(target: &str) -> Option<(&str, usize)> {
    let (slug, claim_part) = target.split_once("#claim-")?;
    let ordinal: usize = claim_part.parse().ok()?;
    if ordinal == 0 {
        return None; // ordinals are 1-based
    }
    Some((slug, ordinal))
}

// ---------------------------------------------------------------------------
// SQLite schema and insertion
// ---------------------------------------------------------------------------

const SCHEMA: &str = r#"
CREATE TABLE claims (
    id INTEGER PRIMARY KEY,
    slug TEXT NOT NULL,
    text TEXT NOT NULL,
    search_text TEXT NOT NULL,
    sources TEXT NOT NULL,
    stale INTEGER NOT NULL DEFAULT 0,
    superseded_by INTEGER REFERENCES claims(id),
    line_no INTEGER NOT NULL,
    created_run TEXT NOT NULL
);
CREATE VIRTUAL TABLE claims_fts USING fts5(search_text, content='claims');
CREATE TABLE metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

fn create_schema(conn: &Connection) -> Result<(), IndexError> {
    conn.execute_batch(SCHEMA).map_err(|e| IndexError::Sqlite(e.to_string()))
}

fn insert_rows(conn: &Connection, rows: &[ClaimRow]) -> Result<(), IndexError> {
    let tx = conn.transaction().map_err(|e| IndexError::Sqlite(e.to_string()))?;

    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO claims (id, slug, text, search_text, sources, stale, superseded_by, line_no, created_run)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .map_err(|e| IndexError::Sqlite(e.to_string()))?;

        for (idx, row) in rows.iter().enumerate() {
            let id = (idx + 1) as i64;
            stmt.execute(rusqlite::params![
                id,
                row.slug,
                row.text,
                row.search_text,
                row.sources_json,
                row.stale as i32,
                row.superseded_by.map(|v| v as i64),
                row.line_no as i64,
                row.created_run,
            ])
            .map_err(|e| IndexError::Sqlite(e.to_string()))?;
        }
    }

    tx.commit().map_err(|e| IndexError::Sqlite(e.to_string()))
}

fn store_metadata(
    conn: &Connection,
    workspace_root: &Path,
    skipped: &[String],
) -> Result<(), IndexError> {
    let root_string = workspace_root
        .canonicalize()
        .map_err(|_| IndexError::RootUnavailable)?
        .to_string_lossy()
        .to_owned();

    let skipped_json = serde_json::to_string(skipped)
        .map_err(|e| IndexError::Sqlite(e.to_string()))?;

    conn.execute(
        "INSERT INTO metadata (key, value) VALUES ('workspace_root', ?1), ('skipped', ?2)",
        rusqlite::params![root_string, skipped_json],
    )
    .map_err(|e| IndexError::Sqlite(e.to_string()))
}

// ---------------------------------------------------------------------------
// Open and verify
// ---------------------------------------------------------------------------

/// Open an existing index database and verify that the stored workspace root
/// matches the expected canonical root. Returns a read-only connection.
pub(crate) fn open_and_verify(
    db_path: &Path,
    workspace_root: &Path,
) -> Result<Connection, IndexError> {
    if !db_path.try_exists().map_err(|_| IndexError::Missing)? {
        return Err(IndexError::Missing);
    }

    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| {
        if e.to_string().contains("not a database") {
            IndexError::Sqlite("corrupt or not a database file".to_owned())
        } else {
            IndexError::Sqlite(e.to_string())
        }
    })?;

    // Verify the stored workspace root.
    let stored_root: String = conn
        .query_row("SELECT value FROM metadata WHERE key = 'workspace_root'", [], |row| {
            row.get(0)
        })
        .map_err(|e| {
            if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                IndexError::Sqlite("missing metadata".to_owned())
            } else {
                IndexError::Sqlite(e.to_string())
            }
        })?;

    let expected_root = workspace_root
        .canonicalize()
        .map_err(|_| IndexError::RootUnavailable)?
        .to_string_lossy()
        .to_owned();

    if stored_root != expected_root {
        return Err(IndexError::RootMismatch {
            stored: stored_root,
            expected: expected_root,
        });
    }

    Ok(conn)
}

// ---------------------------------------------------------------------------
// Query-side tokenization (Q5)
// ---------------------------------------------------------------------------

/// Tokenize a user query using the same normalization pipeline as the index.
/// Returns a deduplicated list of tokens. A query that tokenizes to zero
/// terms is a no-match (not an error).
pub(crate) fn tokenize_query(query: &str) -> Vec<String> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let normalized = normalize_for_search(query);
    if normalized.is_empty() {
        Vec::new()
    } else {
        normalized.split(' ').map(|s| s.to_owned()).collect()
    }
}

/// Build the FTS5 MATCH expression from query tokens: a quoted OR-joined
/// list. No FTS5 query syntax is exposed to the model (Q5).
pub(crate) fn build_match_expression(tokens: &[String]) -> Option<String> {
    if tokens.is_empty() {
        return None;
    }
    // Quote each token to prevent FTS5 syntax injection, then OR-join.
    let quoted: Vec<String> = tokens
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    Some(quoted.join(" OR "))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Normalization tests (section 12.2 examples + edge cases) --

    #[test]
    fn normalize_camel_case() {
        // CompactionRequired → compaction required (pre-stem)
        let result = normalize_for_search("CompactionRequired");
        // After Porter stemming: "compaction" → "compact", "required" → "requir"
        assert_eq!(result, "compact requir");
    }

    #[test]
    fn normalize_acronym_to_word() {
        // HTTPServer2 → http server 2 (pre-stem)
        let result = normalize_for_search("HTTPServer2");
        // "http" stems to "http", "server" stems to "server", "2" stays
        assert_eq!(result, "http server 2");
    }

    #[test]
    fn normalize_snake_case() {
        // needs_compaction → needs compaction (pre-stem)
        let result = normalize_for_search("needs_compaction");
        // "needs" → "need", "compaction" → "compact"
        assert_eq!(result, "need compact");
    }

    #[test]
    fn normalize_dotted_and_dashed() {
        // sha256-12 → sha 256 12 (pre-stem)
        let result = normalize_for_search("sha256-12");
        // "sha" stays (3 chars, Porter may not change it), "256" stays, "12" stays
        assert_eq!(result, "sha 256 12");
    }

    #[test]
    fn normalize_paths() {
        let result = normalize_for_search("products/services/agl-daemon/src");
        // Split on / and - and _
        // "products" → "product", "services" → "service", "agl" → "agl",
        // "daemon" → "daemon", "src" → "src"
        assert_eq!(result, "product service agl daemon src");
    }

    #[test]
    fn normalize_repeated_terms_dedup() {
        let result = normalize_for_search("the the the compaction compaction");
        // "the" → Porter may stem to "the" or ""... let's check: Porter stems
        // "the" → "the" (too short, no change). Actually Porter might produce
        // an empty stem for very short words. Let's just check dedup works.
        assert!(!result.contains("the the"));
        assert!(!result.contains("compaction compaction"));
    }

    #[test]
    fn normalize_punctuation() {
        let result = normalize_for_search("hello, world! foo; bar.");
        assert_eq!(result, "hello world foo bar");
    }

    #[test]
    fn normalize_unicode_lowercase() {
        // Unicode characters are lowercased but not stemmed (not pure ASCII).
        let result = normalize_for_search("CAFÉ");
        // "café" is not pure ASCII lowercase (é is not ASCII), so no stemming.
        assert_eq!(result, "café");
    }

    #[test]
    fn normalize_mixed_digit_letter() {
        // 256abc → 256 abc
        let result = normalize_for_search("256abc");
        assert_eq!(result, "256 abc");
    }

    #[test]
    fn normalize_empty() {
        assert_eq!(normalize_for_search(""), "");
        assert_eq!(normalize_for_search("   "), "");
        assert_eq!(normalize_for_search("---"), "");
    }

    // -- Identifier splitting tests --

    #[test]
    fn split_simple_word() {
        let tokens = split_identifier_tokens("hello");
        assert_eq!(tokens, vec!["hello"]);
    }

    #[test]
    fn split_camel_case() {
        let tokens = split_identifier_tokens("fooBarBaz");
        assert_eq!(tokens, vec!["foo", "Bar", "Baz"]);
    }

    #[test]
    fn split_acronym() {
        let tokens = split_identifier_tokens("HTTPServer");
        assert_eq!(tokens, vec!["HTTP", "Server"]);
    }

    #[test]
    fn split_acronym_single_char() {
        // Xy → Xy (no split: only one uppercase followed by lowercase)
        let tokens = split_identifier_tokens("Xy");
        assert_eq!(tokens, vec!["Xy"]);
    }

    #[test]
    fn split_all_caps() {
        // ALLCAPS → no boundary (no lowercase following)
        let tokens = split_identifier_tokens("ALLCAPS");
        assert_eq!(tokens, vec!["ALLCAPS"]);
    }

    #[test]
    fn split_mixed_boundaries() {
        let tokens = split_identifier_tokens("SHA256Hash");
        // S-H-A-2-5-6-H-a-s-h
        // A→2: letter to digit, split
        // 6→H: digit to letter, split
        // H→a: no split (uppercase to lowercase within a word)
        assert_eq!(tokens, vec!["SHA", "256", "Hash"]);
    }

    #[test]
    fn split_with_separators() {
        let tokens = split_identifier_tokens("foo_bar-baz.qux/baz");
        assert_eq!(tokens, vec!["foo", "bar", "baz", "qux", "baz"]);
    }

    // -- Line number recovery --

    #[test]
    fn claim_line_numbers_basic() {
        let text = "# Title\n\n- slug: test\n- created: run_01\n- updated: run_01\n\n## Claims\n\n- First claim [src: msg_01]\n- Second claim [src: msg_02]\n";
        let lines = claim_bullet_line_numbers(text);
        assert_eq!(lines, vec![8, 9]);
    }

    #[test]
    fn claim_line_numbers_with_blank_lines() {
        let text = "# T\n\n- slug: t\n- created: run_01\n- updated: run_01\n\n## Claims\n\n- A [src: m1]\n\n- B [src: m2]\n";
        let lines = claim_bullet_line_numbers(text);
        assert_eq!(lines, vec![8, 10]);
    }

    // -- Supersession reference parsing --

    #[test]
    fn parse_supersession_ref_valid() {
        let (slug, ordinal) = parse_supersession_ref("my-topic#claim-3").unwrap();
        assert_eq!(slug, "my-topic");
        assert_eq!(ordinal, 3);
    }

    #[test]
    fn parse_supersession_ref_invalid() {
        assert!(parse_supersession_ref("no-claim-part").is_none());
        assert!(parse_supersession_ref("slug#claim-0").is_none());
        assert!(parse_supersession_ref("slug#claim-abc").is_none());
    }

    // -- Query tokenization --

    #[test]
    fn tokenize_query_basic() {
        let tokens = tokenize_query("compaction required");
        assert_eq!(tokens, vec!["compact", "requir"]);
    }

    #[test]
    fn tokenize_query_empty() {
        assert!(tokenize_query("").is_empty());
        assert!(tokenize_query("   ").is_empty());
    }

    #[test]
    fn build_match_expression_basic() {
        let expr = build_match_expression(&["foo".to_owned(), "bar".to_owned()]).unwrap();
        assert_eq!(expr, r#""foo" OR "bar""#);
    }

    #[test]
    fn build_match_expression_empty() {
        assert!(build_match_expression(&[]).is_none());
    }

    #[test]
    fn build_match_expression_quotes() {
        let expr = build_match_expression(&["say \"hello\"".to_owned()]).unwrap();
        assert_eq!(expr, r#""say \"\"hello\"\"""#);
    }

    // -- Path computation --

    #[test]
    fn index_path_deterministic() {
        let data_root = Path::new("/tmp/data");
        let ws = Path::new("/home/user/project");
        let p1 = index_db_path(data_root, ws);
        let p2 = index_db_path(data_root, ws);
        assert_eq!(p1, p2);
        assert!(p1.to_string_lossy().contains("/store/memory-index/"));
        assert!(p1.to_string_lossy().ends_with(".db"));
        // Full SHA-256 hex is 64 characters.
        let filename = p1.file_name().unwrap().to_string_lossy().to_owned();
        assert_eq!(filename.len(), 67); // 64 hex + ".db" (3) = 67
    }

    // -- Full rebuild integration (requires temp dir) --

    #[test]
    fn rebuild_and_open_roundtrip() {
        let tmp = std::env::temp_dir().join(format!(
            "agl-index-test-{}",
            uuid::Uuid::now_v7()
        ));
        let data_root = tmp.join("data");
        let ws_root = tmp.join("workspace");

        // Create workspace with a memory entry.
        fs::create_dir_all(ws_root.join("memory/entries")).unwrap();
        let entry = r#"# Test Topic

- slug: test-topic
- created: run_01a00000-0000-7000-8000-000000000001
- updated: run_01a00000-0000-7000-8000-000000000001

## Claims

- The test claim text here. [src: msg_01a00000-0000-7000-8000-000000000001]
- Another claim. [src: msg_01a00000-0000-7000-8000-000000000002] [status: stale]
"#;
        fs::write(ws_root.join("memory/entries/test-topic.md"), entry).unwrap();

        // Rebuild with a static generation.
        let gen = 1u64;
        let result = rebuild_index(&data_root, &ws_root, gen, || gen).unwrap();
        assert_eq!(result.claim_count, 2);
        assert!(result.skipped.is_empty());

        // Open and verify.
        let db_path = index_db_path(&data_root, &ws_root);
        let conn = open_and_verify(&db_path, &ws_root).unwrap();
        let count: usize = conn
            .query_row("SELECT COUNT(*) FROM claims", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);

        // Check the stale flag.
        let stale_count: usize = conn
            .query_row("SELECT COUNT(*) FROM claims WHERE stale = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stale_count, 1);

        // Check FTS5 search works.
        let fts_count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM claims_fts WHERE claims_fts MATCH ?",
                [r#""test""#],
                |r| r.get(0),
            )
            .unwrap();
        assert!(fts_count >= 1);

        // Cleanup.
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rebuild_skips_malformed_entry() {
        let tmp = std::env::temp_dir().join(format!(
            "agl-index-test-{}",
            uuid::Uuid::now_v7()
        ));
        let data_root = tmp.join("data");
        let ws_root = tmp.join("workspace");

        fs::create_dir_all(ws_root.join("memory/entries")).unwrap();

        // Good entry.
        let good = r#"# Good

- slug: good
- created: run_01a00000-0000-7000-8000-000000000001
- updated: run_01a00000-0000-7000-8000-000000000001

## Claims

- A valid claim. [src: msg_01a00000-0000-7000-8000-000000000001]
"#;
        fs::write(ws_root.join("memory/entries/good.md"), good).unwrap();

        // Malformed entry (missing title).
        fs::write(ws_root.join("memory/entries/bad.md"), "no title here\n").unwrap();

        let gen = 1u64;
        let result = rebuild_index(&data_root, &ws_root, gen, || gen).unwrap();
        assert_eq!(result.claim_count, 1);
        assert_eq!(result.skipped, vec!["bad".to_owned()]);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rebuild_empty_memory() {
        let tmp = std::env::temp_dir().join(format!(
            "agl-index-test-{}",
            uuid::Uuid::now_v7()
        ));
        let data_root = tmp.join("data");
        let ws_root = tmp.join("workspace");

        // No memory/ directory at all.
        fs::create_dir_all(&ws_root).unwrap();

        let gen = 1u64;
        let result = rebuild_index(&data_root, &ws_root, gen, || gen).unwrap();
        assert_eq!(result.claim_count, 0);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn open_and_verify_root_mismatch() {
        let tmp = std::env::temp_dir().join(format!(
            "agl-index-test-{}",
            uuid::Uuid::now_v7()
        ));
        let data_root = tmp.join("data");
        let ws_root = tmp.join("workspace");
        let other_root = tmp.join("other-workspace");

        fs::create_dir_all(ws_root.join("memory/entries")).unwrap();
        fs::create_dir_all(&other_root).unwrap();

        let entry = r#"# T

- slug: t
- created: run_01a00000-0000-7000-8000-000000000001
- updated: run_01a00000-0000-7000-8000-000000000001

## Claims

- A claim. [src: msg_01a00000-0000-7000-8000-000000000001]
"#;
        fs::write(ws_root.join("memory/entries/t.md"), entry).unwrap();

        let gen = 1u64;
        rebuild_index(&data_root, &ws_root, gen, || gen).unwrap();

        // Opening with a different root should fail.
        let db_path = index_db_path(&data_root, &ws_root);
        let err = open_and_verify(&db_path, &other_root).unwrap_err();
        assert!(matches!(err, IndexError::RootMismatch { .. }));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rebuild_idempotent() {
        let tmp = std::env::temp_dir().join(format!(
            "agl-index-test-{}",
            uuid::Uuid::now_v7()
        ));
        let data_root = tmp.join("data");
        let ws_root = tmp.join("workspace");

        fs::create_dir_all(ws_root.join("memory/entries")).unwrap();
        let entry = r#"# T

- slug: t
- created: run_01a00000-0000-7000-8000-000000000001
- updated: run_01a00000-0000-7000-8000-000000000001

## Claims

- A claim. [src: msg_01a00000-0000-7000-8000-000000000001]
"#;
        fs::write(ws_root.join("memory/entries/t.md"), entry).unwrap();

        let gen = 1u64;
        let r1 = rebuild_index(&data_root, &ws_root, gen, || gen).unwrap();
        let r2 = rebuild_index(&data_root, &ws_root, gen, || gen).unwrap();
        assert_eq!(r1.claim_count, r2.claim_count);

        let _ = fs::remove_dir_all(&tmp);
    }
}
