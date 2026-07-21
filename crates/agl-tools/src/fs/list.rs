use std::collections::BTreeMap;
use std::fs;
use std::mem;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use agl_ids::RequestId;
use anyhow::{Context, Result, bail, ensure};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{CoreTools, FS_LIST_TOOL_ID, PathKind, normalize_repo_path};

const DEFAULT_PAGE_SIZE: u16 = 100;
const MAX_PAGE_SIZE: u16 = 500;
const MAX_ACTIVE_QUERIES: usize = 4;
const MAX_RETAINED_ENTRIES: usize = 100_000;
const MAX_RETAINED_BYTES: usize = 8 * 1024 * 1024;
const QUERY_TTL_MS: u64 = 10 * 60 * 1_000;
const MAX_VISITED_DIRECTORIES: usize = 10_000;
const MAX_EXAMINED_ENTRIES: usize = 100_000;
const MAX_GLOB_BYTES: usize = 256;
const MAX_CURSOR_BYTES: usize = 128;

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ListArgs {
    pub path: String,
    #[serde(default)]
    pub recursive: bool,
    #[serde(default = "default_page_size")]
    pub page_size: u16,
    pub cursor: Option<String>,
    #[serde(default)]
    pub kind: EntryKindFilter,
    pub name_glob: Option<String>,
    #[serde(default)]
    pub match_on: GlobTarget,
    #[serde(default)]
    pub case: GlobCase,
}

