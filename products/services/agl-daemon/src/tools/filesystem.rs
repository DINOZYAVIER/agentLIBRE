use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use agl_core::EffectId;
use agl_core::agent::{EffectReceipt, ToolFailure, ToolFailureKind};
use agl_runtime::extension::ToolContext;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::tools::{execution, invalid_result};

pub const READ: &str = "agentlibre.builtins:fs_read";
pub const APPLY_PATCH: &str = "agentlibre.builtins:fs_apply_patch";
pub const WRITE_EFFECT: &str = "agentlibre.builtins:filesystem_write";

const MAX_READ_LINES: usize = 500;
#[allow(dead_code)]
const MAX_LIST_ENTRIES: usize = 1_000;
#[allow(dead_code)]
const MAX_SEARCH_MATCHES: usize = 500;
#[allow(dead_code)]
const MAX_SEARCH_FILES: usize = 10_000;
#[allow(dead_code)]
const MAX_SEARCH_BYTES: usize = 64 * 1024 * 1024;
#[allow(dead_code)]
const MAX_MATCH_CHARS: usize = 1_000;
const MAX_PATCH_OPERATIONS: usize = 64;
const MAX_PATCH_BYTES: usize = 8 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyPatchArgs {
    operations: Vec<PatchOperation>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum PatchOperation {
    Create {
        path: String,
        content: String,
        expected_absent: bool,
    },
    Update {
        path: String,
        expected_digest: String,
        edits: Vec<PatchEdit>,
    },
    Delete {
        path: String,
        expected_digest: String,
    },
    Move {
        from: String,
        to: String,
        expected_digest: String,
        expected_absent: bool,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchEdit {
    old_text: String,
    new_text: String,
}

pub(crate) fn execute(
    id: &str,
    context: &ToolContext,
    input: Value,
    _mutation: &Arc<Mutex<()>>,
) -> Result<(Value, Vec<EffectReceipt>), ToolFailure> {
    let workspace = Workspace::new(context)?;
    let write_receipt = if id == APPLY_PATCH {
        Some(workspace.authorize_write(context)?)
    } else {
        None
    };
    let value = match id {
        READ => workspace.read(input, context.result_bytes)?,
        APPLY_PATCH => {
            let workspace_mutation = crate::tools::workspace_mutation(&workspace.root);
            let _guard = workspace_mutation.lock().map_err(|_| execution())?;
            workspace.apply_patch(input, context)?
        }
        _ => return Err(execution()),
    };
    let receipts = write_receipt.into_iter().collect();
    Ok((value, receipts))
}

struct Workspace {
    root: PathBuf,
    cwd: PathBuf,
}

impl Workspace {
    fn new(context: &ToolContext) -> Result<Self, ToolFailure> {
        let root = context
            .workspace
            .root
            .as_path()
            .canonicalize()
            .map_err(|_| execution())?;
        let cwd = root
            .join(context.workspace.working_directory.as_path())
            .canonicalize()
            .map_err(|_| execution())?;
        if !root.is_dir() || !cwd.is_dir() || !cwd.starts_with(&root) {
            return Err(execution());
        }
        Ok(Self { root, cwd })
    }

    fn authorize_write(&self, context: &ToolContext) -> Result<EffectReceipt, ToolFailure> {
        let effect = EffectId::new(WRITE_EFFECT).expect("static Effect ID");
        let grant = context
            .authority
            .0
            .iter()
            .find(|grant| grant.effect == effect)
            .ok_or_else(unauthorized)?;
        let declared_root = grant
            .scope
            .as_value()
            .as_object()
            .and_then(|scope| scope.get("root"))
            .and_then(Value::as_str)
            .map(Path::new)
            .filter(|path| path.is_absolute())
            .and_then(|path| path.canonicalize().ok())
            .ok_or_else(unauthorized)?;
        if declared_root != self.root {
            return Err(unauthorized());
        }
        Ok(EffectReceipt {
            effect,
            scope: grant.scope.clone(),
        })
    }

    fn read(&self, input: Value, result_bytes: u64) -> Result<Value, ToolFailure> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            path: String,
            cursor: usize,
            limit_lines: usize,
        }
        let args: Args = decode(input)?;
        let Some(path) = self.existing_path(&args.path)? else {
            return Ok(not_found(&args.path));
        };
        if !path.is_file() {
            return Err(invalid_input());
        }
        let bytes = fs::read(&path).map_err(|_| execution())?;
        if bytes.len() > MAX_PATCH_BYTES {
            return Err(invalid_result());
        }
        let content = std::str::from_utf8(&bytes).map_err(|_| invalid_result())?;
        let offset = args.cursor.clamp(1, usize::MAX);
        let limit = args.limit_lines.clamp(1, MAX_READ_LINES);
        let all = content.lines().collect::<Vec<_>>();
        let mut lines = all
            .iter()
            .enumerate()
            .skip(offset - 1)
            .take(limit)
            .map(|(index, line)| json!({"line":index + 1,"text":line}))
            .collect::<Vec<_>>();
        loop {
            let next = offset
                .checked_add(lines.len())
                .filter(|next| *next <= all.len());
            let value = json!({
                "path": self.display(&path), "digest": digest(&bytes),
                "start_line": offset, "end_line": offset.saturating_add(lines.len()).saturating_sub(1),
                "total_lines": all.len(), "truncated": next.is_some(),
                "next_cursor": next, "lines": lines
            });
            if value_fits(&value, result_bytes)? {
                if next.is_some() && lines.is_empty() {
                    return Err(result_too_large());
                }
                return Ok(value);
            }
            if lines.pop().is_none() {
                return Err(result_too_large());
            }
        }
    }

    #[allow(dead_code)]
    fn list(&self, input: Value, result_bytes: u64) -> Result<Value, ToolFailure> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            path: String,
            recursive: bool,
            max_entries: usize,
            cursor: usize,
        }
        let args: Args = decode(input)?;
        let Some(path) = self.existing_path(&args.path)? else {
            return Ok(not_found(&args.path));
        };
        if !path.is_dir() {
            return Err(invalid_input());
        }
        let limit = args.max_entries.clamp(1, MAX_LIST_ENTRIES);
        let cursor = args.cursor;
        let walk_limit = cursor
            .checked_add(limit)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(invalid_input)?;
        let (paths, walk_truncated) = self.walk(&path, args.recursive, walk_limit)?;
        let observed_entries = paths.len();
        let mut entries = paths
            .into_iter()
            .skip(cursor)
            .take(limit)
            .map(|path| {
                let kind = if path.is_dir() { "directory" } else { "file" };
                json!({"path":self.display(&path),"kind":kind})
            })
            .collect::<Vec<_>>();
        loop {
            let truncated =
                walk_truncated || cursor.saturating_add(entries.len()) < observed_entries;
            let next = truncated.then(|| cursor.saturating_add(entries.len()));
            let value = json!({
                "status":"ok", "path":self.display(&path), "entry_count":entries.len(),
                "truncated":truncated, "next_cursor":next, "entries":entries
            });
            if value_fits(&value, result_bytes)? {
                if truncated && entries.is_empty() {
                    return Err(result_too_large());
                }
                return Ok(value);
            }
            if entries.pop().is_none() {
                return Err(result_too_large());
            }
        }
    }

    #[allow(dead_code)]
    fn search(&self, input: Value, result_bytes: u64) -> Result<Value, ToolFailure> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            pattern: String,
            path: String,
            case_sensitive: bool,
            max_matches: usize,
            cursor: usize,
        }
        let args: Args = decode(input)?;
        if args.pattern.is_empty() || args.pattern.chars().count() > 4_096 {
            return Err(invalid_input());
        }
        let raw_path = args.path.as_str();
        let Some(path) = self.existing_path(raw_path)? else {
            return Ok(not_found(raw_path));
        };
        let limit = args.max_matches.clamp(1, MAX_SEARCH_MATCHES);
        let cursor = args.cursor;
        let files = if path.is_file() {
            vec![path.clone()]
        } else if path.is_dir() {
            self.walk(&path, true, MAX_SEARCH_FILES)?.0
        } else {
            return Err(invalid_input());
        };
        let case_sensitive = args.case_sensitive;
        let needle = (!case_sensitive).then(|| args.pattern.to_lowercase());
        let mut searched_bytes = 0usize;
        let mut matches = Vec::new();
        let mut seen_matches = 0usize;
        let mut truncated = false;
        for file in files.into_iter().filter(|path| path.is_file()) {
            let metadata = fs::metadata(&file).map_err(|_| execution())?;
            let Ok(size) = usize::try_from(metadata.len()) else {
                truncated = true;
                break;
            };
            if size > MAX_PATCH_BYTES || searched_bytes.saturating_add(size) > MAX_SEARCH_BYTES {
                truncated = true;
                continue;
            }
            searched_bytes += size;
            let bytes = fs::read(&file).map_err(|_| execution())?;
            let Ok(content) = std::str::from_utf8(&bytes) else {
                continue;
            };
            for (index, line) in content.lines().enumerate() {
                let found = if case_sensitive {
                    line.contains(&args.pattern)
                } else {
                    line.to_lowercase()
                        .contains(needle.as_deref().expect("case-folded needle"))
                };
                if found {
                    if seen_matches < cursor {
                        seen_matches += 1;
                        continue;
                    }
                    matches.push(json!({
                        "path":self.display(&file), "line":index + 1,
                        "text":line.chars().take(MAX_MATCH_CHARS).collect::<String>()
                    }));
                    if matches.len() > limit {
                        truncated = true;
                        break;
                    }
                }
            }
            if matches.len() > limit {
                break;
            }
        }
        if matches.len() > limit {
            matches.pop();
        }
        loop {
            let next = truncated.then(|| cursor.saturating_add(matches.len()));
            let value = json!({
                "status":"ok", "path":self.display(&path), "pattern":args.pattern,
                "match_count":matches.len(), "truncated":truncated,
                "next_cursor":next, "matches":matches
            });
            if value_fits(&value, result_bytes)? {
                if truncated && matches.is_empty() {
                    return Err(result_too_large());
                }
                return Ok(value);
            }
            if matches.pop().is_none() {
                return Err(result_too_large());
            }
            truncated = true;
        }
    }

    #[allow(dead_code)]
    fn walk(
        &self,
        directory: &Path,
        recursive: bool,
        limit: usize,
    ) -> Result<(Vec<PathBuf>, bool), ToolFailure> {
        let mut pending = vec![directory.to_path_buf()];
        let mut output = Vec::new();
        while let Some(current) = pending.pop() {
            let mut entries = fs::read_dir(&current)
                .map_err(|_| execution())?
                .map(|entry| entry.map_err(|_| execution()))
                .collect::<Result<Vec<_>, _>>()?;
            entries.sort_by_key(|entry| entry.file_name());
            let mut directories = Vec::new();
            for entry in entries {
                let file_type = entry.file_type().map_err(|_| execution())?;
                if file_type.is_symlink() {
                    continue;
                }
                let path = entry.path();
                if !file_type.is_file() && !file_type.is_dir() {
                    continue;
                }
                if output.len() == limit {
                    return Ok((output, true));
                }
                output.push(path.clone());
                if recursive && file_type.is_dir() {
                    directories.push(path);
                }
            }
            for path in directories.into_iter().rev() {
                pending.push(path);
            }
        }
        Ok((output, false))
    }

    fn apply_patch(&self, input: Value, context: &ToolContext) -> Result<Value, ToolFailure> {
        let args: ApplyPatchArgs = decode(input)?;
        if args.operations.is_empty() || args.operations.len() > MAX_PATCH_OPERATIONS {
            return Err(invalid_input());
        }
        let mut changes = BTreeMap::<PathBuf, Option<Vec<u8>>>::new();
        let mut original = BTreeMap::<PathBuf, Option<Vec<u8>>>::new();
        let mut original_permissions = BTreeMap::<PathBuf, fs::Permissions>::new();
        let mut output_permissions = BTreeMap::<PathBuf, fs::Permissions>::new();
        let mut total = 0usize;
        for operation in args.operations {
            match operation {
                PatchOperation::Create {
                    path,
                    content,
                    expected_absent,
                } => {
                    if !expected_absent {
                        return Err(invalid_input());
                    }
                    let path = self.absent(&path)?;
                    insert_once(&mut original, path.clone(), None)?;
                    total = total.checked_add(content.len()).ok_or_else(invalid_input)?;
                    insert_once(&mut changes, path, Some(content.into_bytes()))?;
                }
                PatchOperation::Update {
                    path,
                    expected_digest,
                    edits,
                } => {
                    let path = self.existing_file(&path)?;
                    let before = fs::read(&path).map_err(|_| execution())?;
                    let permissions = fs::metadata(&path).map_err(|_| execution())?.permissions();
                    require_digest(&before, &expected_digest)?;
                    let text = String::from_utf8(before.clone()).map_err(|_| invalid_input())?;
                    let after = apply_edits(&text, edits)?;
                    total = total.checked_add(after.len()).ok_or_else(invalid_input)?;
                    insert_once(&mut original, path.clone(), Some(before))?;
                    original_permissions.insert(path.clone(), permissions.clone());
                    output_permissions.insert(path.clone(), permissions);
                    insert_once(&mut changes, path, Some(after.into_bytes()))?;
                }
                PatchOperation::Delete {
                    path,
                    expected_digest,
                } => {
                    let path = self.existing_file(&path)?;
                    let before = fs::read(&path).map_err(|_| execution())?;
                    let permissions = fs::metadata(&path).map_err(|_| execution())?.permissions();
                    require_digest(&before, &expected_digest)?;
                    insert_once(&mut original, path.clone(), Some(before))?;
                    original_permissions.insert(path.clone(), permissions);
                    insert_once(&mut changes, path, None)?;
                }
                PatchOperation::Move {
                    from,
                    to,
                    expected_digest,
                    expected_absent,
                } => {
                    if !expected_absent {
                        return Err(invalid_input());
                    }
                    let from = self.existing_file(&from)?;
                    let to = self.absent(&to)?;
                    let before = fs::read(&from).map_err(|_| execution())?;
                    let permissions = fs::metadata(&from).map_err(|_| execution())?.permissions();
                    require_digest(&before, &expected_digest)?;
                    total = total.checked_add(before.len()).ok_or_else(invalid_input)?;
                    insert_once(&mut original, from.clone(), Some(before.clone()))?;
                    insert_once(&mut original, to.clone(), None)?;
                    original_permissions.insert(from.clone(), permissions.clone());
                    output_permissions.insert(to.clone(), permissions);
                    insert_once(&mut changes, from, None)?;
                    insert_once(&mut changes, to, Some(before))?;
                }
            }
        }
        if total > MAX_PATCH_BYTES {
            return Err(invalid_input());
        }
        self.commit(
            changes,
            original,
            original_permissions,
            output_permissions,
            context,
        )?;
        Ok(json!({"status":"committed"}))
    }

    fn commit(
        &self,
        changes: BTreeMap<PathBuf, Option<Vec<u8>>>,
        original: BTreeMap<PathBuf, Option<Vec<u8>>>,
        original_permissions: BTreeMap<PathBuf, fs::Permissions>,
        output_permissions: BTreeMap<PathBuf, fs::Permissions>,
        context: &ToolContext,
    ) -> Result<(), ToolFailure> {
        let transaction = self.root.join(format!(
            ".agentlibre-fs-transaction-{}-{}",
            context.operation.run_id, context.operation.ordinal
        ));
        if transaction.exists() {
            return Err(outcome_unknown());
        }
        let staged = transaction.join("staged");
        let backups = transaction.join("backups");
        fs::create_dir(&transaction)
            .and_then(|_| fs::create_dir(&staged))
            .and_then(|_| fs::create_dir(&backups))
            .map_err(|_| outcome_unknown())?;
        let mut created_directories = Vec::new();
        let result = (|| {
            for (index, (path, after)) in changes.iter().enumerate() {
                if let Some(bytes) = after {
                    let output = staged.join(index.to_string());
                    fs::write(&output, bytes).map_err(|_| execution())?;
                    if let Some(permissions) = output_permissions.get(path) {
                        fs::set_permissions(&output, permissions.clone())
                            .map_err(|_| execution())?;
                    }
                }
            }
            verify_originals(&original)?;
            for (index, (path, after)) in changes.iter().enumerate() {
                if let Some(parent) = path.parent() {
                    create_contained_directories(&self.root, parent, &mut created_directories)?;
                }
                if original.get(path).is_some_and(Option::is_some) {
                    fs::rename(path, backups.join(index.to_string())).map_err(|_| execution())?;
                }
                if after.is_some() {
                    fs::rename(staged.join(index.to_string()), path).map_err(|_| execution())?;
                }
            }
            Ok::<_, ToolFailure>(())
        })();
        if result.is_err() {
            let rollback = applied_rollback(&original, &original_permissions, &created_directories);
            if rollback.is_err() {
                // Keep original backups available when restoration is uncertain.
                return Err(outcome_unknown());
            }
            let cleaned = fs::remove_dir_all(&transaction);
            return if cleaned.is_ok() {
                result
            } else {
                Err(outcome_unknown())
            };
        }
        if fs::remove_dir_all(&transaction).is_err() {
            // The requested changes already committed. Cleanup failure must
            // not discard the receipt or invite the model to repeat the patch.
            tracing::warn!(operation = ?context.operation, "committed patch retained transaction cleanup data");
        }
        Ok(())
    }

    fn existing_file(&self, raw: &str) -> Result<PathBuf, ToolFailure> {
        let path = self.join(raw)?;
        reject_symlinks(&self.root, &path)?;
        let canonical = path.canonicalize().map_err(|_| invalid_input())?;
        if !canonical.starts_with(&self.root) || !canonical.is_file() {
            return Err(invalid_input());
        }
        Ok(canonical)
    }

    fn existing_path(&self, raw: &str) -> Result<Option<PathBuf>, ToolFailure> {
        let path = self.join(raw)?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| parent.starts_with(&self.root))
        {
            reject_existing_symlinks(&self.root, parent)?;
        }
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(execution()),
        }
        reject_symlinks(&self.root, &path)?;
        let canonical = path.canonicalize().map_err(|_| execution())?;
        if !canonical.starts_with(&self.root) {
            return Err(invalid_input());
        }
        Ok(Some(canonical))
    }

    fn absent(&self, raw: &str) -> Result<PathBuf, ToolFailure> {
        let path = self.join(raw)?;
        if fs::symlink_metadata(&path).is_ok() {
            return Err(invalid_input());
        }
        if let Some(parent) = path.parent() {
            reject_existing_symlinks(&self.root, parent)?;
        }
        Ok(path)
    }

    fn join(&self, raw: &str) -> Result<PathBuf, ToolFailure> {
        let path = Path::new(raw);
        if path.is_absolute()
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
        {
            return Err(invalid_input());
        }
        if path.components().next().is_some_and(|part| {
            matches!(part, Component::Normal(name) if name.to_string_lossy().starts_with(".agentlibre-fs-transaction-"))
        }) {
            return Err(invalid_input());
        }
        let joined = self.cwd.join(path);
        if !joined.starts_with(&self.root) {
            return Err(invalid_input());
        }
        Ok(joined)
    }

    fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }
}

