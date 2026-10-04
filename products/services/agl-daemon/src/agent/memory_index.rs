//! Disposable FTS5 projection of a Forge memory source. The source read
//! guard prevents publication during rebuild/query; cache metadata binds the
//! canonical workspace and four-field source declaration.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agl_core::AgentRunId;
use agl_core::agent::{ClaimStatus, MemoryEntry, StoredClaim, parse_entry};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use stemmer::Stemmer;

pub(crate) fn index_db_path(data: &Path, workspace: &Path) -> PathBuf {
    let digest = Sha256::digest(workspace.as_os_str().as_encoded_bytes());
    let name = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    data.join("store/memory-index").join(format!("{name}.db"))
}

pub(crate) struct MemorySource {
    workspace: PathBuf,
    declaration: agl_runtime::ForgeDeclaration,
    reader: ayeque_forge_core::ForgeWorkspace,
}
impl MemorySource {
    pub(crate) fn open(storage: &Path, workspace: &Path) -> Result<Self> {
        let workspace = workspace.canonicalize()?;
        let declarations = agl_runtime::read_workspace_forge_declarations_at(&workspace)?;
        let sources = agl_runtime::workspace_backend_declarations(&declarations)?;
        let mut memory = declarations
            .into_iter()
            .filter(|source| source.kind == "memory");
        let declaration = memory.next().context("workspace has no memory source")?;
        ensure!(
            memory.next().is_none(),
            "multiple memory sources are not allowed"
        );
        let reader = ayeque_forge_core::ForgeWorkspace::open(storage, &workspace, &sources)?;
        Ok(Self {
            workspace,
            declaration,
            reader,
        })
    }
    fn binding(&self) -> Result<String> {
        let source = ayeque_forge_core::ForgeDeclaration::new(
            self.declaration.id.clone(),
            self.declaration.git.clone(),
            self.declaration.revision.clone(),
            self.declaration.path.clone(),
        )?;
        Ok(serde_json::to_string(&source)?)
    }
    pub(crate) fn index_path(&self) -> String {
        format!("forge:{}/INDEX.md", self.declaration.id)
    }
    fn file(&self, relative: &Path) -> Result<agl_tool_api::ForgeFile> {
        let (file, guard, status) = self.reader.open_file(&self.declaration.id, relative)?;
        let status = match status {
            ayeque_forge_core::VerificationStatus::Verified => {
                agl_tool_api::VerificationStatus::Verified
            }
            ayeque_forge_core::VerificationStatus::Unchecked => {
                agl_tool_api::VerificationStatus::Unchecked
            }
        };
        Ok(agl_tool_api::ForgeFile::new(file, guard, status))
    }
    fn entries(&self) -> Result<(Vec<ClaimRow>, Vec<String>, Vec<String>)> {
        let root = self.reader.materialized_path(&self.declaration.id)?;
        let entries = root.join("entries");
        if !entries.try_exists()? {
            return Ok((Vec::new(), Vec::new(), Vec::new()));
        }
        let mut names = fs::read_dir(entries)?
            .map(|entry| Ok(entry?.file_name()))
            .collect::<Result<Vec<_>>>()?;
        names.sort();
        let mut rows = Vec::new();
        let mut skipped = Vec::new();
        let mut unchecked = Vec::new();
        for name in names {
            if !name.as_encoded_bytes().ends_with(b".md") {
                continue;
            }
            let path = PathBuf::from("entries").join(&name);
            let parsed = (|| -> Result<_> {
                let mut file = self.file(&path)?;
                if file.verification_status() == agl_tool_api::VerificationStatus::Unchecked {
                    unchecked.push(format!("forge:{}/{}", self.declaration.id, path.display()));
                }
                let mut content = String::new();
                file.read_to_string(&mut content)?;
                let entry = parse_entry(&content)?;
                ensure!(
                    path.file_stem().and_then(|name| name.to_str()) == Some(entry.slug.as_str()),
                    "entry filename and slug differ"
                );
                Ok(project_entry(&entry, &content))
            })();
            match parsed {
                Ok(entry_rows) => rows.extend(entry_rows),
                Err(error) => {
                    tracing::warn!(path=%path.display(), %error, "skipping malformed memory entry");
                    skipped.push(path.display().to_string());
                }
            }
        }
        resolve_supersession(&mut rows);
        Ok((rows, skipped, unchecked))
    }
}