fn default_page_size() -> u16 {
    DEFAULT_PAGE_SIZE
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum EntryKindFilter {
    #[default]
    Any,
    File,
    Directory,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum GlobTarget {
    #[default]
    Basename,
    RelativePath,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum GlobCase {
    #[default]
    Sensitive,
    AsciiInsensitive,
}

#[derive(Debug, Default)]
pub(super) struct ListQueryRegistry {
    runs: BTreeMap<String, RunQueryUsage>,
    queries: BTreeMap<String, ListQueryState>,
}

#[derive(Debug, Default)]
struct RunQueryUsage {
    active_queries: usize,
    retained_entries: usize,
    retained_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct QueryFingerprint {
    path: String,
    recursive: bool,
    page_size: u16,
    kind: EntryKindFilter,
    name_glob: Option<String>,
    match_on: GlobTarget,
    case: GlobCase,
}

#[derive(Debug)]
struct ListQueryState {
    run_id: String,
    query: QueryFingerprint,
    entries: Vec<ListEntry>,
    next_index: usize,
    directory_fingerprints: Vec<DirectoryFingerprint>,
    retained_bytes: usize,
    expires_at_unix_ms: u64,
}

#[derive(Clone, Debug)]
struct ListEntry {
    path: String,
    basename: String,
    kind: EntryKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryKind {
    File,
    Directory,
}

impl EntryKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DirectoryFingerprint {
    path: String,
    identity: FileIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[derive(Debug)]
struct ScanResult {
    entries: Vec<ListEntry>,
    directories: Vec<DirectoryFingerprint>,
    scan_limit_reached: bool,
}

struct BoundedDirectoryEntries {
    entries: Vec<fs::DirEntry>,
    has_unexamined_entry: bool,
}

#[derive(Clone, Copy, Debug)]
struct ScanLimits {
    visited_directories: usize,
    examined_entries: usize,
}

impl ScanLimits {
    const PRODUCTION: Self = Self {
        visited_directories: MAX_VISITED_DIRECTORIES,
        examined_entries: MAX_EXAMINED_ENTRIES,
    };
}

#[derive(Clone, Debug)]
enum GlobToken {
    Literal(char),
    Question,
    Star,
    DoubleStar,
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
}

pub(super) fn list_page(tools: &CoreTools, run_id: &str, args: ListArgs) -> Result<Value> {
    validate_args(&args)?;
    let pattern = args
        .name_glob
        .as_deref()
        .map(|pattern| parse_glob(pattern, args.case))
        .transpose()?;
    let now = unix_now_ms()?;

    if let Some(cursor) = args.cursor.as_deref() {
        let query = query_fingerprint(normalized_list_path(&args.path)?, &args);
        return continue_query(tools, run_id, &query, cursor, now);
    }

    let path = tools.resolve_existing_path(&args.path, PathKind::Directory, true)?;
    let canonical_path = tools.display_path(&path);
    let query = query_fingerprint(canonical_path.clone(), &args);
    let reservation = ListScanReservation::begin(&tools.list_queries, run_id, now)?;
    let mut scanned = scan_entries(tools, &path, &query, pattern.as_deref())?;
    if scanned.scan_limit_reached {
        let page = render_page(
            &canonical_path,
            &scanned.entries[..scanned.entries.len().min(usize::from(args.page_size))],
            PageOutcome::ScanLimit,
        );
        reservation.release();
        return Ok(page);
    }

    if scanned.entries.len() <= usize::from(args.page_size) {
        let page = render_page(&canonical_path, &scanned.entries, PageOutcome::Complete);
        reservation.release();
        return Ok(page);
    }

    compact_scan_result(&mut scanned);
    let first_page = scanned.entries[..usize::from(args.page_size)].to_vec();
    let retained_bytes = retained_size(run_id, &query, &scanned)?;
    let state = ListQueryState {
        run_id: run_id.to_string(),
        query,
        entries: scanned.entries,
        next_index: usize::from(args.page_size),
        directory_fingerprints: scanned.directories,
        retained_bytes,
        expires_at_unix_ms: query_deadline(now)?,
    };
    let cursor = reservation.publish(state)?;
    Ok(render_page(
        &canonical_path,
        &first_page,
        PageOutcome::PageBoundary(cursor),
    ))
}

fn query_fingerprint(path: String, args: &ListArgs) -> QueryFingerprint {
    QueryFingerprint {
        path,
        recursive: args.recursive,
        page_size: args.page_size,
        kind: args.kind,
        name_glob: args.name_glob.clone(),
        match_on: args.match_on,
        case: args.case,
    }
}

fn normalized_list_path(raw: &str) -> Result<String> {
    let path = normalize_repo_path(raw, true)?;
    if path.as_os_str().is_empty() {
        return Ok(".".to_string());
    }
    path.to_str()
        .map(str::to_string)
        .context("non_utf8_path: fs.list query path is not valid UTF-8")
}

fn validate_args(args: &ListArgs) -> Result<()> {
    ensure!(
        (1..=MAX_PAGE_SIZE).contains(&args.page_size),
        "fs.list page_size must be between 1 and {MAX_PAGE_SIZE}"
    );
    if let Some(cursor) = &args.cursor {
        ensure!(
            !cursor.is_empty() && cursor.len() <= MAX_CURSOR_BYTES,
            "fs.list cursor is invalid"
        );
        ensure!(
            cursor
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
            "fs.list cursor is invalid"
        );
    }
    if let Some(pattern) = &args.name_glob {
        ensure!(
            !pattern.is_empty() && pattern.len() <= MAX_GLOB_BYTES,
            "invalid_glob: fs.list name_glob must contain 1..={MAX_GLOB_BYTES} UTF-8 bytes"
        );
        ensure!(
            !pattern.chars().any(char::is_control),
            "invalid_glob: fs.list name_glob contains a control character"
        );
    }
    Ok(())
}

fn continue_query(
    tools: &CoreTools,
    run_id: &str,
    query: &QueryFingerprint,
    cursor: &str,
    now: u64,
) -> Result<Value> {
    let mut checked_out = CheckedOutQuery::new(&tools.list_queries, run_id, query, cursor, now)?;
    if let Err(error) = validate_directories(tools, &checked_out.state().directory_fingerprints) {
        bail!("cursor_stale: {error:#}");
    }

    let start = checked_out.state().next_index;
    let end = start
        .saturating_add(usize::from(query.page_size))
        .min(checked_out.state().entries.len());
    let path = checked_out.state().query.path.clone();
    let entries = checked_out.state().entries[start..end].to_vec();
    if end == checked_out.state().entries.len() {
        checked_out.release();
        return Ok(render_page(&path, &entries, PageOutcome::Complete));
    }

    checked_out.state_mut().next_index = end;
    checked_out.state_mut().expires_at_unix_ms = query_deadline(now)?;
    let next_cursor = checked_out.rotate();
    Ok(render_page(
        &path,
        &entries,
        PageOutcome::PageBoundary(next_cursor),
    ))
}

struct ListScanReservation<'a> {
    registry: &'a Mutex<ListQueryRegistry>,
    run_id: String,
    active: bool,
}

impl<'a> ListScanReservation<'a> {
    fn begin(registry: &'a Mutex<ListQueryRegistry>, run_id: &str, now: u64) -> Result<Self> {
        let mut state = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.prune_expired(now);
        state.reserve_active(run_id)?;
        Ok(Self {
            registry,
            run_id: run_id.to_string(),
            active: true,
        })
    }

    fn release(mut self) {
        self.release_inner();
    }

    fn publish(mut self, state: ListQueryState) -> Result<String> {
        let result = {
            let mut registry = self
                .registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.publish_reserved(state)
        };
        if result.is_err() {
            self.release_inner();
        } else {
            self.active = false;
        }
        result
    }

    fn release_inner(&mut self) {
        if !self.active {
            return;
        }
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.release_unpublished(&self.run_id);
        self.active = false;
    }
}

impl Drop for ListScanReservation<'_> {
    fn drop(&mut self) {
        self.release_inner();
    }
}

struct CheckedOutQuery<'a> {
    registry: &'a Mutex<ListQueryRegistry>,
    state: Option<ListQueryState>,
}

impl<'a> CheckedOutQuery<'a> {
    fn new(
        registry: &'a Mutex<ListQueryRegistry>,
        run_id: &str,
        query: &QueryFingerprint,
        cursor: &str,
        now: u64,
    ) -> Result<Self> {
        let state = {
            let mut registry = registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.prune_expired(now);
            let state = registry.queries.get(cursor).ok_or_else(|| {
                anyhow::anyhow!(
                    "cursor_stale: fs.list cursor is unknown, expired, or already consumed"
                )
            })?;
            ensure!(
                state.run_id == run_id,
                "cursor_stale: fs.list cursor belongs to another run"
            );
            ensure!(
                &state.query == query,
                "cursor_query_mismatch: fs.list query fields changed while continuing"
            );
            registry
                .queries
                .remove(cursor)
                .context("fs.list cursor disappeared while checking it out")?
        };
        Ok(Self {
            registry,
            state: Some(state),
        })
    }

    fn state(&self) -> &ListQueryState {
        self.state
            .as_ref()
            .expect("checked-out fs.list query state is present")
    }

    fn state_mut(&mut self) -> &mut ListQueryState {
        self.state
            .as_mut()
            .expect("checked-out fs.list query state is present")
    }

    fn rotate(mut self) -> String {
        let state = self
            .state
            .take()
            .expect("checked-out fs.list query state is present");
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.rotate_checked_out(state)
    }

    fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.release_state(&state);
    }
}

impl Drop for CheckedOutQuery<'_> {
    fn drop(&mut self) {
        self.release_inner();
    }
}

impl ListQueryRegistry {
    fn reserve_active(&mut self, run_id: &str) -> Result<()> {
        let usage = self.runs.entry(run_id.to_string()).or_default();
        if usage.active_queries >= MAX_ACTIVE_QUERIES {
            bail!("cursor_capacity: fs.list already has {MAX_ACTIVE_QUERIES} active queries");
        }
        usage.active_queries += 1;
        Ok(())
    }

    fn release_unpublished(&mut self, run_id: &str) {
        if let Some(usage) = self.runs.get_mut(run_id) {
            usage.active_queries = usage.active_queries.saturating_sub(1);
        }
        self.remove_empty_usage(run_id);
    }

    fn publish_reserved(&mut self, state: ListQueryState) -> Result<String> {
        let entry_count = state.entries.len();
        let usage = self
            .runs
            .get_mut(&state.run_id)
            .context("fs.list active-query reservation disappeared")?;
        let retained_entries = usage
            .retained_entries
            .checked_add(entry_count)
            .context("cursor_capacity: fs.list retained-entry accounting overflowed")?;
        let retained_bytes = usage
            .retained_bytes
            .checked_add(state.retained_bytes)
            .context("cursor_capacity: fs.list retained-byte accounting overflowed")?;
        if retained_entries > MAX_RETAINED_ENTRIES || retained_bytes > MAX_RETAINED_BYTES {
            bail!("cursor_capacity: fs.list retained-query budget would be exceeded");
        }
        usage.retained_entries = retained_entries;
        usage.retained_bytes = retained_bytes;
        let cursor = self.fresh_cursor();
        self.queries.insert(cursor.clone(), state);
        Ok(cursor)
    }

    fn rotate_checked_out(&mut self, state: ListQueryState) -> String {
        let cursor = self.fresh_cursor();
        self.queries.insert(cursor.clone(), state);
        cursor
    }

    fn release_state(&mut self, state: &ListQueryState) {
        if let Some(usage) = self.runs.get_mut(&state.run_id) {
            usage.active_queries = usage.active_queries.saturating_sub(1);
            usage.retained_entries = usage.retained_entries.saturating_sub(state.entries.len());
            usage.retained_bytes = usage.retained_bytes.saturating_sub(state.retained_bytes);
        }
        self.remove_empty_usage(&state.run_id);
    }

    fn fresh_cursor(&self) -> String {
        loop {
            let request_id = RequestId::generate();
            let payload = request_id
                .as_str()
                .strip_prefix("req_")
                .unwrap_or(request_id.as_str());
            let cursor = format!("list_{payload}");
            if !self.queries.contains_key(&cursor) {
                return cursor;
            }
        }
    }

    fn prune_expired(&mut self, now: u64) {
        let expired = self
            .queries
            .iter()
            .filter_map(|(cursor, state)| {
                (state.expires_at_unix_ms <= now).then_some(cursor.clone())
            })
            .collect::<Vec<_>>();
        for cursor in expired {
            if let Some(state) = self.queries.remove(&cursor) {
                self.release_state(&state);
            }
        }
    }

    fn remove_empty_usage(&mut self, run_id: &str) {
        if self.runs.get(run_id).is_some_and(|usage| {
            usage.active_queries == 0 && usage.retained_entries == 0 && usage.retained_bytes == 0
        }) {
            self.runs.remove(run_id);
        }
    }
}

fn scan_entries(
    tools: &CoreTools,
    root: &Path,
    query: &QueryFingerprint,
    pattern: Option<&[GlobToken]>,
) -> Result<ScanResult> {
    scan_entries_with_limits(tools, root, query, pattern, ScanLimits::PRODUCTION)
}

fn scan_entries_with_limits(
    tools: &CoreTools,
    root: &Path,
    query: &QueryFingerprint,
    pattern: Option<&[GlobToken]>,
    limits: ScanLimits,
) -> Result<ScanResult> {
    let mut pending = vec![root.to_path_buf()];
    let mut entries = Vec::new();
    let mut directories = Vec::new();
    let mut visited_directories = 0_usize;
    let mut examined_entries = 0_usize;

    while let Some(directory) = pending.pop() {
        if visited_directories == limits.visited_directories {
            return finish_scan(tools, entries, directories, true);
        }
        visited_directories += 1;
        let before = directory_fingerprint(tools, &directory)?;
        directories.push(before.clone());
        let mut child_directories = Vec::new();
        let remaining_entries = limits.examined_entries.saturating_sub(examined_entries);
        let directory_entries = bounded_sorted_dir_entries(&directory, remaining_entries)?;
        for entry in directory_entries.entries {
            examined_entries += 1;
            let name = entry.file_name();
            let name = name.to_str().ok_or_else(|| {
                anyhow::anyhow!("non_utf8_path: fs.list encountered a non-UTF-8 Linux filename")
            })?;
            if name == ".git" {
                continue;
            }
            let file_type = entry
                .file_type()
                .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
            if file_type.is_symlink() {
                continue;
            }
            let kind = if file_type.is_dir() {
                EntryKind::Directory
            } else if file_type.is_file() {
                EntryKind::File
            } else {
                bail!(
                    "unsupported_path_type: fs.list cannot represent {}",
                    entry.path().display()
                );
            };
            let relative = tools.display_path(&entry.path());
            if query.recursive && kind == EntryKind::Directory {
                child_directories.push(entry.path());
            }
            if kind_matches(query.kind, kind)
                && pattern.is_none_or(|pattern| {
                    let candidate = match query.match_on {
                        GlobTarget::Basename => name,
                        GlobTarget::RelativePath => &relative,
                    };
                    glob_matches(pattern, candidate, query.case)
                })
            {
                entries.push(ListEntry {
                    path: relative,
                    basename: name.to_string(),
                    kind,
                });
            }
        }
        ensure_directory_unchanged(tools, &directory, &before)?;
        if directory_entries.has_unexamined_entry {
            return finish_scan(tools, entries, directories, true);
        }
        if query.recursive {
            child_directories.reverse();
            pending.extend(child_directories);
        }
    }

    finish_scan(tools, entries, directories, false)
}

fn bounded_sorted_dir_entries(path: &Path, maximum: usize) -> Result<BoundedDirectoryEntries> {
    let mut source = fs::read_dir(path)
        .with_context(|| format!("failed to read directory {}", path.display()))?;
    let mut entries = Vec::with_capacity(maximum.min(4_096));
    while entries.len() < maximum {
        let Some(entry) = source.next() else {
            entries.sort_by_key(fs::DirEntry::file_name);
            return Ok(BoundedDirectoryEntries {
                entries,
                has_unexamined_entry: false,
            });
        };
        entries
            .push(entry.with_context(|| {
                format!("failed to read directory entry in {}", path.display())
            })?);
    }
    let has_unexamined_entry = source
        .next()
        .transpose()
        .with_context(|| format!("failed to read directory entry in {}", path.display()))?
        .is_some();
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(BoundedDirectoryEntries {
        entries,
        has_unexamined_entry,
    })
}

fn ensure_directory_unchanged(
    tools: &CoreTools,
    path: &Path,
    expected: &DirectoryFingerprint,
) -> Result<()> {
    let actual = directory_fingerprint(tools, path)?;
    ensure!(
        actual == *expected,
        "cursor_stale: fs.list directory changed while it was being scanned"
    );
    Ok(())
}

fn finish_scan(
    tools: &CoreTools,
    entries: Vec<ListEntry>,
    mut directories: Vec<DirectoryFingerprint>,
    scan_limit_reached: bool,
) -> Result<ScanResult> {
    if let Err(error) = validate_directories(tools, &directories) {
        bail!("cursor_stale: fs.list traversal changed before publication: {error:#}");
    }
    directories.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    Ok(ScanResult {
        entries: sorted_entries(entries),
        directories,
        scan_limit_reached,
    })
}

fn sorted_entries(mut entries: Vec<ListEntry>) -> Vec<ListEntry> {
    entries.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    entries
}

fn compact_scan_result(scanned: &mut ScanResult) {
    for entry in &mut scanned.entries {
        entry.path.shrink_to_fit();
        entry.basename.shrink_to_fit();
    }
    for directory in &mut scanned.directories {
        directory.path.shrink_to_fit();
    }
    scanned.entries.shrink_to_fit();
    scanned.directories.shrink_to_fit();
}

fn kind_matches(filter: EntryKindFilter, kind: EntryKind) -> bool {
    matches!(
        (filter, kind),
        (EntryKindFilter::Any, _)
            | (EntryKindFilter::File, EntryKind::File)
            | (EntryKindFilter::Directory, EntryKind::Directory)
    )
}

fn directory_fingerprint(tools: &CoreTools, path: &Path) -> Result<DirectoryFingerprint> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect listing directory {}", path.display()))?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "listing directory changed type: {}",
        path.display()
    );
    Ok(DirectoryFingerprint {
        path: tools.display_path(path),
        identity: file_identity(&metadata)?,
    })
}