fn decode<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, ToolFailure> {
    serde_json::from_value(value).map_err(|_| invalid_input())
}

fn value_fits(value: &Value, limit: u64) -> Result<bool, ToolFailure> {
    let text = serde_json::to_string(value).map_err(|_| invalid_result())?;
    let content = agl_core::Content::text(text).map_err(|_| invalid_result())?;
    let result = agl_core::agent::ToolResult {
        content,
        effect_receipts: Vec::new(),
    };
    let bytes = serde_json::to_vec(&result).map_err(|_| invalid_result())?;
    Ok((bytes.len() as u64) <= limit.min(agl_core::agent::MAX_TOOL_RESULT_BYTES))
}

fn invalid_input() -> ToolFailure {
    ToolFailure::no_effect(
        ToolFailureKind::InvalidInput,
        Some("input"),
        &[
            "Inspect the input schema and use workspace-relative paths. To create a new file, use op=create with path, content and expected_absent=true; no digest is required. For update/delete, read the existing target with fs_read and copy its digest into expected_digest. For move, read the existing source and copy its digest; do not move /dev/null to create a file.",
        ],
    )
}

fn result_too_large() -> ToolFailure {
    ToolFailure::no_effect(
        ToolFailureKind::ResultTooLarge,
        Some("result_bytes"),
        &["Narrow the requested path or range and reduce the result limit."],
    )
}