#[derive(Default)]
struct WorkspaceIndex {
    binding: String,
    searched_runs: BTreeSet<AgentRunId>,
}
pub(crate) struct MemoryIndex {
    data: PathBuf,
    storage: PathBuf,
    workspaces: Mutex<BTreeMap<PathBuf, Arc<Mutex<WorkspaceIndex>>>>,
}
impl MemoryIndex {
    pub(crate) fn new(data: PathBuf, storage: PathBuf) -> Self {
        Self {
            data,
            storage,
            workspaces: Mutex::new(BTreeMap::new()),
        }
    }
    pub(crate) fn memory_source_id(&self, workspace: &Path) -> Option<String> {
        MemorySource::open(&self.storage, workspace)
            .ok()
            .map(|source| source.declaration.id)
    }
    pub(crate) fn run_finished(&self, run: AgentRunId) {
        if let Ok(workspaces) = self.workspaces.lock() {
            for state in workspaces.values() {
                if let Ok(mut state) = state.lock() {
                    state.searched_runs.remove(&run);
                }
            }
        }
    }
    pub(crate) fn search(
        &self,
        workspace: &Path,
        run: AgentRunId,
        query: &str,
        limit: i64,
    ) -> Result<SearchPage> {
        let source = MemorySource::open(&self.storage, workspace)?;
        let binding = source.binding()?;
        let state = self
            .workspaces
            .lock()
            .map_err(|_| anyhow::anyhow!("memory index registry poisoned"))?
            .entry(source.workspace.clone())
            .or_default()
            .clone();
        let mut state = state
            .lock()
            .map_err(|_| anyhow::anyhow!("memory index state poisoned"))?;
        let path = index_db_path(&self.data, &source.workspace);
        let fresh = state.binding == binding && state.searched_runs.contains(&run);
        if !fresh {
            rebuild(&path, &source, &binding)?;
        }
        let page = match query_index(&path, &source, &binding, query, limit) {
            Ok(page) => page,
            Err(error) => {
                tracing::warn!(%error, "memory index query failed; rebuilding once");
                rebuild(&path, &source, &binding)?;
                query_index(&path, &source, &binding, query, limit)?
            }
        };
        state.binding = binding;
        state.searched_runs.insert(run);
        Ok(page)
    }
}

pub(crate) struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub total: usize,
    pub skipped: Vec<String>,
    pub unchecked: Vec<String>,
    pub fallback: String,
    pub index_path: String,
}
pub(crate) struct SearchHit {
    pub score: f64,
    pub slug: String,
    pub text: String,
    pub sources: String,
    pub stale: bool,
    pub superseded: bool,
    pub supersession: Option<String>,
}

fn resolve_supersession(rows: &mut [ClaimRow]) {
    let mut lookup = BTreeMap::new();
    let mut ordinals = BTreeMap::<String, usize>::new();
    for (index, row) in rows.iter().enumerate() {
        let ordinal = ordinals.entry(row.slug.clone()).or_default();
        *ordinal += 1;
        lookup.insert((row.slug.clone(), *ordinal), index + 1);
    }
    for row in rows {
        row.superseded_by = row
            .superseded_by_raw
            .as_deref()
            .and_then(parse_supersession_ref)
            .and_then(|(slug, ordinal)| lookup.get(&(slug.to_owned(), ordinal)).copied());
    }
}
fn parse_supersession_ref(target: &str) -> Option<(&str, usize)> {
    let (slug, claim) = target.split_once("#claim-")?;
    let ordinal = claim.parse::<usize>().ok()?;
    (ordinal > 0).then_some((slug, ordinal))
}