fn validate_directories(tools: &CoreTools, expected: &[DirectoryFingerprint]) -> Result<()> {
    for directory in expected {
        let path = if directory.path == "." {
            tools.root.clone()
        } else {
            tools.root.join(&directory.path)
        };
        tools.reject_symlink_components(&path, &directory.path)?;
        let actual = directory_fingerprint(tools, &path)?;
        ensure!(
            actual == *directory,
            "listing directory changed: {}",
            directory.path
        );
    }
    Ok(())
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    })
}

#[cfg(not(unix))]
fn file_identity(metadata: &fs::Metadata) -> Result<FileIdentity> {
    let modified = metadata
        .modified()
        .context("failed to inspect listing directory modification time")?
        .duration_since(UNIX_EPOCH)
        .context("listing directory modification time predates the Unix epoch")?;
    Ok(FileIdentity {
        device: 0,
        inode: 0,
        modified_seconds: i64::try_from(modified.as_secs()).unwrap_or(i64::MAX),
        modified_nanoseconds: i64::from(modified.subsec_nanos()),
        changed_seconds: i64::try_from(modified.as_secs()).unwrap_or(i64::MAX),
        changed_nanoseconds: i64::from(modified.subsec_nanos()),
    })
}

fn retained_size(run_id: &str, query: &QueryFingerprint, scanned: &ScanResult) -> Result<usize> {
    let mut total = mem::size_of::<ListQueryState>()
        .checked_add(MAX_CURSOR_BYTES)
        .and_then(|value| value.checked_add(128))
        .context("cursor_capacity: fs.list retained-query byte accounting overflowed")?;
    for bytes in [
        run_id.len(),
        query.path.capacity(),
        query.name_glob.as_ref().map_or(0, String::capacity),
        scanned
            .entries
            .capacity()
            .checked_mul(mem::size_of::<ListEntry>())
            .context("cursor_capacity: fs.list retained-entry byte accounting overflowed")?,
        scanned
            .directories
            .capacity()
            .checked_mul(mem::size_of::<DirectoryFingerprint>())
            .context("cursor_capacity: fs.list retained-directory byte accounting overflowed")?,
    ] {
        total = total
            .checked_add(bytes)
            .context("cursor_capacity: fs.list retained-query byte accounting overflowed")?;
    }
    for entry in &scanned.entries {
        total = total
            .checked_add(entry.path.capacity())
            .and_then(|value| value.checked_add(entry.basename.capacity()))
            .context("cursor_capacity: fs.list retained-entry byte accounting overflowed")?;
    }
    for directory in &scanned.directories {
        total = total
            .checked_add(directory.path.capacity())
            .context("cursor_capacity: fs.list retained-directory byte accounting overflowed")?;
    }
    Ok(total)
}