fn not_found(path: &str) -> Value {
    json!({"status":"error","error":{"kind":"not_found","path":path}})
}
fn outcome_unknown() -> ToolFailure {
    ToolFailure::unknown(ToolFailureKind::OutcomeUnknown)
}
fn unauthorized() -> ToolFailure {
    ToolFailure::no_effect(
        ToolFailureKind::Unauthorized,
        Some("path"),
        &["Use an admitted workspace-relative path and only granted filesystem operations."],
    )
}

fn digest(bytes: &[u8]) -> String {
    let value = Sha256::digest(bytes);
    let mut encoded = String::with_capacity("sha256:".len() + value.len() * 2);
    encoded.push_str("sha256:");
    for byte in value {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn require_digest(bytes: &[u8], expected: &str) -> Result<(), ToolFailure> {
    (digest(bytes) == expected)
        .then_some(())
        .ok_or_else(invalid_input)
}

fn insert_once<T>(
    map: &mut BTreeMap<PathBuf, T>,
    path: PathBuf,
    value: T,
) -> Result<(), ToolFailure> {
    if map.insert(path, value).is_some() {
        Err(invalid_input())
    } else {
        Ok(())
    }
}

fn apply_edits(text: &str, edits: Vec<impl Edit>) -> Result<String, ToolFailure> {
    let mut spans = Vec::new();
    for edit in edits {
        let old = edit.old();
        let mut positions = text.match_indices(old);
        let Some((start, _)) = positions.next() else {
            return Err(invalid_input());
        };
        if positions.next().is_some() {
            return Err(invalid_input());
        }
        spans.push((start, start + old.len(), edit.replacement().to_owned()));
    }
    spans.sort_by_key(|span| span.0);
    if spans.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err(invalid_input());
    }
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end, replacement) in spans {
        output.push_str(&text[cursor..start]);
        output.push_str(&replacement);
        cursor = end;
    }
    output.push_str(&text[cursor..]);
    Ok(output)
}

