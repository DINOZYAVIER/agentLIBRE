use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read as _};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

mod list;

use list::{ListArgs, ListQueryRegistry};

use crate::parse_tool_args as parse_args;
use agl_artifact::ArtifactHandle;
use agl_kernel::{
    ArtifactAccess, ArtifactId, EffectDeclaration, EffectId, ExtensionDescriptor, ExtensionId,
    ObservedEffect, OperationKind, ToolDeclaration, ToolDispatchContext, ToolHandler,
    ToolHandlerError, ToolId, ToolResult,
};
use anyhow::{Context, Result, bail, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const EXTENSION_ID: &str = "core.workspace";
pub const FS_READ_TOOL_ID: &str = "core.workspace:fs.read";
pub const FS_LIST_TOOL_ID: &str = "core.workspace:fs.list";
pub const FS_SEARCH_TOOL_ID: &str = "core.workspace:fs.search";
pub const FS_APPLY_PATCH_TOOL_ID: &str = "core.workspace:fs.apply_patch";

const DEFAULT_READ_LINES: usize = 200;
const MAX_READ_LINES: usize = 500;
const DEFAULT_SEARCH_MATCHES: usize = 50;
const MAX_SEARCH_MATCHES: usize = 200;
const MAX_SEARCH_FILE_BYTES: u64 = 1024 * 1024;
const MAX_PATCH_OPERATIONS: usize = 64;
const MAX_PATCH_EDITS: usize = 64;
const MAX_PATCH_CONTENT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct CoreTools {
    root: PathBuf,
    list_queries: Arc<Mutex<ListQueryRegistry>>,
    mutation_lock: Arc<Mutex<()>>,
    commit_fail_after: Arc<Mutex<Option<usize>>>,
    registered_artifact_paths: Arc<BTreeSet<PathBuf>>,
    artifact_routes: Arc<BTreeMap<PathBuf, ArtifactHandle>>,
}

impl CoreTools {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().canonicalize().with_context(|| {
            format!(
                "failed to canonicalize tool root {}",
                root.as_ref().display()
            )
        })?;
        ensure!(
            root.is_dir(),
            "tool root is not a directory: {}",
            root.display()
        );
        let mutation_lock = workspace_mutation_lock(&root);
        let registered_artifact_paths = discover_registered_artifact_paths(&root)?;
        Ok(Self {
            root,
            list_queries: Arc::new(Mutex::new(ListQueryRegistry::default())),
            mutation_lock,
            commit_fail_after: Arc::new(Mutex::new(None)),
            registered_artifact_paths: Arc::new(registered_artifact_paths),
            artifact_routes: Arc::new(BTreeMap::new()),
        })
    }

    pub fn with_artifact_route(
        mut self,
        workspace_path: impl Into<PathBuf>,
        handle: ArtifactHandle,
    ) -> Result<Self> {
        let path = workspace_path.into();
        ensure!(
            path.starts_with(".agl") && path.components().count() > 1,
            "Artifact route must be a path below .agl"
        );
        ensure!(
            self.registered_artifact_paths.contains(&path),
            "Artifact route is not registered in .gitmodules: {}",
            path.display()
        );
        let mut routes = (*self.artifact_routes).clone();
        ensure!(
            routes.insert(path.clone(), handle).is_none(),
            "duplicate Artifact route: {}",
            path.display()
        );
        self.artifact_routes = Arc::new(routes);
        Ok(self)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<Value> {
        self.dispatch_for_run("direct", name, arguments)
    }

    pub(crate) fn apply_patch_for_tool(
        &self,
        arguments: Value,
    ) -> std::result::Result<Value, ToolHandlerError> {
        self.apply_patch(arguments)
            .map_err(PatchError::into_tool_error)
    }

    fn dispatch_for_run(&self, run_id: &str, name: &str, arguments: Value) -> Result<Value> {
        match name {
            FS_READ_TOOL_ID => self.read(arguments),
            FS_LIST_TOOL_ID => self.list(run_id, arguments),
            FS_SEARCH_TOOL_ID => self.search(arguments),
            FS_APPLY_PATCH_TOOL_ID => self
                .apply_patch(arguments)
                .map_err(|error| anyhow::anyhow!(error.to_string())),
            _ => bail!("unknown core tool `{name}`"),
        }
    }

    fn read(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<ReadArgs>(FS_READ_TOOL_ID, arguments)?;
        let path = self.resolve_existing_path(&args.path, PathKind::File, false)?;
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read UTF-8 file {}", path.display()))?;
        let total_lines = content.lines().count();
        let digest = content_digest(content.as_bytes());
        let start_line = args.offset_line.unwrap_or(1).max(1);
        let limit = args
            .limit_lines
            .unwrap_or(DEFAULT_READ_LINES)
            .min(MAX_READ_LINES);
        let mut lines = Vec::new();
        for (index, line) in content.lines().enumerate().skip(start_line - 1).take(limit) {
            lines.push(json!({
                "line": index + 1,
                "text": line,
            }));
        }
        let end_line = start_line.saturating_add(lines.len()).saturating_sub(1);
        let truncated = end_line < total_lines;

        Ok(json!({
            "tool": FS_READ_TOOL_ID,
            "status": "ok",
            "path": self.display_path(&path),
            "start_line": start_line,
            "end_line": end_line,
            "total_lines": total_lines,
            "truncated": truncated,
            "digest": digest,
            "lines": lines,
        }))
    }

    fn list(&self, run_id: &str, arguments: Value) -> Result<Value> {
        let args = parse_args::<ListArgs>(FS_LIST_TOOL_ID, arguments)?;
        list::list_page(self, run_id, args)
    }

    fn search(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<SearchArgs>(FS_SEARCH_TOOL_ID, arguments)?;
        ensure!(
            !args.pattern.trim().is_empty(),
            "core.workspace:fs.search pattern cannot be blank"
        );
        let search_path = args.path.unwrap_or_else(|| ".".to_string());
        let path = self.resolve_existing_path(&search_path, PathKind::Directory, true)?;
        let max_matches = args
            .max_matches
            .unwrap_or(DEFAULT_SEARCH_MATCHES)
            .min(MAX_SEARCH_MATCHES);
        let case_sensitive = args.case_sensitive.unwrap_or(true);
        let needle = if case_sensitive {
            args.pattern.clone()
        } else {
            args.pattern.to_ascii_lowercase()
        };
        let mut matches = Vec::new();
        self.collect_matches(&path, &needle, case_sensitive, max_matches, &mut matches)?;
        let truncated = matches.len() >= max_matches;

        Ok(json!({
            "tool": FS_SEARCH_TOOL_ID,
            "status": "ok",
            "path": self.display_path(&path),
            "pattern": args.pattern,
            "match_count": matches.len(),
            "truncated": truncated,
            "matches": matches,
        }))
    }

    fn apply_patch(&self, arguments: Value) -> std::result::Result<Value, PatchError> {
        let args = serde_json::from_value::<ApplyPatchArgs>(arguments)
            .map_err(|error| PatchError::invalid(format!("invalid patch arguments: {error}")))?;
        if args.operations.is_empty() || args.operations.len() > MAX_PATCH_OPERATIONS {
            return Err(PatchError::invalid(format!(
                "operations must contain between 1 and {MAX_PATCH_OPERATIONS} items"
            )));
        }
        let _guard = self
            .mutation_lock
            .lock()
            .map_err(|error| PatchError::terminal(format!("mutation lock is poisoned: {error}")))?;
        let plan = self.plan_patch(args)?;
        self.commit_patch(plan)
    }

    fn plan_patch(&self, args: ApplyPatchArgs) -> std::result::Result<PatchPlan, PatchError> {
        let mut changes = Vec::new();
        let mut receipts = Vec::new();
        let mut touched = std::collections::BTreeSet::new();
        let mut content_bytes = 0usize;

        for operation in args.operations {
            match operation {
                PatchOperation::Create {
                    path,
                    content,
                    expected_absent,
                } => {
                    if !expected_absent {
                        return Err(PatchError::invalid("create requires expected_absent=true"));
                    }
                    let relative = self.resolve_absent_target(&path)?;
                    ensure_unique_path(&mut touched, &relative)?;
                    content_bytes = checked_patch_bytes(content_bytes, content.len())?;
                    changes.push(PlannedFileChange {
                        relative: relative.clone(),
                        before: None,
                        after: Some(content.into_bytes()),
                        after_permissions: None,
                    });
                    receipts.push(PatchReceipt {
                        operation: "create",
                        path: Some(display_relative(&relative)),
                        from: None,
                        to: None,
                        before_digest: None,
                        after_digest: changes
                            .last()
                            .and_then(|change| change.after.as_deref())
                            .map(content_digest),
                    });
                }
                PatchOperation::Update {
                    path,
                    expected_digest,
                    edits,
                } => {
                    let (relative, snapshot) =
                        self.read_preconditioned_file(&path, &expected_digest)?;
                    let ExistingFile {
                        content: before,
                        permissions,
                    } = snapshot;
                    ensure_unique_path(&mut touched, &relative)?;
                    let after = apply_exact_text_edits(&path, &before, edits)?;
                    content_bytes = checked_patch_bytes(content_bytes, after.len())?;
                    receipts.push(PatchReceipt {
                        operation: "update",
                        path: Some(display_relative(&relative)),
                        from: None,
                        to: None,
                        before_digest: Some(content_digest(&before)),
                        after_digest: Some(content_digest(&after)),
                    });
                    changes.push(PlannedFileChange {
                        relative,
                        before: Some(before),
                        after: Some(after),
                        after_permissions: Some(permissions),
                    });
                }
                PatchOperation::Delete {
                    path,
                    expected_digest,
                } => {
                    let (relative, snapshot) =
                        self.read_preconditioned_file(&path, &expected_digest)?;
                    let before = snapshot.content;
                    ensure_unique_path(&mut touched, &relative)?;
                    receipts.push(PatchReceipt {
                        operation: "delete",
                        path: Some(display_relative(&relative)),
                        from: None,
                        to: None,
                        before_digest: Some(content_digest(&before)),
                        after_digest: None,
                    });
                    changes.push(PlannedFileChange {
                        relative,
                        before: Some(before),
                        after: None,
                        after_permissions: None,
                    });
                }
                PatchOperation::Move {
                    from,
                    to,
                    expected_digest,
                    expected_destination_absent,
                } => {
                    if !expected_destination_absent {
                        return Err(PatchError::invalid(
                            "move requires expected_destination_absent=true",
                        ));
                    }
                    let (source, snapshot) =
                        self.read_preconditioned_file(&from, &expected_digest)?;
                    let ExistingFile {
                        content: before,
                        permissions,
                    } = snapshot;
                    let destination = self.resolve_absent_target(&to)?;
                    ensure_unique_path(&mut touched, &source)?;
                    ensure_unique_path(&mut touched, &destination)?;
                    content_bytes = checked_patch_bytes(content_bytes, before.len())?;
                    receipts.push(PatchReceipt {
                        operation: "move",
                        path: None,
                        from: Some(display_relative(&source)),
                        to: Some(display_relative(&destination)),
                        before_digest: Some(content_digest(&before)),
                        after_digest: Some(content_digest(&before)),
                    });
                    changes.push(PlannedFileChange {
                        relative: source,
                        before: Some(before.clone()),
                        after: None,
                        after_permissions: None,
                    });
                    changes.push(PlannedFileChange {
                        relative: destination,
                        before: None,
                        after: Some(before),
                        after_permissions: Some(permissions),
                    });
                }
            }
        }

        Ok(PatchPlan { changes, receipts })
    }

    fn read_preconditioned_file(
        &self,
        raw: &str,
        expected_digest: &str,
    ) -> std::result::Result<(PathBuf, ExistingFile), PatchError> {
        let relative = normalize_repo_path(raw, false)
            .map_err(|error| PatchError::invalid(error.to_string()))?;
        self.enforce_artifact_write_access(&relative)
            .map_err(|error| PatchError::invalid(format!("{error:#}")))?;
        let path = self
            .resolve_existing_path(raw, PathKind::File, false)
            .map_err(|error| {
                if self.root.join(&relative).exists() {
                    PatchError::invalid(error.to_string())
                } else {
                    PatchError::not_found(raw)
                }
            })?;
        let mut file = fs::File::open(&path)
            .map_err(|error| PatchError::terminal(format!("failed to open `{raw}`: {error}")))?;
        let permissions = file
            .metadata()
            .map_err(|error| {
                PatchError::terminal(format!("failed to inspect `{raw}` permissions: {error}"))
            })?
            .permissions();
        let mut content = Vec::new();
        file.read_to_end(&mut content)
            .map_err(|error| PatchError::terminal(format!("failed to read `{raw}`: {error}")))?;
        let actual = content_digest(&content);
        if actual != expected_digest {
            return Err(PatchError::conflict(raw, expected_digest, &actual));
        }
        Ok((
            relative,
            ExistingFile {
                content,
                permissions,
            },
        ))
    }

    fn resolve_absent_target(&self, raw: &str) -> std::result::Result<PathBuf, PatchError> {
        let relative = normalize_repo_path(raw, false)
            .map_err(|error| PatchError::invalid(error.to_string()))?;
        self.enforce_artifact_write_access(&relative)
            .map_err(|error| PatchError::invalid(format!("{error:#}")))?;
        let target = self.root.join(&relative);
        match fs::symlink_metadata(&target) {
            Ok(_) => return Err(PatchError::conflict_absence(raw)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(PatchError::terminal(format!(
                    "failed to inspect `{raw}`: {error}"
                )));
            }
        }
        let parent = target
            .parent()
            .ok_or_else(|| PatchError::invalid(format!("path has no parent: {raw}")))?;
        self.validate_destination_parent(parent, raw)?;
        Ok(relative)
    }

    fn validate_destination_parent(
        &self,
        parent: &Path,
        raw: &str,
    ) -> std::result::Result<(), PatchError> {
        let relative = parent
            .strip_prefix(&self.root)
            .map_err(|_| PatchError::invalid(format!("path escapes workspace: {raw}")))?;
        let mut cursor = self.root.clone();
        let mut missing = false;
        for component in relative.components() {
            if let Component::Normal(segment) = component {
                cursor.push(segment);
                if missing {
                    continue;
                }
                match fs::symlink_metadata(&cursor) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return Err(PatchError::invalid(format!(
                            "repository path cannot traverse symlink: {raw}"
                        )));
                    }
                    Ok(metadata) if !metadata.is_dir() => {
                        return Err(PatchError::invalid(format!(
                            "destination parent is not a directory: {raw}"
                        )));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => missing = true,
                    Err(error) => {
                        return Err(PatchError::terminal(format!(
                            "failed to inspect destination parent for `{raw}`: {error}"
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    fn commit_patch(&self, plan: PatchPlan) -> std::result::Result<Value, PatchError> {
        let transaction_root = self.transaction_root()?;
        let staged_root = transaction_root.join("staged");
        let backup_root = transaction_root.join("backup");
        fs::create_dir(&transaction_root)
            .and_then(|()| fs::create_dir(&staged_root))
            .and_then(|()| fs::create_dir(&backup_root))
            .map_err(|error| {
                PatchError::terminal(format!("failed to prepare patch transaction: {error}"))
            })?;

        let result = (|| {
            for (index, change) in plan.changes.iter().enumerate() {
                if let Some(after) = &change.after {
                    let staged = staged_root.join(index.to_string());
                    fs::write(&staged, after).map_err(|error| {
                        PatchError::terminal(format!(
                            "failed to stage `{}`: {error}",
                            display_relative(&change.relative)
                        ))
                    })?;
                    if let Some(permissions) = &change.after_permissions {
                        fs::set_permissions(&staged, permissions.clone()).map_err(|error| {
                            PatchError::terminal(format!(
                                "failed to preserve permissions for staged `{}`: {error}",
                                display_relative(&change.relative)
                            ))
                        })?;
                    }
                }
            }

            let mut applied = Vec::new();
            let mut created_directories = Vec::new();
            for (index, change) in plan.changes.iter().enumerate() {
                let target = self.root.join(&change.relative);
                let backup = backup_root.join(index.to_string());
                if change.after.is_some()
                    && let Err(error) = create_missing_parent_directories(
                        &self.root,
                        target
                            .parent()
                            .expect("planned repository file always has a parent"),
                        &mut created_directories,
                    )
                {
                    rollback_changes(
                        &self.root,
                        &backup_root,
                        &applied,
                        &created_directories,
                    )
                    .map_err(|rollback_error| {
                        PatchError::outcome_unknown(format!(
                            "failed to create parent for `{}`: {error}; rollback failed: {rollback_error}",
                            display_relative(&change.relative)
                        ))
                    })?;
                    return Err(PatchError::terminal(format!(
                        "failed to create parent for `{}`: {error}",
                        display_relative(&change.relative)
                    )));
                }
                if change.before.is_some()
                    && let Err(error) = fs::rename(&target, &backup)
                {
                    rollback_changes(
                        &self.root,
                        &backup_root,
                        &applied,
                        &created_directories,
                    )
                    .map_err(|rollback_error| {
                        PatchError::outcome_unknown(format!(
                            "failed to prepare `{}` for commit: {error}; rollback failed: {rollback_error}",
                            display_relative(&change.relative)
                        ))
                    })?;
                    return Err(PatchError::terminal(format!(
                        "failed to prepare `{}` for commit: {error}",
                        display_relative(&change.relative)
                    )));
                }
                if change.after.is_some()
                    && let Err(error) = fs::rename(staged_root.join(index.to_string()), &target)
                {
                    let rollback =
                        rollback_changes(&self.root, &backup_root, &applied, &created_directories);
                    if let Some(rollback_error) = rollback.err() {
                        return Err(PatchError::outcome_unknown(format!(
                            "commit failed for `{}`: {error}; rollback failed: {rollback_error}",
                            display_relative(&change.relative)
                        )));
                    }
                    if change.before.is_some() {
                        fs::rename(&backup, &target).map_err(|rollback_error| {
                            PatchError::outcome_unknown(format!(
                                "commit failed for `{}`: {error}; current rollback failed: {rollback_error}",
                                display_relative(&change.relative)
                            ))
                        })?;
                    }
                    return Err(PatchError::terminal(format!(
                        "patch commit failed for `{}`: {error}",
                        display_relative(&change.relative)
                    )));
                }
                applied.push((index, change));
                if self.inject_commit_failure(applied.len()) {
                    rollback_changes(&self.root, &backup_root, &applied, &created_directories)
                        .map_err(|error| {
                            PatchError::outcome_unknown(format!(
                                "injected commit failure rollback failed: {error}"
                            ))
                        })?;
                    return Err(PatchError::terminal("injected patch commit failure"));
                }
            }
            Ok(())
        })();

        let cleanup = fs::remove_dir_all(&transaction_root);
        result?;
        cleanup.map_err(|error| {
            PatchError::outcome_unknown(format!(
                "patch committed but transaction cleanup failed: {error}"
            ))
        })?;

        Ok(json!({
            "tool": FS_APPLY_PATCH_TOOL_ID,
            "status": "committed",
            "change_count": plan.receipts.len(),
            "changes": plan.receipts,
        }))
    }

    fn transaction_root(&self) -> std::result::Result<PathBuf, PatchError> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| PatchError::terminal(format!("system clock error: {error}")))?
            .as_nanos();
        Ok(self.root.join(format!(
            ".agl-fs-transaction-{}-{nonce}",
            std::process::id()
        )))
    }

    fn inject_commit_failure(&self, applied: usize) -> bool {
        self.commit_fail_after
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .is_some_and(|limit| applied >= limit)
    }

    #[cfg(test)]
    fn fail_commit_after(&self, changes: Option<usize>) {
        *self.commit_fail_after.lock().unwrap() = changes;
    }

    fn resolve_existing_path(
        &self,
        raw: &str,
        kind: PathKind,
        allow_root: bool,
    ) -> Result<PathBuf> {
        let relative = normalize_repo_path(raw, allow_root)?;
        self.enforce_artifact_access(&relative, ArtifactAccess::ReadTree)?;
        let joined = self.root.join(relative);
        self.reject_symlink_components(&joined, raw)?;
        let canonical = joined
            .canonicalize()
            .with_context(|| format!("failed to canonicalize repository path `{raw}`"))?;
        ensure!(
            canonical.starts_with(&self.root),
            "repository path escapes tool root: {raw}"
        );
        match kind {
            PathKind::File => ensure!(canonical.is_file(), "repository path is not a file: {raw}"),
            PathKind::Directory => {
                ensure!(
                    canonical.is_dir(),
                    "repository path is not a directory: {raw}"
                )
            }
        }
        Ok(canonical)
    }

    fn reject_symlink_components(&self, path: &Path, raw: &str) -> Result<()> {
        let relative = path.strip_prefix(&self.root).with_context(|| {
            format!("repository path is outside tool root before canonicalization: {raw}")
        })?;
        let mut cursor = self.root.clone();
        for component in relative.components() {
            if let Component::Normal(segment) = component {
                cursor.push(segment);
                let metadata = fs::symlink_metadata(&cursor)
                    .with_context(|| format!("failed to inspect repository path `{raw}`"))?;
                ensure!(
                    !metadata.file_type().is_symlink(),
                    "repository path cannot traverse symlink: {raw}"
                );
            }
        }
        Ok(())
    }

    fn collect_matches(
        &self,
        path: &Path,
        needle: &str,
        case_sensitive: bool,
        max_matches: usize,
        matches: &mut Vec<Value>,
    ) -> Result<()> {
        if matches.len() >= max_matches {
            return Ok(());
        }
        for entry in sorted_dir_entries(path)? {
            if matches.len() >= max_matches {
                break;
            }
            let file_type = entry
                .file_type()
                .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
            if file_type.is_symlink() || entry.file_name() == ".git" {
                continue;
            }
            if file_type.is_dir() {
                let entry_path = entry.path();
                let relative = entry_path.strip_prefix(&self.root).with_context(|| {
                    format!("search path escaped workspace: {}", entry_path.display())
                })?;
                self.enforce_artifact_access(relative, ArtifactAccess::ReadTree)?;
                self.collect_matches(&entry_path, needle, case_sensitive, max_matches, matches)?;
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            let metadata = entry
                .metadata()
                .with_context(|| format!("failed to read metadata {}", entry.path().display()))?;
            if metadata.len() > MAX_SEARCH_FILE_BYTES {
                continue;
            }
            let Ok(content) = fs::read_to_string(entry.path()) else {
                continue;
            };
            for (line_index, line) in content.lines().enumerate() {
                let haystack = if case_sensitive {
                    line.to_string()
                } else {
                    line.to_ascii_lowercase()
                };
                if haystack.contains(needle) {
                    matches.push(json!({
                        "path": self.display_path(&entry.path()),
                        "line": line_index + 1,
                        "text": line,
                    }));
                    if matches.len() >= max_matches {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn display_path(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .ok()
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| ".".to_string())
    }

    fn enforce_artifact_write_access(&self, relative: &Path) -> Result<()> {
        self.enforce_artifact_access(relative, ArtifactAccess::MutateTree)
    }

    pub(crate) fn enforce_artifact_access(
        &self,
        relative: &Path,
        access: ArtifactAccess,
    ) -> Result<()> {
        let protected = self
            .registered_artifact_paths
            .iter()
            .filter(|prefix| relative.starts_with(prefix))
            .max_by_key(|prefix| prefix.components().count());
        let Some(prefix) = protected else {
            return Ok(());
        };
        let handle = self.artifact_routes.get(prefix).with_context(|| {
            format!(
                "path `{}` belongs to registered Artifact `{}` but no admitted ArtifactHandle is bound",
                relative.display(),
                prefix.display()
            )
        })?;
        handle
            .require_access(access)
            .map_err(|error| anyhow::anyhow!(error.to_string()))
    }
}

fn discover_registered_artifact_paths(root: &Path) -> Result<BTreeSet<PathBuf>> {
    if !root.join(".gitmodules").is_file() {
        return Ok(BTreeSet::new());
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "config",
            "-f",
            ".gitmodules",
            "--get-regexp",
            "^submodule\\..*\\.path$",
        ])
        .output()
        .context("failed to inspect .gitmodules Artifact registrations")?;
    ensure!(
        output.status.success(),
        "invalid .gitmodules Artifact registrations: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let stdout = String::from_utf8(output.stdout)
        .context(".gitmodules Artifact registrations are not UTF-8")?;
    let mut artifact_ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    for line in stdout.lines() {
        let (key, raw_path) = line
            .split_once(char::is_whitespace)
            .context("malformed .gitmodules Artifact registration")?;
        let name = key
            .strip_prefix("submodule.")
            .and_then(|key| key.strip_suffix(".path"))
            .context("malformed .gitmodules Artifact registration key")?;
        let Ok(artifact_id) = ArtifactId::new(name) else {
            // Ordinary Git submodules are not Artifacts. Only an exact
            // owner-qualified ArtifactId opts a submodule into this boundary.
            continue;
        };
        ensure!(
            artifact_ids.insert(artifact_id),
            "duplicate Artifact registration: {name}"
        );
        let raw_path = raw_path.trim();
        let path = normalize_repo_path(raw_path, false)?;
        ensure!(
            path == Path::new(raw_path)
                && path.starts_with(".agl")
                && path.components().count() > 1,
            "Artifact submodule path must be an explicit relative path below .agl: {raw_path}"
        );
        ensure!(
            paths.insert(path),
            "duplicate Artifact submodule path: {raw_path}"
        );
    }
    Ok(paths)
}

fn workspace_mutation_lock(root: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<std::collections::BTreeMap<PathBuf, Weak<Mutex<()>>>>> =
        OnceLock::new();
    let locks = LOCKS.get_or_init(|| Mutex::new(std::collections::BTreeMap::new()));
    let mut locks = locks
        .lock()
        .expect("workspace mutation lock registry is not poisoned");
    if let Some(lock) = locks.get(root).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(root.to_path_buf(), Arc::downgrade(&lock));
    lock
}

impl ToolHandler for CoreTools {
    fn dispatch(&self, context: ToolDispatchContext) -> agl_kernel::ToolHandlerFuture<'_> {
        Box::pin(async move {
            let invocation = context.into_invocation();
            let run_id = invocation.scope.run_id().as_str().to_string();
            let tool_id = invocation.tool_id.as_str().to_string();
            let is_patch = tool_id == FS_APPLY_PATCH_TOOL_ID;
            let data = if is_patch {
                self.apply_patch(invocation.arguments)
                    .map_err(PatchError::into_tool_error)?
            } else {
                let arguments = invocation.arguments;
                self.dispatch_for_run(&run_id, &tool_id, arguments.clone())
                    .map_err(|error| classify_read_only_error(&tool_id, &arguments, error))?
            };
            let observed = if is_patch {
                data["changes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|change| {
                        ["path", "from", "to"]
                            .into_iter()
                            .filter_map(|field| change[field].as_str())
                    })
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .map(|path| {
                        ObservedEffect::new(
                            EffectId::repo_files(),
                            [
                                ("transaction".to_owned(), "atomic".to_owned()),
                                ("path".to_owned(), path.to_owned()),
                            ],
                        )
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            Ok(ToolResult::new(data).with_observed_effects(observed))
        })
    }
}

fn classify_read_only_error(
    tool_id: &str,
    arguments: &Value,
    error: anyhow::Error,
) -> ToolHandlerError {
    let is_not_found = error.chain().any(|source| {
        source
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
    });
    if is_not_found && let Some(path) = requested_read_only_path(tool_id, arguments) {
        return ToolHandlerError::new(
            "not_found",
            format!("repository path was not found: {path}"),
            json!({"path": path}),
        );
    }
    error.into()
}

fn requested_read_only_path<'a>(tool_id: &str, arguments: &'a Value) -> Option<&'a str> {
    match tool_id {
        FS_READ_TOOL_ID | FS_LIST_TOOL_ID => arguments.get("path")?.as_str(),
        FS_SEARCH_TOOL_ID => arguments.get("path").and_then(Value::as_str).or(Some(".")),
        _ => None,
    }
}

pub fn declaration() -> ExtensionDescriptor {
    let descriptor = ExtensionDescriptor::builtin(
        ExtensionId::new(EXTENSION_ID).expect("core tool extension id is valid"),
        "Core Tools",
        "1.2.0",
    )
    .expect("core tool declaration is valid")
    .with_tool(read_only_action::<ReadArgs, ReadOutput>(
        FS_READ_TOOL_ID,
        "Read a UTF-8 file from the repository with line bounds.",
    ))
    .with_tool(read_only_action::<ListArgs, ListOutput>(
        FS_LIST_TOOL_ID,
        "List repository directory entries.",
    ))
    .with_tool(read_only_action::<SearchArgs, SearchOutput>(
        FS_SEARCH_TOOL_ID,
        "Search repository text files for a literal pattern.",
    ))
    .with_tool(
        action::<ApplyPatchArgs, ApplyPatchOutput>(
            FS_APPLY_PATCH_TOOL_ID,
            "Atomically mutate repository files. Operation objects use `op` as the discriminator; create requires `expected_absent=true`, update applies exact `old_text`/`new_text` edits, and update/delete/move require the complete digest returned by `fs.read`, including its `sha256:` prefix. Use at most one operation per path and group all updates to that path in one `edits` array.",
            OperationKind::Write,
        )
        .with_errors([
            agl_kernel::ToolErrorDeclaration::recoverable("invalid_patch")
                .with_data_schema::<InvalidPatchErrorData>(),
            agl_kernel::ToolErrorDeclaration::recoverable("not_found")
                .with_data_schema::<PathNotFoundErrorData>(),
            agl_kernel::ToolErrorDeclaration::recoverable("conflict")
                .with_data_schema::<PatchConflictErrorData>(),
            agl_kernel::ToolErrorDeclaration::terminal("execution_failed")
                .with_data_schema::<EmptyToolErrorData>(),
            agl_kernel::ToolErrorDeclaration::terminal("outcome_unknown")
                .with_data_schema::<EmptyToolErrorData>(),
        ])
        .expect("filesystem patch error declarations are valid")
        .with_state_effects([EffectId::repo_files()]),
    );
    crate::with_observation_workflow(
        descriptor.with_effect(EffectDeclaration::for_standard(EffectId::repo_files()).unwrap()),
    )
}

fn action<I: JsonSchema, O: JsonSchema>(
    id: &str,
    description: &str,
    operation_kind: OperationKind,
) -> ToolDeclaration {
    ToolDeclaration::from_schema::<I>(
        ToolId::new(id).expect("core tool id is valid"),
        description,
        operation_kind,
    )
    .expect("core tool declaration input schema is valid")
    .with_output_schema::<O>()
    .expect("core tool declaration output schema is valid")
}

fn read_only_action<I: JsonSchema, O: JsonSchema>(id: &str, description: &str) -> ToolDeclaration {
    action::<I, O>(id, description, OperationKind::Read)
        .with_errors([
            agl_kernel::ToolErrorDeclaration::recoverable("not_found")
                .with_data_schema::<PathNotFoundErrorData>(),
            agl_kernel::ToolErrorDeclaration::terminal("execution_failed")
                .with_data_schema::<EmptyToolErrorData>(),
        ])
        .expect("read-only filesystem Tool error declarations are valid")
}

fn sorted_dir_entries(path: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("failed to read directory {}", path.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read directory entry in {}", path.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

fn ensure_unique_path(
    touched: &mut std::collections::BTreeSet<PathBuf>,
    path: &Path,
) -> std::result::Result<(), PatchError> {
    if touched.insert(path.to_path_buf()) {
        Ok(())
    } else {
        Err(PatchError::invalid(format!(
            "patch touches `{}` more than once",
            display_relative(path)
        )))
    }
}

fn checked_patch_bytes(
    current: usize,
    additional: usize,
) -> std::result::Result<usize, PatchError> {
    let total = current
        .checked_add(additional)
        .ok_or_else(|| PatchError::invalid("patch content byte count overflowed"))?;
    if total > MAX_PATCH_CONTENT_BYTES {
        return Err(PatchError::invalid(format!(
            "patch content exceeds {MAX_PATCH_CONTENT_BYTES} bytes"
        )));
    }
    Ok(total)
}

fn apply_exact_text_edits(
    path: &str,
    before: &[u8],
    edits: Vec<PatchEdit>,
) -> std::result::Result<Vec<u8>, PatchError> {
    if edits.is_empty() || edits.len() > MAX_PATCH_EDITS {
        return Err(PatchError::invalid_at(
            path,
            None,
            format!("edits must contain between 1 and {MAX_PATCH_EDITS} items"),
        ));
    }
    let source = std::str::from_utf8(before)
        .map_err(|_| PatchError::invalid_at(path, None, "update target is not valid UTF-8"))?;
    let edit_count = edits.len();
    let mut resolved = Vec::with_capacity(edit_count);

    for (edit_index, edit) in edits.into_iter().enumerate() {
        let (start, end) = if edit.old_text.is_empty() {
            if source.is_empty() && edit_count == 1 {
                (0, 0)
            } else {
                return Err(PatchError::invalid_at(
                    path,
                    Some(edit_index),
                    "empty old_text requires an empty file and exactly one edit",
                ));
            }
        } else {
            let needle = edit.old_text.as_bytes();
            let mut matches = source
                .as_bytes()
                .windows(needle.len())
                .enumerate()
                .filter_map(|(offset, candidate)| (candidate == needle).then_some(offset));
            let Some(start) = matches.next() else {
                return Err(PatchError::invalid_at(
                    path,
                    Some(edit_index),
                    "old_text did not match the original file",
                ));
            };
            if matches.next().is_some() {
                return Err(PatchError::invalid_at(
                    path,
                    Some(edit_index),
                    "old_text matched the original file more than once",
                ));
            }
            (start, start + needle.len())
        };
        resolved.push(ResolvedPatchEdit {
            edit_index,
            start,
            end,
            new_text: edit.new_text.into_bytes(),
        });
    }

    resolved.sort_by_key(|edit| (edit.start, edit.end));
    for pair in resolved.windows(2) {
        if pair[1].start < pair[0].end {
            return Err(PatchError::invalid_at(
                path,
                Some(pair[1].edit_index),
                format!("edit overlaps edit {}", pair[0].edit_index),
            ));
        }
    }

    let mut after = before.to_vec();
    for edit in resolved.into_iter().rev() {
        after.splice(edit.start..edit.end, edit.new_text);
    }
    Ok(after)
}

fn content_digest(content: &[u8]) -> String {
    let digest = Sha256::digest(content);
    let mut encoded = String::with_capacity(7 + digest.len() * 2);
    encoded.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn display_relative(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn rollback_changes(
    root: &Path,
    backup_root: &Path,
    applied: &[(usize, &PlannedFileChange)],
    created_directories: &[PathBuf],
) -> io::Result<()> {
    for (index, change) in applied.iter().rev() {
        let target = root.join(&change.relative);
        if change.after.is_some() {
            match fs::remove_file(&target) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        if change.before.is_some() {
            fs::rename(backup_root.join(index.to_string()), target)?;
        }
    }
    for directory in created_directories.iter().rev() {
        match fs::remove_dir(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn create_missing_parent_directories(
    root: &Path,
    parent: &Path,
    created: &mut Vec<PathBuf>,
) -> io::Result<()> {
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| io::Error::other("destination parent escaped workspace"))?;
    let mut cursor = root.to_path_buf();
    for component in relative.components() {
        if let Component::Normal(segment) = component {
            cursor.push(segment);
            match fs::create_dir(&cursor) {
                Ok(()) => created.push(cursor.clone()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    if !fs::symlink_metadata(&cursor)?.is_dir() {
                        return Err(io::Error::other(
                            "destination parent component is not a directory",
                        ));
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

fn normalize_repo_path(raw: &str, allow_root: bool) -> Result<PathBuf> {
    ensure!(!raw.trim().is_empty(), "repository path cannot be blank");
    ensure!(!raw.contains('\0'), "repository path contains NUL");
    ensure!(
        !raw.contains('\\'),
        "repository path must use forward slashes"
    );

    let path = Path::new(raw);
    ensure!(!path.is_absolute(), "repository path cannot be absolute");

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => {
                ensure!(segment != ".git", "repository path cannot enter .git");
                ensure!(
                    !segment
                        .to_string_lossy()
                        .starts_with(".agl-fs-transaction-"),
                    "repository path cannot enter the Tool transaction namespace"
                );
                normalized.push(segment);
            }
            Component::CurDir => {}
            Component::ParentDir => bail!("repository path cannot contain parent traversal"),
            Component::RootDir | Component::Prefix(_) => {
                bail!("repository path cannot be absolute")
            }
        }
    }
    ensure!(
        allow_root || !normalized.as_os_str().is_empty(),
        "repository path must name a file or subdirectory"
    );
    Ok(normalized)
}

#[derive(Clone, Copy, Debug)]
enum PathKind {
    File,
    Directory,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    offset_line: Option<usize>,
    limit_lines: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    pattern: String,
    path: Option<String>,
    max_matches: Option<usize>,
    case_sensitive: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ApplyPatchArgs {
    /// Atomic create, update, delete, and move operations. Each object uses
    /// `op` as its discriminator and includes its required precondition. Use
    /// at most one operation per path and group same-file replacements in one
    /// update `edits` array.
    #[schemars(length(min = 1, max = 64))]
    operations: Vec<PatchOperation>,
}

/// One filesystem mutation with an explicit concurrency precondition.
#[derive(Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum PatchOperation {
    Create {
        path: String,
        content: String,
        /// Must be explicitly true so create conflicts with an existing path.
        expected_absent: bool,
    },
    Update {
        path: String,
        /// Complete opaque digest returned by `fs.read`. Copy the `sha256:`
        /// prefix and hexadecimal value without normalization.
        expected_digest: String,
        /// Exact UTF-8 spans resolved against the digest-pinned original file.
        #[schemars(length(min = 1, max = 64))]
        edits: Vec<PatchEdit>,
    },
    Delete {
        path: String,
        /// Complete opaque digest returned by `fs.read`. Copy the `sha256:`
        /// prefix and hexadecimal value without normalization.
        expected_digest: String,
    },
    Move {
        from: String,
        to: String,
        /// Complete opaque source digest returned by `fs.read`. Copy the
        /// `sha256:` prefix and hexadecimal value without normalization.
        expected_digest: String,
        /// Must be explicitly true so move conflicts with an existing target.
        expected_destination_absent: bool,
    },
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct PatchEdit {
    /// Exact original text. It must identify one unique source span.
    old_text: String,
    /// UTF-8 replacement text. Use an empty string to delete the selected span.
    new_text: String,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct ReadLine {
    line: usize,
    text: String,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct ReadOutput {
    tool: String,
    status: String,
    path: String,
    start_line: usize,
    end_line: usize,
    total_lines: usize,
    truncated: bool,
    digest: String,
    lines: Vec<ReadLine>,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct ListEntryOutput {
    path: String,
    kind: String,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct ListOutcome {
    state: String,
    reason: Option<String>,
    next_cursor: Option<String>,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct ListOutput {
    tool: String,
    status: String,
    path: String,
    entry_count: usize,
    entries: Vec<ListEntryOutput>,
    outcome: ListOutcome,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct SearchMatchOutput {
    path: String,
    line: usize,
    text: String,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct SearchOutput {
    tool: String,
    status: String,
    path: String,
    pattern: String,
    match_count: usize,
    truncated: bool,
    matches: Vec<SearchMatchOutput>,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct ApplyPatchOutput {
    tool: String,
    status: String,
    change_count: usize,
    changes: Vec<PatchReceipt>,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct EmptyToolErrorData {}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct InvalidPatchErrorData {
    path: Option<String>,
    edit_index: Option<usize>,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct PathNotFoundErrorData {
    path: String,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct PatchConflictErrorData {
    path: String,
    expected_digest: Option<String>,
    actual_digest: Option<String>,
    expected_absent: Option<bool>,
}

struct PatchPlan {
    changes: Vec<PlannedFileChange>,
    receipts: Vec<PatchReceipt>,
}

struct PlannedFileChange {
    relative: PathBuf,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
    after_permissions: Option<fs::Permissions>,
}

struct ExistingFile {
    content: Vec<u8>,
    permissions: fs::Permissions,
}

struct ResolvedPatchEdit {
    edit_index: usize,
    start: usize,
    end: usize,
    new_text: Vec<u8>,
}

#[derive(Clone, Debug, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct PatchReceipt {
    operation: &'static str,
    path: Option<String>,
    from: Option<String>,
    to: Option<String>,
    before_digest: Option<String>,
    after_digest: Option<String>,
}

#[derive(Debug)]
struct PatchError {
    code: &'static str,
    message: String,
    data: Value,
}

impl PatchError {
    fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_patch", message, json!({}))
    }

    fn invalid_at(path: &str, edit_index: Option<usize>, message: impl Into<String>) -> Self {
        let data = match edit_index {
            Some(edit_index) => json!({"path": path, "edit_index": edit_index}),
            None => json!({"path": path}),
        };
        Self::new("invalid_patch", message, data)
    }

    fn not_found(path: &str) -> Self {
        Self::new(
            "not_found",
            format!("repository file or parent was not found: {path}"),
            json!({"path": path}),
        )
    }

    fn conflict(path: &str, expected: &str, actual: &str) -> Self {
        Self::new(
            "conflict",
            format!("content digest changed for `{path}`"),
            json!({"path": path, "expected_digest": expected, "actual_digest": actual}),
        )
    }

    fn conflict_absence(path: &str) -> Self {
        Self::new(
            "conflict",
            format!("expected destination to be absent: {path}"),
            json!({"path": path, "expected_absent": true}),
        )
    }

    fn terminal(message: impl Into<String>) -> Self {
        Self::new("execution_failed", message, json!({}))
    }

    fn outcome_unknown(message: impl Into<String>) -> Self {
        Self::new("outcome_unknown", message, json!({}))
    }

    fn new(code: &'static str, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
            data,
        }
    }

    fn into_tool_error(self) -> ToolHandlerError {
        ToolHandlerError::new(self.code, self.message, self.data)
    }
}

impl std::fmt::Display for PatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for PatchError {}

#[cfg(test)]
mod tests {
    use agl_ids::{ExecutionScope, RunId, StepId};
    use agl_kernel::{
        ExtensionRegistration, ToolBinding, ToolDispatchControl, ToolErrorClass, ToolInvocation,
    };
    use agl_kernel::{ToolAccessMode, ToolOutcomeStatus, ToolPolicyInput};
    use serde_json::json;

    use crate::test_support::temp_root;

    use super::*;

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;

        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn declaration_registers_core_filesystem_tools() {
        let declaration = declaration();
        declaration.validate().unwrap();
        let read = declaration
            .tool(&ToolId::new(FS_READ_TOOL_ID).unwrap())
            .unwrap();
        assert_eq!(
            read.description,
            "Read a UTF-8 file from the repository with line bounds."
        );
        assert_eq!(read.input_schema["additionalProperties"], json!(false));
        let schema = read.compile_schema().unwrap();
        assert!(schema.validate(&json!({"path": "README.MD"})).is_ok());
        assert!(schema.validate(&json!({})).is_err());
        assert!(
            schema
                .validate(&json!({"path": "README.MD", "extra": true}))
                .is_err()
        );
        assert!(schema.validate(&json!({"path": 42})).is_err());
        assert!(
            declaration
                .tool(&ToolId::new(FS_APPLY_PATCH_TOOL_ID).unwrap())
                .is_some()
        );
        assert_eq!(
            declaration
                .tool(&ToolId::new(FS_APPLY_PATCH_TOOL_ID).unwrap())
                .unwrap()
                .declared_error("invalid_patch")
                .unwrap()
                .class,
            ToolErrorClass::Recoverable
        );
        for id in [FS_READ_TOOL_ID, FS_LIST_TOOL_ID, FS_SEARCH_TOOL_ID] {
            let tool = declaration.tool(&ToolId::new(id).unwrap()).unwrap();
            assert_eq!(
                tool.declared_error("not_found").unwrap().class,
                ToolErrorClass::Recoverable
            );
        }
    }

    #[test]
    fn read_only_missing_paths_are_recoverable_tool_outcomes() {
        let root = temp_root("read-only-not-found");
        let tools = CoreTools::new(&root).unwrap();
        let descriptor = declaration();
        let bindings = descriptor
            .tools
            .iter()
            .map(|tool| ToolBinding::new(tool.id.clone(), tools.clone()))
            .collect::<Vec<_>>();
        let mut runtime = agl_kernel::ToolRuntime::new();
        runtime
            .register_extension(ExtensionRegistration::new(descriptor.clone(), bindings))
            .unwrap();
        let cases = [
            (
                FS_READ_TOOL_ID,
                json!({"path": "missing.txt"}),
                "missing.txt",
            ),
            (
                FS_LIST_TOOL_ID,
                json!({"path": "missing-directory"}),
                "missing-directory",
            ),
            (
                FS_SEARCH_TOOL_ID,
                json!({"path": "missing-directory", "pattern": "needle"}),
                "missing-directory",
            ),
        ];
        let effective = ToolPolicyInput::new(
            [descriptor.clone()],
            cases.iter().map(|(id, _, _)| ToolId::new(*id).unwrap()),
            ToolAccessMode::ReadOnly,
        )
        .resolve()
        .unwrap();

        for (id, arguments, expected_path) in cases {
            let tool_id = ToolId::new(id).unwrap();
            let declaration = descriptor.tool(&tool_id).unwrap();
            let invocation = ToolInvocation::new(
                ExecutionScope::builder(RunId::generate())
                    .step_id(StepId::generate())
                    .build()
                    .unwrap(),
                tool_id,
                descriptor.id.clone(),
                declaration.digest(),
                effective.policy_hash().clone(),
                arguments,
            );

            let outcome = runtime
                .dispatch(invocation, &effective, ToolDispatchControl::uncancellable())
                .unwrap();

            assert_eq!(outcome.status, ToolOutcomeStatus::RecoverableError);
            assert_eq!(outcome.outcome_code, "not_found");
            assert_eq!(outcome.error.unwrap().data, json!({"path": expected_path}));
        }
    }

    #[test]
    fn invalid_exact_edit_is_one_recoverable_observation_without_mutation() {
        let root = temp_root("apply-patch-recoverable-edit");
        let path = root.join("file.txt");
        fs::write(&path, "before\n").unwrap();
        let tools = CoreTools::new(&root).unwrap();
        let descriptor = declaration();
        let bindings = descriptor
            .tools
            .iter()
            .map(|tool| ToolBinding::new(tool.id.clone(), tools.clone()))
            .collect::<Vec<_>>();
        let mut runtime = agl_kernel::ToolRuntime::new();
        runtime
            .register_extension(ExtensionRegistration::new(descriptor.clone(), bindings))
            .unwrap();
        let tool_id = ToolId::new(FS_APPLY_PATCH_TOOL_ID).unwrap();
        let effective = ToolPolicyInput::new(
            [descriptor.clone()],
            [tool_id.clone()],
            ToolAccessMode::Write,
        )
        .resolve()
        .unwrap();
        let declaration = descriptor.tool(&tool_id).unwrap();
        let invocation = ToolInvocation::new(
            ExecutionScope::builder(RunId::generate())
                .step_id(StepId::generate())
                .build()
                .unwrap(),
            tool_id,
            descriptor.id.clone(),
            declaration.digest(),
            effective.policy_hash().clone(),
            json!({"operations": [{
                "op": "update",
                "path": "file.txt",
                "expected_digest": content_digest(b"before\n"),
                "edits": [{"old_text": "missing", "new_text": "after"}]
            }]}),
        );

        let outcome = runtime
            .dispatch(invocation, &effective, ToolDispatchControl::uncancellable())
            .unwrap();

        assert_eq!(outcome.status, ToolOutcomeStatus::RecoverableError);
        assert_eq!(outcome.outcome_code, "invalid_patch");
        assert_eq!(
            outcome.error.unwrap().data,
            json!({"path": "file.txt", "edit_index": 0})
        );
        assert_eq!(fs::read_to_string(path).unwrap(), "before\n");
    }

    #[test]
    fn apply_patch_schema_explains_the_live_create_shape() {
        let declaration = declaration();
        let patch = declaration
            .tool(&ToolId::new(FS_APPLY_PATCH_TOOL_ID).unwrap())
            .unwrap();
        assert!(patch.description.contains("use `op` as the discriminator"));
        assert!(patch.description.contains("`expected_absent=true`"));
        assert!(patch.description.contains("including its `sha256:` prefix"));
        assert!(patch.description.contains("at most one operation per path"));
        let encoded_schema = patch.input_schema.to_string();
        assert!(encoded_schema.contains("explicit concurrency precondition"));
        assert!(encoded_schema.contains("Must be explicitly true"));
        assert!(encoded_schema.contains("prefix and hexadecimal value"));
        assert!(encoded_schema.contains("one operation per path"));

        let schema = patch.compile_schema().unwrap();
        let error = schema
            .validate(&json!({
                "operations": [{
                    "action": "create",
                    "content": "",
                    "path": "test.file"
                }]
            }))
            .unwrap_err()
            .to_string();
        assert!(error.contains("'action' was unexpected"));
        assert!(error.contains("\"op\" is a required property"));
        assert!(error.contains("\"expected_absent\" is a required property"));
        assert!(!error.contains("not valid under any of the schemas"));

        assert!(
            schema
                .validate(&json!({
                    "operations": [{
                        "op": "create",
                        "path": "test.file",
                        "content": "",
                        "expected_absent": true
                    }]
                }))
                .is_ok()
        );
        assert!(
            schema
                .validate(&json!({
                    "operations": [{
                        "op": "update",
                        "path": "test.file",
                        "expected_digest": "sha256:example",
                        "edits": [{
                            "old_text": "before",
                            "new_text": "after"
                        }]
                    }]
                }))
                .is_ok()
        );
        assert!(
            schema
                .validate(&json!({
                    "operations": [{
                        "op": "update",
                        "path": "test.file",
                        "expected_digest": "sha256:example",
                        "content": "obsolete"
                    }]
                }))
                .is_err()
        );
        assert!(encoded_schema.contains("old_text"));
        assert!(encoded_schema.contains("new_text"));
        assert!(encoded_schema.contains("\"edits\""));
    }

    #[test]
    fn read_rejects_parent_traversal() {
        let root = temp_root("read-parent");
        let tools = CoreTools::new(&root).unwrap();

        let err = tools
            .dispatch(FS_READ_TOOL_ID, json!({"path": "../secret.txt"}))
            .unwrap_err();

        assert!(format!("{err:#}").contains("parent traversal"));
    }

    #[test]
    fn list_skips_git_directory() {
        let root = temp_root("list");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "secret").unwrap();
        fs::write(root.join("README.MD"), "hello").unwrap();
        let tools = CoreTools::new(&root).unwrap();

        let output = tools
            .dispatch(FS_LIST_TOOL_ID, json!({"path": "."}))
            .unwrap();

        assert_eq!(output["tool"], FS_LIST_TOOL_ID);
        assert_eq!(output["entry_count"], 1);
        assert_eq!(output["entries"][0]["path"], "README.MD");
        assert_eq!(output["entries"][0]["kind"], "file");
        assert_eq!(output["outcome"]["state"], "complete");
        assert!(output.get("truncated").is_none());
    }

    #[test]
    fn list_uses_deterministic_consumable_pagination() {
        let root = temp_root("list-pages");
        for name in ["e.txt", "c.txt", "a.txt", "d.txt", "b.txt"] {
            fs::write(root.join(name), name).unwrap();
        }
        let tools = CoreTools::new(&root).unwrap();

        let first = tools
            .dispatch(FS_LIST_TOOL_ID, json!({"path": ".", "page_size": 2}))
            .unwrap();
        assert_eq!(first["entries"][0]["path"], "a.txt");
        assert_eq!(first["entries"][1]["path"], "b.txt");
        assert_eq!(first["outcome"]["state"], "truncated");
        assert_eq!(first["outcome"]["reason"], "page_boundary");
        let cursor = first["outcome"]["next_cursor"].as_str().unwrap();

        let second = tools
            .dispatch(
                FS_LIST_TOOL_ID,
                json!({"path": ".", "page_size": 2, "cursor": cursor}),
            )
            .unwrap();
        assert_eq!(second["entries"][0]["path"], "c.txt");
        assert_eq!(second["entries"][1]["path"], "d.txt");
        assert_eq!(second["outcome"]["state"], "truncated");
        let next_cursor = second["outcome"]["next_cursor"].as_str().unwrap();
        assert!(
            tools
                .dispatch(
                    FS_LIST_TOOL_ID,
                    json!({"path": ".", "page_size": 2, "cursor": cursor}),
                )
                .unwrap_err()
                .to_string()
                .contains("cursor_stale")
        );

        let third = tools
            .dispatch(
                FS_LIST_TOOL_ID,
                json!({"path": ".", "page_size": 2, "cursor": next_cursor}),
            )
            .unwrap();
        assert_eq!(third["entries"][0]["path"], "e.txt");
        assert_eq!(third["outcome"]["state"], "complete");
    }

    #[test]
    fn list_filters_kind_glob_target_and_ascii_case_without_pruning_traversal() {
        let root = temp_root("list-filter");
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        fs::write(root.join("src/deep/MOD.RS"), "").unwrap();
        fs::write(root.join("src/deep/readme.md"), "").unwrap();
        let tools = CoreTools::new(&root).unwrap();

        let output = tools
            .dispatch(
                FS_LIST_TOOL_ID,
                json!({
                    "path": ".",
                    "recursive": true,
                    "page_size": 10,
                    "kind": "file",
                    "name_glob": "src/**/*.rs",
                    "match_on": "relative_path",
                    "case": "ascii_insensitive"
                }),
            )
            .unwrap();
        let paths = output["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(paths, ["src/deep/MOD.RS"]);
        assert_eq!(output["outcome"]["state"], "complete");
    }

    #[test]
    fn list_cursor_binds_every_query_field_and_directory_identity() {
        let root = temp_root("list-stale");
        fs::write(root.join("a"), "").unwrap();
        fs::write(root.join("b"), "").unwrap();
        let tools = CoreTools::new(&root).unwrap();
        let first = tools
            .dispatch(FS_LIST_TOOL_ID, json!({"path": ".", "page_size": 1}))
            .unwrap();
        let cursor = first["outcome"]["next_cursor"]
            .as_str()
            .unwrap()
            .to_string();
        let mismatch = tools
            .dispatch(
                FS_LIST_TOOL_ID,
                json!({
                    "path": ".",
                    "page_size": 1,
                    "cursor": cursor,
                    "kind": "file"
                }),
            )
            .unwrap_err();
        assert!(format!("{mismatch:#}").contains("cursor_query_mismatch"));
        let continued = tools
            .dispatch(
                FS_LIST_TOOL_ID,
                json!({"path": ".", "page_size": 1, "cursor": cursor}),
            )
            .unwrap();
        assert_eq!(continued["entries"][0]["path"], "b");
        assert_eq!(continued["outcome"]["state"], "complete");

        let first = tools
            .dispatch(FS_LIST_TOOL_ID, json!({"path": ".", "page_size": 1}))
            .unwrap();
        let cursor = first["outcome"]["next_cursor"].as_str().unwrap();
        fs::write(root.join("c"), "").unwrap();
        let stale = tools
            .dispatch(
                FS_LIST_TOOL_ID,
                json!({"path": ".", "page_size": 1, "cursor": cursor}),
            )
            .unwrap_err();
        assert!(format!("{stale:#}").contains("cursor_stale"));
    }

    #[test]
    fn list_wrong_run_cannot_consume_a_cursor_and_file_content_changes_are_allowed() {
        let root = temp_root("list-run-binding");
        fs::write(root.join("a"), "before").unwrap();
        fs::write(root.join("b"), "before").unwrap();
        let tools = CoreTools::new(&root).unwrap();
        let first = tools
            .dispatch_for_run(
                "run-a",
                FS_LIST_TOOL_ID,
                json!({"path": ".", "page_size": 1}),
            )
            .unwrap();
        let cursor = first["outcome"]["next_cursor"].as_str().unwrap();

        let wrong_run = tools
            .dispatch_for_run(
                "run-b",
                FS_LIST_TOOL_ID,
                json!({"path": ".", "page_size": 1, "cursor": cursor}),
            )
            .unwrap_err();
        assert!(format!("{wrong_run:#}").contains("cursor_stale"));

        fs::write(
            root.join("a"),
            "content changed without changing the listing",
        )
        .unwrap();
        let continued = tools
            .dispatch_for_run(
                "run-a",
                FS_LIST_TOOL_ID,
                json!({"path": ".", "page_size": 1, "cursor": cursor}),
            )
            .unwrap();
        assert_eq!(continued["entries"][0]["path"], "b");
        assert_eq!(continued["outcome"]["state"], "complete");
    }

    #[test]
    fn list_deleted_query_root_reports_cursor_stale() {
        let root = temp_root("list-deleted-root");
        fs::create_dir_all(root.join("listed")).unwrap();
        fs::write(root.join("listed/a"), "").unwrap();
        fs::write(root.join("listed/b"), "").unwrap();
        let tools = CoreTools::new(&root).unwrap();
        let first = tools
            .dispatch(FS_LIST_TOOL_ID, json!({"path": "listed", "page_size": 1}))
            .unwrap();
        let cursor = first["outcome"]["next_cursor"].as_str().unwrap();
        fs::remove_dir_all(root.join("listed")).unwrap();

        let error = tools
            .dispatch(
                FS_LIST_TOOL_ID,
                json!({"path": "listed", "page_size": 1, "cursor": cursor}),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("cursor_stale"));
    }

    #[test]
    fn list_rejects_obsolete_shape_and_a_fifth_active_query() {
        let root = temp_root("list-capacity");
        let tools = CoreTools::new(&root).unwrap();
        assert!(
            tools
                .dispatch(FS_LIST_TOOL_ID, json!({"path": ".", "max_entries": 20}),)
                .is_err()
        );
        for index in 0..5 {
            let directory = root.join(format!("d{index}"));
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("a"), "").unwrap();
            fs::write(directory.join("b"), "").unwrap();
        }
        for index in 0..4 {
            let output = tools
                .dispatch(
                    FS_LIST_TOOL_ID,
                    json!({"path": format!("d{index}"), "page_size": 1}),
                )
                .unwrap();
            assert_eq!(output["outcome"]["state"], "truncated");
        }
        let error = tools
            .dispatch(FS_LIST_TOOL_ID, json!({"path": "d4", "page_size": 1}))
            .unwrap_err();
        assert!(format!("{error:#}").contains("cursor_capacity"));
    }

    #[cfg(unix)]
    #[test]
    fn list_non_utf8_name_cannot_produce_a_complete_claim() {
        use std::os::unix::ffi::OsStringExt as _;

        let root = temp_root("list-non-utf8");
        let name = std::ffi::OsString::from_vec(vec![b'f', 0xff]);
        fs::write(root.join(name), "").unwrap();
        let tools = CoreTools::new(&root).unwrap();

        let error = tools
            .dispatch(FS_LIST_TOOL_ID, json!({"path": "."}))
            .unwrap_err();
        assert!(format!("{error:#}").contains("non_utf8_path"));
    }

    #[test]
    fn search_returns_bounded_literal_matches() {
        let root = temp_root("search");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "alpha\nbeta\nalpha\n").unwrap();
        let tools = CoreTools::new(&root).unwrap();

        let output = tools
            .dispatch(
                FS_SEARCH_TOOL_ID,
                json!({"path": ".", "pattern": "alpha", "max_matches": 1}),
            )
            .unwrap();

        assert_eq!(output["match_count"], 1);
        assert_eq!(output["truncated"], true);
        assert_eq!(output["matches"][0]["path"], "src/lib.rs");
        assert_eq!(output["matches"][0]["line"], 1);
        assert_eq!(output["matches"][0]["text"], "alpha");
    }

    #[test]
    fn apply_patch_updates_with_digest_precondition() {
        let root = temp_root("apply-patch-update");
        let path = root.join("README.MD");
        fs::write(&path, "hello old\n").unwrap();
        let tools = CoreTools::new(&root).unwrap();

        let output = tools
            .dispatch(
                FS_APPLY_PATCH_TOOL_ID,
                json!({"operations": [{
                    "op": "update",
                    "path": "README.MD",
                    "expected_digest": content_digest(b"hello old\n"),
                    "edits": [{"old_text": "hello old", "new_text": "hello new"}]
                }]}),
            )
            .unwrap();

        assert_eq!(output["status"], "committed");
        assert_eq!(output["changes"][0]["path"], "README.MD");
        assert_eq!(fs::read_to_string(path).unwrap(), "hello new\n");
    }

    #[test]
    fn apply_patch_updates_large_file_after_one_bounded_read() {
        let root = temp_root("apply-patch-large-paged");
        let path = root.join("large.txt");
        let before = (1..=600)
            .map(|line| format!("line {line} original\n"))
            .collect::<String>();
        fs::write(&path, &before).unwrap();
        let tools = CoreTools::new(&root).unwrap();

        let page = tools
            .dispatch(
                FS_READ_TOOL_ID,
                json!({"path": "large.txt", "offset_line": 540, "limit_lines": 20}),
            )
            .unwrap();
        assert_eq!(page["start_line"], 540);
        assert_eq!(page["end_line"], 559);
        assert_eq!(page["total_lines"], 600);
        assert_eq!(page["truncated"], true);

        tools
            .dispatch(
                FS_APPLY_PATCH_TOOL_ID,
                json!({"operations": [{
                    "op": "update",
                    "path": "large.txt",
                    "expected_digest": page["digest"],
                    "edits": [{
                        "old_text": "line 550 original",
                        "new_text": "line 550 changed"
                    }]
                }]}),
            )
            .unwrap();

        let after = fs::read_to_string(path).unwrap();
        assert!(after.contains("line 549 original\nline 550 changed\nline 551 original"));
        assert_eq!(after.lines().count(), 600);
    }

    #[test]
    fn apply_patch_applies_multiple_exact_utf8_edits_against_one_snapshot() {
        let root = temp_root("apply-patch-multiple-edits");
        let path = root.join("unicode.txt");
        let before = "alpha\nпривет мир\nomega\n";
        fs::write(&path, before).unwrap();
        let tools = CoreTools::new(&root).unwrap();

        tools
            .dispatch(
                FS_APPLY_PATCH_TOOL_ID,
                json!({"operations": [{
                    "op": "update",
                    "path": "unicode.txt",
                    "expected_digest": content_digest(before.as_bytes()),
                    "edits": [
                        {"old_text": "alpha", "new_text": "first"},
                        {"old_text": "привет", "new_text": "здравствуй"},
                        {"old_text": "omega", "new_text": ""}
                    ]
                }]}),
            )
            .unwrap();

        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "first\nздравствуй мир\n\n"
        );
    }

    #[test]
    fn apply_patch_rejects_missing_ambiguous_and_overlapping_edits_without_changes() {
        let cases = [
            (
                "missing",
                "alpha beta gamma\n",
                json!([{"old_text": "absent", "new_text": "changed"}]),
                json!({"path": "file.txt", "edit_index": 0}),
            ),
            (
                "ambiguous",
                "same same\n",
                json!([{"old_text": "same", "new_text": "changed"}]),
                json!({"path": "file.txt", "edit_index": 0}),
            ),
            (
                "overlap",
                "alpha beta gamma\n",
                json!([
                    {"old_text": "alpha beta", "new_text": "first"},
                    {"old_text": "beta gamma", "new_text": "second"}
                ]),
                json!({"path": "file.txt", "edit_index": 1}),
            ),
        ];

        for (name, before, edits, expected_data) in cases {
            let root = temp_root(&format!("apply-patch-{name}"));
            let path = root.join("file.txt");
            fs::write(&path, before).unwrap();
            let tools = CoreTools::new(&root).unwrap();
            let error = tools
                .apply_patch(json!({"operations": [{
                    "op": "update",
                    "path": "file.txt",
                    "expected_digest": content_digest(before.as_bytes()),
                    "edits": edits
                }]}))
                .unwrap_err();

            assert_eq!(error.code, "invalid_patch");
            assert_eq!(error.data, expected_data);
            assert_eq!(fs::read_to_string(path).unwrap(), before);
        }
    }

    #[test]
    fn apply_patch_rejects_invalid_utf8_and_excessive_edits_without_changes() {
        let invalid_root = temp_root("apply-patch-invalid-utf8");
        let invalid_path = invalid_root.join("file.bin");
        let invalid_before = [0xff, 0xfe];
        fs::write(&invalid_path, invalid_before).unwrap();
        let invalid_tools = CoreTools::new(&invalid_root).unwrap();
        let invalid_error = invalid_tools
            .apply_patch(json!({"operations": [{
                "op": "update",
                "path": "file.bin",
                "expected_digest": content_digest(&invalid_before),
                "edits": [{"old_text": "x", "new_text": "y"}]
            }]}))
            .unwrap_err();
        assert_eq!(invalid_error.code, "invalid_patch");
        assert_eq!(invalid_error.data, json!({"path": "file.bin"}));
        assert_eq!(fs::read(invalid_path).unwrap(), invalid_before);

        let excessive_root = temp_root("apply-patch-excessive-edits");
        let excessive_path = excessive_root.join("file.txt");
        fs::write(&excessive_path, "before\n").unwrap();
        let excessive_tools = CoreTools::new(&excessive_root).unwrap();
        let edits = (0..=MAX_PATCH_EDITS)
            .map(|_| json!({"old_text": "before", "new_text": "after"}))
            .collect::<Vec<_>>();
        let excessive_error = excessive_tools
            .apply_patch(json!({"operations": [{
                "op": "update",
                "path": "file.txt",
                "expected_digest": content_digest(b"before\n"),
                "edits": edits
            }]}))
            .unwrap_err();
        assert_eq!(excessive_error.code, "invalid_patch");
        assert_eq!(excessive_error.data, json!({"path": "file.txt"}));
        assert_eq!(fs::read_to_string(excessive_path).unwrap(), "before\n");
    }

    #[test]
    fn apply_patch_rejects_legacy_full_file_update_and_staged_byte_overflow() {
        let root = temp_root("apply-patch-clean-cutover");
        let path = root.join("file.txt");
        fs::write(&path, "before\n").unwrap();
        let tools = CoreTools::new(&root).unwrap();

        let legacy = tools
            .apply_patch(json!({"operations": [{
                "op": "update",
                "path": "file.txt",
                "expected_digest": content_digest(b"before\n"),
                "content": "after\n"
            }]}))
            .unwrap_err();
        assert_eq!(legacy.code, "invalid_patch");
        assert!(legacy.message.contains("unknown field `content`"));

        let oversized = "x".repeat(MAX_PATCH_CONTENT_BYTES + 1);
        let overflow = tools
            .apply_patch(json!({"operations": [{
                "op": "update",
                "path": "file.txt",
                "expected_digest": content_digest(b"before\n"),
                "edits": [{"old_text": "before", "new_text": oversized}]
            }]}))
            .unwrap_err();
        assert_eq!(overflow.code, "invalid_patch");
        assert!(overflow.message.contains("patch content exceeds"));
        assert_eq!(fs::read_to_string(path).unwrap(), "before\n");
    }

    #[cfg(unix)]
    #[test]
    fn apply_patch_preserves_permissions_when_updating_files() {
        let root = temp_root("apply-patch-update-permissions");
        let executable = root.join("executable.sh");
        let restricted = root.join("restricted.txt");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        fs::write(&restricted, "restricted old\n").unwrap();
        set_mode(&executable, 0o755);
        set_mode(&restricted, 0o640);
        let tools = CoreTools::new(&root).unwrap();

        tools
            .dispatch(
                FS_APPLY_PATCH_TOOL_ID,
                json!({"operations": [
                    {
                        "op": "update",
                        "path": "executable.sh",
                        "expected_digest": content_digest(b"#!/bin/sh\nexit 0\n"),
                        "edits": [{"old_text": "exit 0", "new_text": "exit 1"}]
                    },
                    {
                        "op": "update",
                        "path": "restricted.txt",
                        "expected_digest": content_digest(b"restricted old\n"),
                        "edits": [{"old_text": "restricted old", "new_text": "restricted new"}]
                    }
                ]}),
            )
            .unwrap();

        assert_eq!(mode(&executable), 0o755);
        assert_eq!(mode(&restricted), 0o640);
    }

    #[cfg(unix)]
    #[test]
    fn apply_patch_preserves_permissions_when_moving_a_file() {
        let root = temp_root("apply-patch-move-permissions");
        let source = root.join("source.sh");
        let destination = root.join("destination.sh");
        fs::write(&source, "#!/bin/sh\nexit 0\n").unwrap();
        set_mode(&source, 0o751);
        let tools = CoreTools::new(&root).unwrap();

        tools
            .dispatch(
                FS_APPLY_PATCH_TOOL_ID,
                json!({"operations": [{
                    "op": "move",
                    "from": "source.sh",
                    "to": "destination.sh",
                    "expected_digest": content_digest(b"#!/bin/sh\nexit 0\n"),
                    "expected_destination_absent": true
                }]}),
            )
            .unwrap();

        assert!(!source.exists());
        assert_eq!(mode(&destination), 0o751);
    }

    #[test]
    fn apply_patch_atomically_creates_missing_parent_directories() {
        let root = temp_root("apply-patch-new-parents");
        let tools = CoreTools::new(&root).unwrap();

        let output = tools
            .dispatch(
                FS_APPLY_PATCH_TOOL_ID,
                json!({"operations": [{
                    "op": "create",
                    "path": "new/nested/file.txt",
                    "content": "created\n",
                    "expected_absent": true
                }]}),
            )
            .unwrap();

        assert_eq!(output["status"], "committed");
        assert_eq!(
            fs::read_to_string(root.join("new/nested/file.txt")).unwrap(),
            "created\n"
        );
    }

    #[test]
    fn apply_patch_rejects_stale_digest_without_changes() {
        let root = temp_root("apply-patch-conflict");
        fs::write(root.join("README.MD"), "same\nsame\n").unwrap();
        let tools = CoreTools::new(&root).unwrap();

        let err = tools
            .dispatch(
                FS_APPLY_PATCH_TOOL_ID,
                json!({"operations": [{
                    "op": "update",
                    "path": "README.MD",
                    "expected_digest": content_digest(b"stale\n"),
                    "edits": [{"old_text": "same", "new_text": "changed"}]
                }]}),
            )
            .unwrap_err();

        assert!(format!("{err:#}").contains("conflict"));
        assert_eq!(
            fs::read_to_string(root.join("README.MD")).unwrap(),
            "same\nsame\n"
        );
    }

    #[test]
    fn apply_patch_commits_create_delete_and_move_together() {
        let root = temp_root("apply-patch-mixed");
        fs::write(root.join("update.txt"), "update old\n").unwrap();
        fs::write(root.join("delete.txt"), "delete me\n").unwrap();
        fs::write(root.join("move.txt"), "move me\n").unwrap();
        let tools = CoreTools::new(&root).unwrap();

        let output = tools
            .dispatch(
                FS_APPLY_PATCH_TOOL_ID,
                json!({"operations": [
                    {
                        "op": "create",
                        "path": "created.txt",
                        "content": "created\n",
                        "expected_absent": true
                    },
                    {
                        "op": "update",
                        "path": "update.txt",
                        "expected_digest": content_digest(b"update old\n"),
                        "edits": [{"old_text": "update old", "new_text": "update new"}]
                    },
                    {
                        "op": "delete",
                        "path": "delete.txt",
                        "expected_digest": content_digest(b"delete me\n")
                    },
                    {
                        "op": "move",
                        "from": "move.txt",
                        "to": "moved.txt",
                        "expected_digest": content_digest(b"move me\n"),
                        "expected_destination_absent": true
                    }
                ]}),
            )
            .unwrap();

        assert_eq!(output["change_count"], 4);
        assert_eq!(
            fs::read_to_string(root.join("created.txt")).unwrap(),
            "created\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("update.txt")).unwrap(),
            "update new\n"
        );
        assert!(!root.join("delete.txt").exists());
        assert!(!root.join("move.txt").exists());
        assert_eq!(
            fs::read_to_string(root.join("moved.txt")).unwrap(),
            "move me\n"
        );
    }

    #[test]
    fn apply_patch_rolls_back_an_injected_mid_commit_failure() {
        let root = temp_root("apply-patch-rollback");
        fs::write(root.join("first.txt"), "first old\n").unwrap();
        fs::write(root.join("second.txt"), "second old\n").unwrap();
        let tools = CoreTools::new(&root).unwrap();
        tools.fail_commit_after(Some(1));

        let error = tools
            .dispatch(
                FS_APPLY_PATCH_TOOL_ID,
                json!({"operations": [
                    {
                        "op": "update",
                        "path": "first.txt",
                        "expected_digest": content_digest(b"first old\n"),
                        "edits": [{"old_text": "first old", "new_text": "first new"}]
                    },
                    {
                        "op": "update",
                        "path": "second.txt",
                        "expected_digest": content_digest(b"second old\n"),
                        "edits": [{"old_text": "second old", "new_text": "second new"}]
                    }
                ]}),
            )
            .unwrap_err();

        assert!(format!("{error:#}").contains("injected patch commit failure"));
        assert_eq!(
            fs::read_to_string(root.join("first.txt")).unwrap(),
            "first old\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("second.txt")).unwrap(),
            "second old\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn apply_patch_rollback_restores_original_permissions() {
        let root = temp_root("apply-patch-rollback-permissions");
        let first = root.join("first.sh");
        let second = root.join("second.txt");
        fs::write(&first, "#!/bin/sh\nexit 0\n").unwrap();
        fs::write(&second, "second old\n").unwrap();
        set_mode(&first, 0o755);
        set_mode(&second, 0o640);
        let tools = CoreTools::new(&root).unwrap();
        tools.fail_commit_after(Some(1));

        tools
            .dispatch(
                FS_APPLY_PATCH_TOOL_ID,
                json!({"operations": [
                    {
                        "op": "update",
                        "path": "first.sh",
                        "expected_digest": content_digest(b"#!/bin/sh\nexit 0\n"),
                        "edits": [{"old_text": "exit 0", "new_text": "exit 1"}]
                    },
                    {
                        "op": "update",
                        "path": "second.txt",
                        "expected_digest": content_digest(b"second old\n"),
                        "edits": [{"old_text": "second old", "new_text": "second new"}]
                    }
                ]}),
            )
            .unwrap_err();

        assert_eq!(fs::read_to_string(&first).unwrap(), "#!/bin/sh\nexit 0\n");
        assert_eq!(fs::read_to_string(&second).unwrap(), "second old\n");
        assert_eq!(mode(&first), 0o755);
        assert_eq!(mode(&second), 0o640);
    }

    #[cfg(unix)]
    #[test]
    fn read_rejects_symlink_paths() {
        let root = temp_root("read-symlink");
        fs::write(root.join("README.MD"), "hello\n").unwrap();
        std::os::unix::fs::symlink(root.join("README.MD"), root.join("linked.md")).unwrap();
        let tools = CoreTools::new(&root).unwrap();

        let err = tools
            .dispatch(FS_READ_TOOL_ID, json!({"path": "linked.md"}))
            .unwrap_err();

        assert!(format!("{err:#}").contains("symlink"));
    }

    #[test]
    fn malformed_artifact_registration_fails_closed() {
        let root = temp_root("malformed-artifact-registration");
        fs::write(
            root.join(".gitmodules"),
            "[submodule \"unterminated]\n\tpath = .agl/tasks\n\turl = test://tasks\n",
        )
        .unwrap();

        let error = CoreTools::new(&root).unwrap_err();

        assert!(format!("{error:#}").contains("invalid .gitmodules"));
    }

    #[test]
    fn generic_filesystem_requires_an_exact_artifact_handle_route() {
        let root = temp_root("artifact-handle-route");
        fs::create_dir_all(root.join(".agl/tasks")).unwrap();
        fs::write(root.join(".agl/tasks/task.md"), "task\n").unwrap();
        fs::write(
            root.join(".gitmodules"),
            "[submodule \"core.repo:tasks\"]\n\tpath = .agl/tasks\n\turl = test://tasks\n",
        )
        .unwrap();

        let unbound = CoreTools::new(&root).unwrap();
        let error = unbound
            .dispatch(FS_READ_TOOL_ID, json!({"path": ".agl/tasks/task.md"}))
            .unwrap_err();
        assert!(format!("{error:#}").contains("no admitted ArtifactHandle is bound"));

        let artifact_id = ArtifactId::new("core.repo:tasks").unwrap();
        let binding = agl_artifact::ArtifactBinding::verified_checkout(
            artifact_id.clone(),
            ".agl/tasks",
            "test://tasks",
            "1111111111111111111111111111111111111111",
            "1111111111111111111111111111111111111111",
            root.join(".agl/tasks"),
        )
        .unwrap();
        let read_declaration = agl_kernel::ArtifactDeclaration::new(
            artifact_id.clone(),
            agl_kernel::ArtifactKindId::new("agentlibre.file-tree").unwrap(),
            [ArtifactAccess::ReadTree],
        )
        .unwrap();
        let read_handle = ArtifactHandle::bind(read_declaration, binding.clone()).unwrap();
        let read_only = CoreTools::new(&root)
            .unwrap()
            .with_artifact_route(".agl/tasks", read_handle)
            .unwrap();
        assert_eq!(
            read_only
                .dispatch(FS_READ_TOOL_ID, json!({"path": ".agl/tasks/task.md"}))
                .unwrap()["status"],
            "ok"
        );
        let error = read_only
            .dispatch(
                FS_APPLY_PATCH_TOOL_ID,
                json!({"operations": [{
                    "op": "create",
                    "path": ".agl/tasks/new.md",
                    "content": "new\n",
                    "expected_absent": true
                }]}),
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("MutateTree"));

        let mutate_declaration = agl_kernel::ArtifactDeclaration::new(
            artifact_id,
            agl_kernel::ArtifactKindId::new("agentlibre.file-tree").unwrap(),
            [ArtifactAccess::ReadTree, ArtifactAccess::MutateTree],
        )
        .unwrap();
        let mutate_handle = ArtifactHandle::bind(mutate_declaration, binding).unwrap();
        let mutable = CoreTools::new(&root)
            .unwrap()
            .with_artifact_route(".agl/tasks", mutate_handle)
            .unwrap();
        assert_eq!(
            mutable
                .dispatch(
                    FS_APPLY_PATCH_TOOL_ID,
                    json!({"operations": [{
                        "op": "create",
                        "path": ".agl/tasks/new.md",
                        "content": "new\n",
                        "expected_absent": true
                    }]}),
                )
                .unwrap()["status"],
            "committed"
        );
    }
}