fn query_deadline(now: u64) -> Result<u64> {
    now.checked_add(QUERY_TTL_MS)
        .context("cursor_capacity: fs.list cursor expiry overflowed")
}

enum PageOutcome {
    Complete,
    PageBoundary(String),
    ScanLimit,
}

fn render_page(path: &str, entries: &[ListEntry], outcome: PageOutcome) -> Value {
    let entries = entries
        .iter()
        .map(|entry| {
            json!({
                "path": entry.path,
                "kind": entry.kind.as_str(),
            })
        })
        .collect::<Vec<_>>();
    let outcome = match outcome {
        PageOutcome::Complete => json!({ "state": "complete" }),
        PageOutcome::PageBoundary(cursor) => json!({
            "state": "truncated",
            "reason": "page_boundary",
            "next_cursor": cursor,
        }),
        PageOutcome::ScanLimit => json!({
            "state": "truncated",
            "reason": "scan_limit",
            "next_cursor": Value::Null,
        }),
    };
    json!({
        "tool": FS_LIST_TOOL_ID,
        "status": "ok",
        "path": path,
        "entry_count": entries.len(),
        "entries": entries,
        "outcome": outcome,
    })
}

fn parse_glob(pattern: &str, case: GlobCase) -> Result<Vec<GlobToken>> {
    let chars = pattern.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '*' if chars.get(index + 1) == Some(&'*') => {
                tokens.push(GlobToken::DoubleStar);
                index += 2;
            }
            '*' => {
                tokens.push(GlobToken::Star);
                index += 1;
            }
            '?' => {
                tokens.push(GlobToken::Question);
                index += 1;
            }
            '[' => {
                let (token, next) = parse_class(&chars, index + 1, case)?;
                tokens.push(token);
                index = next;
            }
            '\0'..='\u{1f}' | '\u{7f}' => {
                bail!("invalid_glob: fs.list glob contains a control character")
            }
            literal => {
                tokens.push(GlobToken::Literal(literal));
                index += 1;
            }
        }
    }
    Ok(tokens)
}