trait Edit {
    fn old(&self) -> &str;
    fn replacement(&self) -> &str;
}

impl Edit for PatchEdit {
    fn old(&self) -> &str {
        &self.old_text
    }
    fn replacement(&self) -> &str {
        &self.new_text
    }
}

fn verify_originals(original: &BTreeMap<PathBuf, Option<Vec<u8>>>) -> Result<(), ToolFailure> {
    for (path, expected) in original {
        match (expected, fs::read(path)) {
            (Some(expected), Ok(observed)) if &observed == expected => {}
            (None, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err(invalid_input()),
        }
    }
    Ok(())
}

fn create_contained_directories(
    root: &Path,
    directory: &Path,
    created: &mut Vec<PathBuf>,
) -> Result<(), ToolFailure> {
    let relative = directory.strip_prefix(root).map_err(|_| invalid_input())?;
    let mut cursor = root.to_path_buf();
    for part in relative.components() {
        let Component::Normal(part) = part else {
            return Err(invalid_input());
        };
        cursor.push(part);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return Err(invalid_input()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&cursor).map_err(|_| execution())?;
                created.push(cursor.clone());
            }
            Err(_) => return Err(execution()),
        }
    }
    Ok(())
}

fn applied_rollback(
    original: &BTreeMap<PathBuf, Option<Vec<u8>>>,
    original_permissions: &BTreeMap<PathBuf, fs::Permissions>,
    created_directories: &[PathBuf],
) -> Result<(), ()> {
    for (path, bytes) in original {
        match bytes {
            Some(bytes) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).map_err(|_| ())?;
                }
                fs::write(path, bytes).map_err(|_| ())?;
                if let Some(permissions) = original_permissions.get(path) {
                    fs::set_permissions(path, permissions.clone()).map_err(|_| ())?;
                }
            }
            None => match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(()),
            },
        }
    }
    for directory in created_directories.iter().rev() {
        match fs::remove_dir(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

fn reject_symlinks(root: &Path, target: &Path) -> Result<(), ToolFailure> {
    let relative = target.strip_prefix(root).map_err(|_| invalid_input())?;
    let mut cursor = root.to_path_buf();
    for part in relative.components() {
        if let Component::Normal(part) = part {
            cursor.push(part);
            if fs::symlink_metadata(&cursor)
                .map_err(|_| invalid_input())?
                .file_type()
                .is_symlink()
            {
                return Err(invalid_input());
            }
        }
    }
    Ok(())
}

fn reject_existing_symlinks(root: &Path, target: &Path) -> Result<(), ToolFailure> {
    let relative = target.strip_prefix(root).map_err(|_| invalid_input())?;
    let mut cursor = root.to_path_buf();
    for part in relative.components() {
        if let Component::Normal(part) = part {
            cursor.push(part);
            match fs::symlink_metadata(&cursor) {
                Ok(metadata) if metadata.file_type().is_symlink() => return Err(invalid_input()),
                Ok(metadata) if !metadata.is_dir() => return Err(invalid_input()),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(_) => return Err(execution()),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn invalid_input_distinguishes_create_from_existing_file_edits() {
        let failure = super::invalid_input();
        let agl_core::agent::ToolFailureEffect::None { next_actions, .. } = failure.effect else {
            panic!("invalid input must have no effect");
        };
        let hint = &next_actions[0];
        assert!(hint.contains("op=create"));
        assert!(hint.contains("expected_absent=true; no digest is required"));
        assert!(hint.contains("For update/delete, read the existing target"));
        assert!(hint.contains("For move, read the existing source"));
        assert!(!hint.contains("read each patch target"));
    }

    use super::*;
    use agl_core::AgentRunId;
    use agl_core::CanonicalJson;
    use agl_core::agent::{
        AbsolutePath, AgentOperationKey, AuthorityGrant, AuthorityGrantSet, RelativePath,
        WorkspaceScope,
    };
    use agl_runtime::extension::{ToolCancellation, ToolContext};
    use std::num::NonZeroU32;

    #[derive(Clone)]
    struct OwnedEdit {
        old: String,
        new: String,
    }
    impl Edit for OwnedEdit {
        fn old(&self) -> &str {
            &self.old
        }
        fn replacement(&self) -> &str {
            &self.new
        }
    }

    #[test]
    fn exact_edits_reject_ambiguous_or_overlapping_spans() {
        assert!(
            apply_edits(
                "x x",
                vec![OwnedEdit {
                    old: "x".into(),
                    new: "y".into()
                }]
            )
            .is_err()
        );
        assert_eq!(
            apply_edits(
                "abc",
                vec![OwnedEdit {
                    old: "b".into(),
                    new: "X".into()
                }]
            )
            .unwrap(),
            "aXc"
        );
    }

    #[test]
    fn read_is_contained_and_patch_requires_a_digest() {
        let root = std::env::temp_dir().join(format!("agl-fs-{}", AgentRunId::generate()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "before\n").unwrap();
        let context = ToolContext::new(
            AgentOperationKey {
                run_id: AgentRunId::generate(),
                ordinal: NonZeroU32::MIN,
            },
            None,
            WorkspaceScope {
                root: AbsolutePath::try_from(root.to_string_lossy().into_owned()).unwrap(),
                working_directory: RelativePath::try_from(".".to_owned()).unwrap(),
            },
            AuthorityGrantSet::default(),
            i64::MAX,
            65_536,
            ToolCancellation::new(),
        );
        let workspace = Workspace::new(&context).unwrap();
        let read = workspace
            .read(json!({"path":"a.txt","cursor":1,"limit_lines":200}), 65_536)
            .unwrap();
        assert_eq!(read["digest"], digest(b"before\n"));
        assert!(
            workspace
                .read(
                    json!({"path":"../outside","cursor":1,"limit_lines":200}),
                    65_536,
                )
                .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn only_the_current_transaction_prefix_is_reserved() {
        let root = std::env::temp_dir().join(format!("agl-fs-prefix-{}", AgentRunId::generate()));
        fs::create_dir_all(&root).unwrap();
        let workspace = Workspace::new(&context(&root, AuthorityGrantSet::default())).unwrap();
        assert!(
            workspace
                .join(".agentlibre-fs-transaction-user/file")
                .is_err()
        );
        assert!(workspace.join(".agl-fs-transaction-user/file").is_ok());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovery_is_bounded_deterministic_and_missing_paths_are_observations() {
        let root =
            std::env::temp_dir().join(format!("agl-fs-discovery-{}", AgentRunId::generate()));
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]\nresolver = \"3\"\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub struct Kernel;\n").unwrap();
        fs::write(root.join("src/nested/fsm.rs"), "pub enum KernelState {}\n").unwrap();
        let workspace = Workspace::new(&context(&root, AuthorityGrantSet::default())).unwrap();

        assert!(
            workspace
                .read(json!({"path":"Cargo.toml"}), 65_536)
                .is_err()
        );
        assert!(
            workspace
                .list(
                    json!({"path":".","recursive":false,"max_entries":10}),
                    65_536,
                )
                .is_err()
        );
        assert!(
            workspace
                .search(
                    json!({"path":"src","pattern":"kernel","case_sensitive":false,"max_matches":10}),
                    65_536,
                )
                .is_err()
        );

        let listed = workspace
            .list(
                json!({"path":".","recursive":false,"max_entries":10,"cursor":0}),
                65_536,
            )
            .unwrap();
        assert_eq!(
            listed["entries"],
            json!([
                {"path":"Cargo.toml","kind":"file"},
                {"path":"src","kind":"directory"}
            ])
        );
        let bounded = workspace
            .list(
                json!({"path":".","recursive":true,"max_entries":2,"cursor":0}),
                65_536,
            )
            .unwrap();
        assert_eq!(bounded["entry_count"], 2);
        assert_eq!(bounded["truncated"], true);
        assert_eq!(bounded["next_cursor"], 2);
        let remaining = workspace
            .list(
                json!({"path":".","recursive":true,"max_entries":10,"cursor":2}),
                65_536,
            )
            .unwrap();
        assert_eq!(remaining["truncated"], false);
        assert_eq!(remaining["next_cursor"], Value::Null);
        let byte_bounded = workspace
            .list(
                json!({"path":".","recursive":true,"max_entries":100,"cursor":0}),
                256,
            )
            .unwrap();
        assert!(value_fits(&byte_bounded, 256).unwrap());
        assert_eq!(byte_bounded["truncated"], true);

        let searched = workspace
            .search(
                json!({
                    "path":"src", "pattern":"kernel", "case_sensitive":false,
                    "max_matches":10, "cursor":0
                }),
                65_536,
            )
            .unwrap();
        assert_eq!(searched["match_count"], 2);
        assert_eq!(searched["matches"][0]["path"], "src/lib.rs");
        assert_eq!(searched["matches"][1]["path"], "src/nested/fsm.rs");
        let first_match = workspace
            .search(
                json!({
                    "path":"src", "pattern":"kernel", "case_sensitive":false,
                    "max_matches":1, "cursor":0
                }),
                65_536,
            )
            .unwrap();
        assert_eq!(first_match["truncated"], true);
        assert_eq!(first_match["next_cursor"], 1);
        let second_match = workspace
            .search(
                json!({
                    "path":"src", "pattern":"kernel", "case_sensitive":false,
                    "max_matches":1, "cursor":1
                }),
                65_536,
            )
            .unwrap();
        assert_eq!(second_match["match_count"], 1);
        assert_eq!(second_match["truncated"], false);

        for result in [
            workspace
                .read(
                    json!({"path":"absent","cursor":1,"limit_lines":200}),
                    65_536,
                )
                .unwrap(),
            workspace
                .list(
                    json!({"path":"absent","recursive":false,"max_entries":200,"cursor":0}),
                    65_536,
                )
                .unwrap(),
            workspace
                .search(
                    json!({"path":"absent","pattern":"x","case_sensitive":true,"max_matches":100,"cursor":0}),
                    65_536,
                )
                .unwrap(),
        ] {
            assert_eq!(
                result,
                json!({"status":"error","error":{"kind":"not_found","path":"absent"}})
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn patch_checks_exact_workspace_authority_before_mutation_and_emits_receipt() {
        let root = std::env::temp_dir().join(format!("agl-fs-write-{}", AgentRunId::generate()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "before\n").unwrap();
        let authorized = context(
            &root,
            AuthorityGrantSet(vec![AuthorityGrant {
                effect: EffectId::new(WRITE_EFFECT).unwrap(),
                scope: CanonicalJson::new(json!({"root":root.canonicalize().unwrap()})).unwrap(),
            }]),
        );
        let (_, receipts) = execute(
            APPLY_PATCH,
            &authorized,
            json!({"operations":[{
                "op":"update", "path":"a.txt", "expected_digest":digest(b"before\n"),
                "edits":[{"old_text":"before", "new_text":"after"}]
            }]}),
            &Arc::new(Mutex::new(())),
        )
        .unwrap();
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "after\n");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].scope, authorized.authority.0[0].scope);

        let denied = context(
            &root,
            AuthorityGrantSet(vec![AuthorityGrant {
                effect: EffectId::new(WRITE_EFFECT).unwrap(),
                scope: CanonicalJson::new(json!({"root":"/"})).unwrap(),
            }]),
        );
        let failure = execute(
            APPLY_PATCH,
            &denied,
            json!({"operations":[{
                "op":"delete", "path":"a.txt", "expected_digest":digest(b"after\n")
            }]}),
            &Arc::new(Mutex::new(())),
        )
        .unwrap_err();
        assert_eq!(failure.kind, ToolFailureKind::Unauthorized);
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "after\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn read_never_follows_workspace_symlinks() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("agl-fs-link-{}", AgentRunId::generate()));
        let outside = std::env::temp_dir().join(format!("agl-fs-out-{}", AgentRunId::generate()));
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret"), "secret").unwrap();
        symlink(&outside, root.join("link")).unwrap();
        let workspace = Workspace::new(&context(&root, AuthorityGrantSet::default())).unwrap();
        assert!(
            workspace
                .read(
                    json!({"path":"link/secret","cursor":1,"limit_lines":200}),
                    65_536,
                )
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    fn context(root: &Path, authority: AuthorityGrantSet) -> ToolContext {
        ToolContext::new(
            AgentOperationKey {
                run_id: AgentRunId::generate(),
                ordinal: NonZeroU32::MIN,
            },
            None,
            WorkspaceScope {
                root: AbsolutePath::try_from(root.to_string_lossy().into_owned()).unwrap(),
                working_directory: RelativePath::try_from(".".to_owned()).unwrap(),
            },
            authority,
            i64::MAX,
            65_536,
            ToolCancellation::new(),
        )
    }
}
