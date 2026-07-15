use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::{ToolCatalog, ToolCatalogError, parse_action_args as parse_args};
use agl_capabilities::{
    ActionDeclaration, ActionHandler, ActionHandlerError, ActionInvocation, ActionResult,
    CapabilityId, OperationKind, ProviderDeclaration, ProviderId, StateEffect,
};
use agl_repo::{ArtifactAccess, ArtifactPathHandleRequest};
use anyhow::{Context, Result, bail, ensure};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

pub const PROVIDER_ID: &str = "core-tools";
pub const FS_READ_TOOL_ID: &str = "fs.read";
pub const FS_LIST_TOOL_ID: &str = "fs.list";
pub const FS_SEARCH_TOOL_ID: &str = "fs.search";
pub const FS_EDIT_TOOL_ID: &str = "fs.edit";

const DEFAULT_READ_LINES: usize = 200;
const MAX_READ_LINES: usize = 500;
const DEFAULT_LIST_ENTRIES: usize = 100;
const MAX_LIST_ENTRIES: usize = 500;
const DEFAULT_SEARCH_MATCHES: usize = 50;
const MAX_SEARCH_MATCHES: usize = 200;
const MAX_SEARCH_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct CoreTools {
    root: PathBuf,
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
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<Value> {
        match name {
            FS_READ_TOOL_ID => self.read(arguments),
            FS_LIST_TOOL_ID => self.list(arguments),
            FS_SEARCH_TOOL_ID => self.search(arguments),
            FS_EDIT_TOOL_ID => self.edit(arguments),
            _ => bail!("unknown core tool `{name}`"),
        }
    }

    fn read(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<ReadArgs>(FS_READ_TOOL_ID, arguments)?;
        let path = self.resolve_existing_path(&args.path, PathKind::File, false)?;
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read UTF-8 file {}", path.display()))?;
        let total_lines = content.lines().count();
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
            "lines": lines,
        }))
    }

    fn list(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<ListArgs>(FS_LIST_TOOL_ID, arguments)?;
        let path = self.resolve_existing_path(&args.path, PathKind::Directory, true)?;
        let max_entries = args
            .max_entries
            .unwrap_or(DEFAULT_LIST_ENTRIES)
            .min(MAX_LIST_ENTRIES);
        let recursive = args.recursive.unwrap_or(false);
        let mut entries = Vec::new();
        self.collect_entries(&path, recursive, max_entries, &mut entries)?;
        let truncated = entries.len() >= max_entries;

        Ok(json!({
            "tool": FS_LIST_TOOL_ID,
            "status": "ok",
            "path": self.display_path(&path),
            "entry_count": entries.len(),
            "truncated": truncated,
            "entries": entries,
        }))
    }

    fn search(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<SearchArgs>(FS_SEARCH_TOOL_ID, arguments)?;
        ensure!(
            !args.pattern.trim().is_empty(),
            "fs.search pattern cannot be blank"
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

    fn edit(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<EditArgs>(FS_EDIT_TOOL_ID, arguments)?;
        ensure!(
            !args.old_text.is_empty(),
            "fs.edit old_text cannot be empty"
        );
        let relative = normalize_repo_path(&args.path, false)?;
        self.enforce_artifact_write_access(&relative)?;
        let path = self.resolve_existing_path(&args.path, PathKind::File, false)?;
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read UTF-8 file {}", path.display()))?;
        let occurrences = content.matches(&args.old_text).count();
        ensure!(
            occurrences == 1,
            "fs.edit expected old_text to match exactly once, found {occurrences}"
        );
        let updated = content.replacen(&args.old_text, &args.new_text, 1);
        fs::write(&path, updated.as_bytes())
            .with_context(|| format!("failed to write edited file {}", path.display()))?;

        Ok(json!({
            "tool": FS_EDIT_TOOL_ID,
            "status": "edited",
            "path": self.display_path(&path),
            "old_bytes": args.old_text.len(),
            "new_bytes": args.new_text.len(),
        }))
    }

    fn resolve_existing_path(
        &self,
        raw: &str,
        kind: PathKind,
        allow_root: bool,
    ) -> Result<PathBuf> {
        let relative = normalize_repo_path(raw, allow_root)?;
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

    fn collect_entries(
        &self,
        path: &Path,
        recursive: bool,
        max_entries: usize,
        entries: &mut Vec<Value>,
    ) -> Result<()> {
        if entries.len() >= max_entries {
            return Ok(());
        }
        for entry in sorted_dir_entries(path)? {
            if entries.len() >= max_entries {
                break;
            }
            let file_type = entry
                .file_type()
                .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
            if file_type.is_symlink() || entry.file_name() == ".git" {
                continue;
            }
            entries.push(json!({
                "path": self.display_path(&entry.path()),
                "kind": if file_type.is_dir() { "directory" } else { "file" },
            }));
            if recursive && file_type.is_dir() {
                self.collect_entries(&entry.path(), recursive, max_entries, entries)?;
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
                self.collect_matches(&entry.path(), needle, case_sensitive, max_matches, matches)?;
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
        if !is_agl_path(relative) {
            return Ok(());
        }

        agl_repo::resolve_artifact_path_handle(
            &self.root,
            &ArtifactPathHandleRequest {
                path: relative.to_path_buf(),
                access: ArtifactAccess::Write,
            },
        )
        .context("failed to resolve artifact write handle")?;

        Ok(())
    }
}

impl ActionHandler for CoreTools {
    fn dispatch(
        &self,
        invocation: ActionInvocation,
    ) -> std::result::Result<ActionResult, ActionHandlerError> {
        let data = self.dispatch(invocation.capability_id.as_str(), invocation.arguments)?;
        Ok(ActionResult::new(data))
    }
}

pub fn declaration() -> ProviderDeclaration {
    ProviderDeclaration::builtin(
        ProviderId::new(PROVIDER_ID).expect("core tool provider id is valid"),
        "Core Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("core tool declaration is valid")
    .with_action(action::<ReadArgs>(
        FS_READ_TOOL_ID,
        "Read a UTF-8 file from the repository with line bounds.",
        OperationKind::Read,
    ))
    .with_action(action::<ListArgs>(
        FS_LIST_TOOL_ID,
        "List repository directory entries.",
        OperationKind::Read,
    ))
    .with_action(action::<SearchArgs>(
        FS_SEARCH_TOOL_ID,
        "Search repository text files for a literal pattern.",
        OperationKind::Read,
    ))
    .with_action(
        action::<EditArgs>(
            FS_EDIT_TOOL_ID,
            "Replace one exact text span in an existing repository file.",
            OperationKind::Write,
        )
        .with_state_effects([StateEffect::RepoFiles]),
    )
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn action<T: JsonSchema>(
    id: &str,
    description: &str,
    operation_kind: OperationKind,
) -> ActionDeclaration {
    ActionDeclaration::from_schema::<T>(
        CapabilityId::new(id).expect("core tool id is valid"),
        description,
        operation_kind,
    )
    .expect("core tool declaration schema is valid")
}

fn sorted_dir_entries(path: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("failed to read directory {}", path.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read directory entry in {}", path.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
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

fn is_agl_path(path: &Path) -> bool {
    matches!(
        path.components().next(),
        Some(Component::Normal(component)) if component == ".agl"
    )
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
struct ListArgs {
    path: String,
    recursive: Option<bool>,
    max_entries: Option<usize>,
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
struct EditArgs {
    path: String,
    old_text: String,
    new_text: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::test_support::temp_root;

    use super::*;

    fn declare_artifact(root: &Path, id: &str, path: &str, access: &str) {
        let manifest_path = root.join(agl_repo::WORKSPACE_MANIFEST_PATH);
        let mut manifest = fs::read_to_string(&manifest_path).unwrap();
        manifest.push_str(&format!(
            "\n[artifacts.{id}]\nkind = \"local\"\npath = {path:?}\nrequired = true\naccess = {access:?}\n"
        ));
        fs::write(manifest_path, manifest).unwrap();
    }

    #[test]
    fn declaration_registers_core_filesystem_tools() {
        let declaration = declaration();
        declaration.validate().unwrap();
        let read = declaration
            .action(&CapabilityId::new(FS_READ_TOOL_ID).unwrap())
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
                .action(&CapabilityId::new(FS_EDIT_TOOL_ID).unwrap())
                .is_some()
        );
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
    fn edit_replaces_one_exact_span() {
        let root = temp_root("edit");
        let path = root.join("README.MD");
        fs::write(&path, "hello old\n").unwrap();
        let tools = CoreTools::new(&root).unwrap();

        let output = tools
            .dispatch(
                FS_EDIT_TOOL_ID,
                json!({"path": "README.MD", "old_text": "old", "new_text": "new"}),
            )
            .unwrap();

        assert_eq!(output["status"], "edited");
        assert_eq!(output["path"], "README.MD");
        assert_eq!(fs::read_to_string(path).unwrap(), "hello new\n");
    }

    #[test]
    fn edit_rejects_ambiguous_old_text() {
        let root = temp_root("edit-ambiguous");
        fs::write(root.join("README.MD"), "same\nsame\n").unwrap();
        let tools = CoreTools::new(&root).unwrap();

        let err = tools
            .dispatch(
                FS_EDIT_TOOL_ID,
                json!({"path": "README.MD", "old_text": "same", "new_text": "changed"}),
            )
            .unwrap_err();

        assert!(format!("{err:#}").contains("found 2"));
    }

    #[test]
    fn edit_allows_writable_artifact_paths() {
        let root = temp_root("edit-artifact-writable");
        fs::create_dir_all(root.join(".git")).unwrap();
        agl_repo::init_repo_workspace(&root, &agl_repo::RepoInitOptions::default()).unwrap();
        declare_artifact(&root, "tasks", ".agl/tasks", "read_write");
        fs::create_dir_all(root.join(".agl/tasks")).unwrap();
        fs::write(
            root.join(".agl/tasks/task.md"),
            "# Problem\n\nold problem.\n\n# Goal\n\nGoal.\n\n# Scope\n\nScope.\n\n# Non-goals\n\nNone.\n\n# Implementation\n\nSteps.\n\n# Acceptance Criteria\n\nDone.\n\n# Verification\n\nTests.\n",
        )
        .unwrap();
        let tools = CoreTools::new(&root).unwrap();

        let output = tools
            .dispatch(
                FS_EDIT_TOOL_ID,
                json!({"path": ".agl/tasks/task.md", "old_text": "old", "new_text": "new"}),
            )
            .unwrap();

        assert_eq!(output["status"], "edited");
        assert_eq!(
            fs::read_to_string(root.join(".agl/tasks/task.md")).unwrap(),
            "# Problem\n\nnew problem.\n\n# Goal\n\nGoal.\n\n# Scope\n\nScope.\n\n# Non-goals\n\nNone.\n\n# Implementation\n\nSteps.\n\n# Acceptance Criteria\n\nDone.\n\n# Verification\n\nTests.\n"
        );
    }

    #[test]
    fn edit_rejects_read_only_artifact_paths() {
        let root = temp_root("edit-artifact-read-only");
        fs::create_dir_all(root.join(".git")).unwrap();
        agl_repo::init_repo_workspace(&root, &agl_repo::RepoInitOptions::default()).unwrap();
        declare_artifact(&root, "skills", ".agl/skills", "read");
        fs::create_dir_all(root.join(".agl/skills")).unwrap();
        fs::write(root.join(".agl/skills/SKILL.md"), "hello old\n").unwrap();
        let tools = CoreTools::new(&root).unwrap();

        let err = tools
            .dispatch(
                FS_EDIT_TOOL_ID,
                json!({"path": ".agl/skills/SKILL.md", "old_text": "old", "new_text": "new"}),
            )
            .unwrap_err();

        assert!(format!("{err:#}").contains("does not permit"));
    }

    #[test]
    fn edit_rejects_undeclared_artifact_paths() {
        let root = temp_root("edit-artifact-undeclared");
        fs::create_dir_all(root.join(".git")).unwrap();
        agl_repo::init_repo_workspace(&root, &agl_repo::RepoInitOptions::default()).unwrap();
        declare_artifact(&root, "tasks", ".agl/tasks", "read_write");
        fs::create_dir_all(root.join(".agl/unknown")).unwrap();
        fs::write(root.join(".agl/unknown/file.md"), "hello old\n").unwrap();
        let tools = CoreTools::new(&root).unwrap();

        let err = tools
            .dispatch(
                FS_EDIT_TOOL_ID,
                json!({"path": ".agl/unknown/file.md", "old_text": "old", "new_text": "new"}),
            )
            .unwrap_err();

        assert!(format!("{err:#}").contains("not declared"));
    }

    #[test]
    fn edit_rejects_artifact_root_that_is_not_directory() {
        let root = temp_root("edit-artifact-not-directory");
        fs::create_dir_all(root.join(".git")).unwrap();
        agl_repo::init_repo_workspace(&root, &agl_repo::RepoInitOptions::default()).unwrap();
        declare_artifact(&root, "tasks", ".agl/tasks", "read_write");
        let _ = fs::remove_dir_all(root.join(".agl/tasks"));
        fs::create_dir_all(root.join(".agl")).unwrap();
        fs::write(root.join(".agl/tasks"), "hello old\n").unwrap();
        let tools = CoreTools::new(&root).unwrap();

        let err = tools
            .dispatch(
                FS_EDIT_TOOL_ID,
                json!({"path": ".agl/tasks", "old_text": "old", "new_text": "new"}),
            )
            .unwrap_err();

        let error = format!("{err:#}");
        assert!(error.contains("not_directory"));
        assert_eq!(
            fs::read_to_string(root.join(".agl/tasks")).unwrap(),
            "hello old\n"
        );
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
}