fn parse_class(chars: &[char], mut index: usize, case: GlobCase) -> Result<(GlobToken, usize)> {
    let mut negated = false;
    if matches!(chars.get(index), Some('!') | Some('^')) {
        negated = true;
        index += 1;
    }
    let mut ranges = Vec::new();
    let mut saw_item = false;
    while let Some(current) = chars.get(index).copied() {
        if current == ']' && saw_item {
            return Ok((GlobToken::Class { negated, ranges }, index + 1));
        }
        ensure!(
            current != '/',
            "invalid_glob: fs.list character class cannot contain `/`"
        );
        let end = if chars.get(index + 1) == Some(&'-') {
            let end = chars
                .get(index + 2)
                .copied()
                .context("invalid_glob: fs.list character range has no end")?;
            ensure!(
                end != ']' && end != '/',
                "invalid_glob: fs.list character range is invalid"
            );
            index += 3;
            end
        } else {
            index += 1;
            current
        };
        ensure!(
            current <= end,
            "invalid_glob: fs.list character range is reversed"
        );
        ensure!(
            fold_ascii(current, case) <= fold_ascii(end, case),
            "invalid_glob: fs.list character range reverses after ASCII case folding"
        );
        ranges.push((current, end));
        saw_item = true;
    }
    bail!("invalid_glob: fs.list character class is not closed")
}

