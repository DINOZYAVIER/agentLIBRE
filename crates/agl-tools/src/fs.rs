use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::{
    ToolCapability, ToolCatalog, ToolCatalogError, ToolDeclaration, ToolHandler, ToolId, ToolInput,
    ToolOutput, ToolProviderDeclaration, ToolProviderId, ToolStateEffect,
    parse_tool_args as parse_args,
};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;

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

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<String> {
        match name {
            FS_READ_TOOL_ID => self.read(arguments),
            FS_LIST_TOOL_ID => self.list(arguments),
            FS_SEARCH_TOOL_ID => self.search(arguments),
            FS_EDIT_TOOL_ID => self.edit(arguments),
            _ => bail!("unknown core tool `{name}`"),
        }
    }

    fn read(&self, arguments: Value) -> Result<String> {
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
        let mut selected = Vec::new();
        for (index, line) in content.lines().enumerate().skip(start_line - 1).take(limit) {
            selected.push(format!("{:>6} | {}", index + 1, line));
        }
        let end_line = start_line.saturating_add(selected.len()).saturating_sub(1);
        let truncated = end_line < total_lines;

        Ok(format!(
            "tool=fs.read\npath={}\nstart_line={start_line}\nend_line={end_line}\ntotal_lines={total_lines}\ntruncated={truncated}\n---\n{}",
            self.display_path(&path),
            selected.join("\n")
        ))
    }

    fn list(&self, arguments: Value) -> Result<String> {
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

        Ok(format!(
            "tool=fs.list\npath={}\nentries={}\ntruncated={truncated}\n---\n{}",
            self.display_path(&path),
            entries.len(),
            entries.join("\n")
        ))
    }

    fn search(&self, arguments: Value) -> Result<String> {
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

        Ok(format!(
            "tool=fs.search\npath={}\npattern={}\nmatches={}\ntruncated={truncated}\n---\n{}",
            self.display_path(&path),
            args.pattern,
            matches.len(),
            matches.join("\n")
        ))
    }

    fn edit(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<EditArgs>(FS_EDIT_TOOL_ID, arguments)?;
        ensure!(
            !args.old_text.is_empty(),
            "fs.edit old_text cannot be empty"
        );
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

        Ok(format!(
            "tool=fs.edit\npath={}\nold_bytes={}\nnew_bytes={}\nstatus=edited",
            self.display_path(&path),
            args.old_text.len(),
            args.new_text.len()
        ))
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
        entries: &mut Vec<String>,
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
            let mut name = self.display_path(&entry.path());
            if file_type.is_dir() {
                name.push('/');
            }
            entries.push(name);
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
        matches: &mut Vec<String>,
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
                    matches.push(format!(
                        "{}:{}:{}",
                        self.display_path(&entry.path()),
                        line_index + 1,
                        line
                    ));
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
}

impl ToolHandler for CoreTools {
    fn dispatch(&self, input: ToolInput) -> Result<ToolOutput> {
        let observation = self.dispatch(input.id.as_str(), input.arguments)?;
        Ok(ToolOutput { observation })
    }
}

pub fn declaration() -> ToolProviderDeclaration {
    ToolProviderDeclaration::new(
        ToolProviderId::new(PROVIDER_ID).expect("core tool provider id is valid"),
        "Core Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("core tool declaration is valid")
    .with_tool(tool(
        FS_READ_TOOL_ID,
        "Read a UTF-8 file from the repository with line bounds.",
        ToolCapability::Read,
        &["path"],
    ))
    .with_tool(tool(
        FS_LIST_TOOL_ID,
        "List repository directory entries.",
        ToolCapability::Read,
        &["path"],
    ))
    .with_tool(tool(
        FS_SEARCH_TOOL_ID,
        "Search repository text files for a literal pattern.",
        ToolCapability::Read,
        &["pattern"],
    ))
    .with_tool(tool(
        FS_EDIT_TOOL_ID,
        "Replace one exact text span in an existing repository file.",
        ToolCapability::Write,
        &["path", "old_text", "new_text"],
    ))
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn tool(
    id: &str,
    description: &str,
    capability: ToolCapability,
    required_arguments: &[&str],
) -> ToolDeclaration {
    let declaration = ToolDeclaration::new(
        ToolId::new(id).expect("core tool id is valid"),
        description,
        capability,
        required_arguments.iter().copied(),
    );
    if id == FS_EDIT_TOOL_ID {
        declaration.with_state_effects([ToolStateEffect::RepoFiles])
    } else {
        declaration
    }
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

#[derive(Clone, Copy, Debug)]
enum PathKind {
    File,
    Directory,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    offset_line: Option<usize>,
    limit_lines: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    path: String,
    recursive: Option<bool>,
    max_entries: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    pattern: String,
    path: Option<String>,
    max_matches: Option<usize>,
    case_sensitive: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    old_text: String,
    new_text: String,
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;

    static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_root(name: &str) -> PathBuf {
        let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("agl-tools-{name}-{}-{id}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn declaration_registers_core_filesystem_tools() {
        let mut catalog = ToolCatalog::new();
        register(&mut catalog).unwrap();

        let read = catalog
            .tool(&ToolId::new(FS_READ_TOOL_ID).unwrap())
            .unwrap();
        assert_eq!(
            read.description,
            "Read a UTF-8 file from the repository with line bounds."
        );
        assert_eq!(read.required_arguments, vec!["path"]);
        assert!(
            catalog
                .tool(&ToolId::new(FS_EDIT_TOOL_ID).unwrap())
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
        fs::remove_dir_all(root).unwrap();
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

        assert!(output.contains("README.MD"));
        assert!(!output.contains(".git"));
        fs::remove_dir_all(root).unwrap();
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

        assert!(output.contains("matches=1"));
        assert!(output.contains("truncated=true"));
        assert!(output.contains("src/lib.rs:1:alpha"));
        assert!(!output.contains("src/lib.rs:3:alpha"));
        fs::remove_dir_all(root).unwrap();
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

        assert!(output.contains("status=edited"));
        assert_eq!(fs::read_to_string(path).unwrap(), "hello new\n");
        fs::remove_dir_all(root).unwrap();
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
        fs::remove_dir_all(root).unwrap();
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
        fs::remove_dir_all(root).unwrap();
    }
}