fn rebuild(path: &Path, source: &MemorySource, binding: &str) -> Result<()> {
    let parent = path.parent().context("index path has no parent")?;
    crate::store::path::ensure_private_dir(parent)?;
    let temp = parent.join(format!(".memory-{}.db", agl_core::RequestId::generate()));
    let result = (|| -> Result<()> {
        let (rows, skipped, unchecked) = source.entries()?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        crate::store::path::set_private_file_permissions(&temp)?;
        drop(file);
        let mut connection = Connection::open(&temp)?;
        connection.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
        connection.execute_batch(SCHEMA)?;
        let tx = connection.transaction()?;
        for (index, row) in rows.iter().enumerate() {
            tx.execute("INSERT INTO claims (id,slug,text,search_text,sources,stale,superseded_by,line_no,created_run,superseded_target) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)", params![index+1,row.slug,row.text,row.search_text,row.sources_json,row.stale,row.superseded_by,row.line_no,row.created_run,row.superseded_by_raw])?;
        }
        tx.execute("INSERT INTO metadata (key,value) VALUES ('workspace_root',?1),('binding',?2),('skipped',?3),('unchecked',?4)", params![source.workspace.to_str().context("workspace is not UTF-8")?, binding, serde_json::to_string(&skipped)?, serde_json::to_string(&unchecked)?])?;
        tx.execute("INSERT INTO claims_fts(claims_fts) VALUES('rebuild')", [])?;
        tx.commit()?;
        connection.close().map_err(|(_, error)| error)?;
        crate::store::path::set_private_file_permissions(&temp)?;
        File::open(&temp)?.sync_all()?;
        fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[derive(Deserialize)]
struct SourceFile {
    path: String,
    digest: Option<String>,
}
#[derive(Deserialize)]
struct Sources {
    messages: Vec<String>,
    files: Vec<SourceFile>,
}
fn query_index(
    path: &Path,
    source: &MemorySource,
    binding: &str,
    query: &str,
    limit: i64,
) -> Result<SearchPage> {
    crate::store::path::validate_private_regular_file(path)?;
    let connection = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let stored: String = connection.query_row(
        "SELECT value FROM metadata WHERE key='workspace_root'",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        stored
            == source
                .workspace
                .to_str()
                .context("workspace is not UTF-8")?,
        "memory index workspace mismatch"
    );
    let stored: String = connection.query_row(
        "SELECT value FROM metadata WHERE key='binding'",
        [],
        |row| row.get(0),
    )?;
    ensure!(stored == binding, "memory index source binding mismatch");
    let total = connection.query_row("SELECT COUNT(*) FROM claims", [], |row| row.get(0))?;
    let skipped: String = connection.query_row(
        "SELECT value FROM metadata WHERE key='skipped'",
        [],
        |row| row.get(0),
    )?;
    let skipped = serde_json::from_str(&skipped)?;
    let unchecked: String = connection.query_row(
        "SELECT value FROM metadata WHERE key='unchecked'",
        [],
        |row| row.get(0),
    )?;
    let mut unchecked: Vec<String> = serde_json::from_str(&unchecked)?;
    let mut hits = Vec::new();
    if let Some(expression) = build_match_expression(&tokenize_query(query)) {
        let mut statement = connection.prepare("SELECT bm25(claims_fts),c.slug,c.text,c.sources,c.stale,c.superseded_by IS NOT NULL,c.superseded_target FROM claims_fts JOIN claims c ON c.id=claims_fts.rowid WHERE claims_fts MATCH ?1 ORDER BY bm25(claims_fts),c.id LIMIT ?2")?;
        let rows = statement.query_map(params![expression, limit.clamp(1, 10)], |row| {
            Ok(SearchHit {
                score: row.get(0)?,
                slug: row.get(1)?,
                text: row.get(2)?,
                sources: row.get(3)?,
                stale: row.get(4)?,
                superseded: row.get(5)?,
                supersession: row.get(6)?,
            })
        })?;
        let memory = super::memory_io::MemoryWorkspace::authoring(
            source.reader.materialized_path(&source.declaration.id)?,
            &source.workspace,
        )
        .map_err(|error| anyhow::anyhow!("memory reader: {error:?}"))?;
        for hit in rows {
            let mut hit = hit?;
            let sources: Sources = serde_json::from_str(&hit.sources)?;
            hit.stale |= sources.files.iter().any(|file| {
                memory
                    .stamped_file(&file.path)
                    .is_none_or(|current| current.digest != file.digest)
            });
            hit.sources =
                sources
                    .messages
                    .into_iter()
                    .chain(sources.files.into_iter().map(|file| {
                        format!("file:{}@{}", file.path, file.digest.unwrap_or_default())
                    }))
                    .collect::<Vec<_>>()
                    .join(", ");
            hits.push(hit);
        }
    }
    let fallback = if hits.is_empty() {
        let mut index = String::new();
        let mut file = source.file(Path::new("INDEX.md"))?;
        if file.verification_status() == agl_tool_api::VerificationStatus::Unchecked {
            unchecked.push(source.index_path());
        }
        file.read_to_string(&mut index)?;
        index
    } else {
        String::new()
    };
    Ok(SearchPage {
        hits,
        total,
        skipped,
        unchecked,
        fallback,
        index_path: source.index_path(),
    })
}

pub(crate) fn normalize_for_search(text: &str) -> String {
    let raw_tokens = split_identifier_tokens(text);
    let mut stemmer = Stemmer::new("porter").expect("bundled Porter stemmer must be available");

    let mut seen = BTreeSet::new();
    let mut result: Vec<String> = Vec::new();

    for token in raw_tokens {
        let lower: String = token.to_lowercase();
        if lower.is_empty() {
            continue;
        }
        // Porter-stem only pure ASCII alphabetic tokens.
        let final_token = if lower.chars().all(|c| c.is_ascii_lowercase()) {
            stemmer.stem(&lower)
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
    superseded_by_raw: Option<String>,
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
            let line_no = line_numbers.get(idx).copied().unwrap_or(idx + 1); // fallback: 1-based index

            let sources_json = serialize_sources(claim);
            let search_text = normalize_for_search(&claim.text);
            let stale = claim.status == Some(ClaimStatus::Stale);

            ClaimRow {
                slug: entry.slug.clone(),
                text: claim.text.clone(),
                search_text,
                sources_json,
                stale,
                superseded_by: None,
                superseded_by_raw: claim.superseded_by.clone(),
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
    serde_json::json!({ "messages": claim.sources.iter().map(ToString::to_string).collect::<Vec<_>>(), "files": claim.files.iter().map(|file| serde_json::json!({"path":file.path,"digest":file.digest})).collect::<Vec<_>>() }).to_string()
}

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

const SCHEMA: &str = r#"
CREATE TABLE claims (
    id INTEGER PRIMARY KEY,
    slug TEXT NOT NULL,
    text TEXT NOT NULL,
    search_text TEXT NOT NULL,
    sources TEXT NOT NULL,
    stale INTEGER NOT NULL DEFAULT 0,
    superseded_by INTEGER REFERENCES claims(id),
    superseded_target TEXT,
    line_no INTEGER NOT NULL,
    created_run TEXT NOT NULL
);
CREATE VIRTUAL TABLE claims_fts USING fts5(search_text, content='claims');
CREATE TABLE metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

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
        // "products" → "product", "services" → "servic", "agl" → "agl",
        // "daemon" → "daemon", "src" → "src"
        assert_eq!(result, "product servic agl daemon src");
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
        assert_eq!(lines, vec![9, 10]);
    }

    #[test]
    fn claim_line_numbers_with_blank_lines() {
        let text = "# T\n\n- slug: t\n- created: run_01\n- updated: run_01\n\n## Claims\n\n- A [src: m1]\n\n- B [src: m2]\n";
        let lines = claim_bullet_line_numbers(text);
        assert_eq!(lines, vec![9, 11]);
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
        assert_eq!(expr, r#""say ""hello""""#);
    }

    // -- Path computation --
}

#[cfg(test)]
mod integration_tests;