fn glob_matches(tokens: &[GlobToken], candidate: &str, case: GlobCase) -> bool {
    let characters = candidate.chars().collect::<Vec<_>>();
    let mut current = vec![false; characters.len() + 1];
    current[0] = true;
    for token in tokens {
        let mut next = vec![false; characters.len() + 1];
        match token {
            GlobToken::Star | GlobToken::DoubleStar => {
                let crosses_separator = matches!(token, GlobToken::DoubleStar);
                next[0] = current[0];
                for index in 0..characters.len() {
                    let may_consume = crosses_separator || characters[index] != '/';
                    next[index + 1] = current[index + 1] || (may_consume && next[index]);
                }
            }
            GlobToken::Question => {
                for index in 0..characters.len() {
                    if current[index] && characters[index] != '/' {
                        next[index + 1] = true;
                    }
                }
            }
            GlobToken::Literal(expected) => {
                for index in 0..characters.len() {
                    if current[index] && chars_equal(*expected, characters[index], case) {
                        next[index + 1] = true;
                    }
                }
            }
            GlobToken::Class { negated, ranges } => {
                for index in 0..characters.len() {
                    if !current[index] || characters[index] == '/' {
                        continue;
                    }
                    let value = fold_ascii(characters[index], case);
                    let contains = ranges.iter().any(|(start, end)| {
                        let start = fold_ascii(*start, case);
                        let end = fold_ascii(*end, case);
                        start <= value && value <= end
                    });
                    if contains != *negated {
                        next[index + 1] = true;
                    }
                }
            }
        }
        current = next;
    }
    current[characters.len()]
}

fn chars_equal(left: char, right: char, case: GlobCase) -> bool {
    fold_ascii(left, case) == fold_ascii(right, case)
}

fn fold_ascii(value: char, case: GlobCase) -> char {
    match case {
        GlobCase::Sensitive => value,
        GlobCase::AsciiInsensitive => value.to_ascii_lowercase(),
    }
}

fn unix_now_ms() -> Result<u64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time predates the Unix epoch")?;
    Ok(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use crate::test_support::temp_root;

    use super::*;

    fn recursive_query() -> QueryFingerprint {
        QueryFingerprint {
            path: ".".to_string(),
            recursive: true,
            page_size: 2,
            kind: EntryKindFilter::Any,
            name_glob: None,
            match_on: GlobTarget::Basename,
            case: GlobCase::Sensitive,
        }
    }

    #[test]
    fn glob_doublestar_and_ascii_case_have_selected_semantics() {
        let pattern = parse_glob("src/**/*.RS", GlobCase::AsciiInsensitive).unwrap();
        assert!(glob_matches(
            &pattern,
            "src/deep/file.rs",
            GlobCase::AsciiInsensitive
        ));
        assert!(!glob_matches(
            &pattern,
            "other/deep/file.rs",
            GlobCase::AsciiInsensitive
        ));
        let star = parse_glob("*.rs", GlobCase::Sensitive).unwrap();
        assert!(!glob_matches(&star, "src/lib.rs", GlobCase::Sensitive));

        let question = parse_glob("?.md", GlobCase::Sensitive).unwrap();
        assert!(glob_matches(&question, "é.md", GlobCase::Sensitive));
        assert!(!glob_matches(&question, "é.md", GlobCase::Sensitive));
        let unicode_case = parse_glob("Ä.md", GlobCase::AsciiInsensitive).unwrap();
        assert!(!glob_matches(
            &unicode_case,
            "ä.md",
            GlobCase::AsciiInsensitive
        ));
    }

    #[test]
    fn invalid_character_classes_fail_closed() {
        assert!(parse_glob("[abc", GlobCase::Sensitive).is_err());
        assert!(parse_glob("[z-a]", GlobCase::Sensitive).is_err());
        assert!(parse_glob("[/]", GlobCase::Sensitive).is_err());
        assert!(parse_glob("[Z-a]", GlobCase::AsciiInsensitive).is_err());
    }

    #[test]
    fn active_query_slots_are_reserved_before_scan_and_scoped_per_run() {
        let registry = Mutex::new(ListQueryRegistry::default());
        let reservations = (0..MAX_ACTIVE_QUERIES)
            .map(|_| ListScanReservation::begin(&registry, "run-a", 1_000).unwrap())
            .collect::<Vec<_>>();
        let error = ListScanReservation::begin(&registry, "run-a", 1_000)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("cursor_capacity"));

        let other_run = ListScanReservation::begin(&registry, "run-b", 1_000).unwrap();
        drop(other_run);
        drop(reservations);
        assert!(registry.lock().unwrap().runs.is_empty());
    }

    #[test]
    fn checked_out_cursor_keeps_its_slot_and_retained_accounting() {
        let root = temp_root("list-checked-out");
        for name in ["a", "b", "c"] {
            fs::write(root.join(name), "").unwrap();
        }
        let tools = CoreTools::new(&root).unwrap();
        let first = tools
            .dispatch(FS_LIST_TOOL_ID, json!({"path": ".", "page_size": 1}))
            .unwrap();
        let cursor = first["outcome"]["next_cursor"].as_str().unwrap();
        let query = QueryFingerprint {
            page_size: 1,
            recursive: false,
            ..recursive_query()
        };
        let checked_out = CheckedOutQuery::new(
            &tools.list_queries,
            "direct",
            &query,
            cursor,
            unix_now_ms().unwrap(),
        )
        .unwrap();
        let before = {
            let registry = tools.list_queries.lock().unwrap();
            let usage = registry.runs.get("direct").unwrap();
            (
                usage.active_queries,
                usage.retained_entries,
                usage.retained_bytes,
            )
        };
        assert_eq!(before.0, 1);
        assert!(before.1 >= 3);
        assert!(before.2 > 0);

        let reservations = (1..MAX_ACTIVE_QUERIES)
            .map(|_| {
                ListScanReservation::begin(&tools.list_queries, "direct", unix_now_ms().unwrap())
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(
            ListScanReservation::begin(&tools.list_queries, "direct", unix_now_ms().unwrap())
                .is_err()
        );
        drop(reservations);
        drop(checked_out);
        assert!(tools.list_queries.lock().unwrap().runs.is_empty());
    }

    #[test]
    fn scan_limits_distinguish_exact_exhaustion_from_one_more_item() {
        let root = temp_root("list-small-limits");
        fs::write(root.join("a"), "").unwrap();
        fs::write(root.join("b"), "").unwrap();
        let tools = CoreTools::new(&root).unwrap();
        let query = recursive_query();

        let exact = scan_entries_with_limits(
            &tools,
            tools.root(),
            &query,
            None,
            ScanLimits {
                visited_directories: 1,
                examined_entries: 2,
            },
        )
        .unwrap();
        assert!(!exact.scan_limit_reached);
        assert_eq!(exact.entries.len(), 2);

        fs::write(root.join("c"), "").unwrap();
        let over = scan_entries_with_limits(
            &tools,
            tools.root(),
            &query,
            None,
            ScanLimits {
                visited_directories: 1,
                examined_entries: 2,
            },
        )
        .unwrap();
        assert!(over.scan_limit_reached);
        assert_eq!(over.entries.len(), 2);

        let directory_root = temp_root("list-small-directory-limits");
        fs::create_dir_all(directory_root.join("child")).unwrap();
        let directory_tools = CoreTools::new(&directory_root).unwrap();
        let exact_directories = scan_entries_with_limits(
            &directory_tools,
            directory_tools.root(),
            &query,
            None,
            ScanLimits {
                visited_directories: 2,
                examined_entries: 10,
            },
        )
        .unwrap();
        assert!(!exact_directories.scan_limit_reached);

        fs::create_dir_all(directory_root.join("child/grandchild")).unwrap();
        let over_directories = scan_entries_with_limits(
            &directory_tools,
            directory_tools.root(),
            &query,
            None,
            ScanLimits {
                visited_directories: 2,
                examined_entries: 10,
            },
        )
        .unwrap();
        assert!(over_directories.scan_limit_reached);
    }

    #[test]
    fn retained_byte_overflow_releases_only_the_unpublished_reservation() {
        let registry = Mutex::new(ListQueryRegistry::default());
        let reservation = ListScanReservation::begin(&registry, "run-a", 1_000).unwrap();
        let state = ListQueryState {
            run_id: "run-a".to_string(),
            query: recursive_query(),
            entries: Vec::new(),
            next_index: 0,
            directory_fingerprints: Vec::new(),
            retained_bytes: MAX_RETAINED_BYTES + 1,
            expires_at_unix_ms: 2_000,
        };
        let error = reservation.publish(state).unwrap_err();
        assert!(format!("{error:#}").contains("cursor_capacity"));
        let registry = registry.lock().unwrap();
        assert!(registry.runs.is_empty());
        assert!(registry.queries.is_empty());
    }
}
