use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const AGL_DIR: &str = ".agl";
pub const WORKSPACE_MANIFEST_PATH: &str = ".agl/workspace.toml";
pub const DEFAULT_PROFILE: &str = "repo-workflow";
pub const DEFAULT_SKILLS_URL: &str = "git@github.com:agentlibre/agl-skills.git";
pub const DEFAULT_SKILLS_REV: &str = "v0.1.0";

const MANAGED_HOOK_MARKER: &str = "agentLIBRE managed hook";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoInitOptions {
    pub profile: String,
    pub dry_run: bool,
    pub force: bool,
}

impl Default for RepoInitOptions {
    fn default() -> Self {
        Self {
            profile: DEFAULT_PROFILE.to_string(),
            dry_run: false,
            force: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoStatusOptions {
    pub component: Option<String>,
    pub strict: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoHooksOptions {
    pub dry_run: bool,
    pub force: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepoInitReport {
    pub workspace_root: PathBuf,
    pub manifest_path: PathBuf,
    pub dry_run: bool,
    pub changes: Vec<RepoInitChange>,
    pub next_steps: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepoInitChange {
    pub path: PathBuf,
    pub action: RepoInitAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoInitAction {
    WouldCreateDir,
    CreatedDir,
    Exists,
    WouldWriteFile,
    WroteFile,
    WouldOverwriteFile,
    OverwroteFile,
    DeclaredSubmodule,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepoStatusReport {
    pub state: RepoStatusState,
    pub workspace_root: PathBuf,
    pub manifest_path: PathBuf,
    pub components: Vec<ComponentStatus>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub next_steps: Vec<String>,
}

impl RepoStatusReport {
    pub fn should_fail(&self, strict: bool) -> bool {
        !self.errors.is_empty() || (strict && !self.warnings.is_empty())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoStatusState {
    Ok,
    Warning,
    Invalid,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ComponentStatus {
    pub name: String,
    pub path: PathBuf,
    pub kind: ComponentKind,
    pub exists: bool,
    pub state: ComponentState,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub expected_url: Option<String>,
    pub actual_url: Option<String>,
    pub expected_rev: Option<String>,
    pub expected_commit: Option<String>,
    pub actual_commit: Option<String>,
    pub expected_tree: Option<String>,
    pub actual_tree: Option<String>,
    pub submodule_registered: Option<bool>,
    pub gitlink_present: Option<bool>,
    pub nested_git_top: Option<PathBuf>,
    pub tracked_dirty: Option<bool>,
    pub untracked_suspicious: Option<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentState {
    Ok,
    Warning,
    Missing,
    Invalid,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HookInstallReport {
    pub workspace_root: PathBuf,
    pub dry_run: bool,
    pub hooks: Vec<HookInstallStatus>,
    pub errors: Vec<String>,
}

impl HookInstallReport {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HookInstallStatus {
    pub hook: String,
    pub path: PathBuf,
    pub action: HookInstallAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookInstallAction {
    WouldInstall,
    Installed,
    AlreadyManaged,
    WouldReplaceManaged,
    ReplacedManaged,
    Conflict,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceManifest {
    pub version: u32,
    pub profile: String,
    pub components: BTreeMap<String, WorkspaceComponent>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceComponent {
    pub path: PathBuf,
    pub kind: ComponentKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComponentKind {
    Local,
    Git,
    Submodule,
    Generated,
    Ignored,
}

pub fn init_repo_workspace(
    start: impl AsRef<Path>,
    options: &RepoInitOptions,
) -> Result<RepoInitReport> {
    if options.profile != DEFAULT_PROFILE {
        bail!("unsupported repo workflow profile: {}", options.profile);
    }

    let workspace_root = resolve_repo_root(start)?;
    let manifest_path = workspace_root.join(WORKSPACE_MANIFEST_PATH);
    let mut changes = Vec::new();

    create_dir_change(
        &workspace_root,
        Path::new(AGL_DIR),
        options.dry_run,
        &mut changes,
    )?;

    let manifest = default_manifest();
    write_manifest_change(&manifest_path, &manifest, options, &mut changes)?;

    for path in [".agl/tasks", ".agl/reviews", ".agl/state"] {
        create_dir_change(
            &workspace_root,
            Path::new(path),
            options.dry_run,
            &mut changes,
        )?;
    }

    changes.push(RepoInitChange {
        path: PathBuf::from(".agl/skills"),
        action: RepoInitAction::DeclaredSubmodule,
    });

    Ok(RepoInitReport {
        workspace_root,
        manifest_path,
        dry_run: options.dry_run,
        changes,
        next_steps: vec![
            "agl status".to_string(),
            "agl skill lock".to_string(),
            "agl skill verify".to_string(),
        ],
    })
}

pub fn status_repo_workspace(
    start: impl AsRef<Path>,
    options: &RepoStatusOptions,
) -> Result<RepoStatusReport> {
    let workspace_root = resolve_repo_root(start)?;
    let manifest_path = workspace_root.join(WORKSPACE_MANIFEST_PATH);

    let manifest = match read_manifest(&manifest_path) {
        Ok(manifest) => manifest,
        Err(err) if is_not_found(&err) => {
            return Ok(RepoStatusReport {
                state: RepoStatusState::Invalid,
                workspace_root,
                manifest_path,
                components: Vec::new(),
                warnings: Vec::new(),
                errors: vec!["workspace_manifest_missing".to_string()],
                next_steps: vec!["agl init".to_string()],
            });
        }
        Err(err) => {
            return Ok(RepoStatusReport {
                state: RepoStatusState::Invalid,
                workspace_root,
                manifest_path,
                components: Vec::new(),
                warnings: Vec::new(),
                errors: vec![format!("workspace_manifest_invalid: {err:#}")],
                next_steps: vec!["fix .agl/workspace.toml".to_string()],
            });
        }
    };

    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    validate_manifest(&manifest, &mut errors);

    let mut components = Vec::new();
    for (name, component) in manifest.components.iter() {
        if let Some(requested) = &options.component {
            if requested != name {
                continue;
            }
        }
        components.push(component_status(&workspace_root, name, component));
    }

    if options.component.is_some() && components.is_empty() {
        errors.push(format!(
            "component_not_found: {}",
            options.component.as_deref().unwrap_or_default()
        ));
    }

    for component in &components {
        warnings.extend(
            component
                .warnings
                .iter()
                .map(|warning| format!("component.{}.{}", component.name, warning)),
        );
        errors.extend(
            component
                .errors
                .iter()
                .map(|error| format!("component.{}.{}", component.name, error)),
        );
    }

    let state = if !errors.is_empty() {
        RepoStatusState::Invalid
    } else if !warnings.is_empty() {
        RepoStatusState::Warning
    } else {
        RepoStatusState::Ok
    };

    let mut next_steps = Vec::new();
    if warnings
        .iter()
        .any(|warning| warning.contains("missing") || warning.contains("not_submodule"))
    {
        next_steps.push("initialize .agl/skills submodule".to_string());
    }
    if !errors.is_empty() {
        next_steps.push("inspect agl status --json".to_string());
    }

    Ok(RepoStatusReport {
        state,
        workspace_root,
        manifest_path,
        components,
        warnings,
        errors,
        next_steps,
    })
}

pub fn install_repo_hooks(
    start: impl AsRef<Path>,
    options: &RepoHooksOptions,
) -> Result<HookInstallReport> {
    let workspace_root = resolve_repo_root(start)?;
    let git_dir = workspace_root.join(".git");
    if !git_dir.is_dir() {
        bail!(
            "repo hooks require a git directory at {}",
            git_dir.display()
        );
    }

    let hooks_dir = git_dir.join("hooks");
    let mut hooks = Vec::new();
    let mut errors = Vec::new();
    for hook in ["pre-commit", "pre-push"] {
        hooks.push(install_hook(&hooks_dir, hook, options, &mut errors)?);
    }

    Ok(HookInstallReport {
        workspace_root,
        dry_run: options.dry_run,
        hooks,
        errors,
    })
}

fn default_manifest() -> WorkspaceManifest {
    WorkspaceManifest {
        version: 1,
        profile: DEFAULT_PROFILE.to_string(),
        components: BTreeMap::from([
            (
                "skills".to_string(),
                WorkspaceComponent {
                    path: PathBuf::from(".agl/skills"),
                    kind: ComponentKind::Submodule,
                    url: Some(DEFAULT_SKILLS_URL.to_string()),
                    rev: Some(DEFAULT_SKILLS_REV.to_string()),
                    commit: None,
                    tree: None,
                    lock: Some(PathBuf::from(".agl/skills.lock")),
                },
            ),
            (
                "tasks".to_string(),
                WorkspaceComponent {
                    path: PathBuf::from(".agl/tasks"),
                    kind: ComponentKind::Local,
                    url: None,
                    rev: None,
                    commit: None,
                    tree: None,
                    lock: None,
                },
            ),
            (
                "reviews".to_string(),
                WorkspaceComponent {
                    path: PathBuf::from(".agl/reviews"),
                    kind: ComponentKind::Generated,
                    url: None,
                    rev: None,
                    commit: None,
                    tree: None,
                    lock: None,
                },
            ),
            (
                "state".to_string(),
                WorkspaceComponent {
                    path: PathBuf::from(".agl/state"),
                    kind: ComponentKind::Ignored,
                    url: None,
                    rev: None,
                    commit: None,
                    tree: None,
                    lock: None,
                },
            ),
        ]),
    }
}

fn resolve_repo_root(start: impl AsRef<Path>) -> Result<PathBuf> {
    let start = start.as_ref().canonicalize().with_context(|| {
        format!(
            "failed to canonicalize repo root {}",
            start.as_ref().display()
        )
    })?;
    if !start.is_dir() {
        bail!("repo root is not a directory: {}", start.display());
    }
    Ok(find_git_top(&start).unwrap_or(start))
}

fn find_git_top(start: &Path) -> Option<PathBuf> {
    for candidate in start.ancestors() {
        if candidate.join(".git").exists() {
            return Some(candidate.to_path_buf());
        }
    }
    None
}

fn create_dir_change(
    workspace_root: &Path,
    relative_path: &Path,
    dry_run: bool,
    changes: &mut Vec<RepoInitChange>,
) -> Result<()> {
    let path = workspace_root.join(relative_path);
    if path.exists() {
        changes.push(RepoInitChange {
            path: relative_path.to_path_buf(),
            action: RepoInitAction::Exists,
        });
        return Ok(());
    }
    if dry_run {
        changes.push(RepoInitChange {
            path: relative_path.to_path_buf(),
            action: RepoInitAction::WouldCreateDir,
        });
        return Ok(());
    }
    fs::create_dir_all(&path)
        .with_context(|| format!("failed to create directory {}", path.display()))?;
    changes.push(RepoInitChange {
        path: relative_path.to_path_buf(),
        action: RepoInitAction::CreatedDir,
    });
    Ok(())
}

fn write_manifest_change(
    manifest_path: &Path,
    manifest: &WorkspaceManifest,
    options: &RepoInitOptions,
    changes: &mut Vec<RepoInitChange>,
) -> Result<()> {
    let relative = PathBuf::from(WORKSPACE_MANIFEST_PATH);
    if manifest_path.exists() && !options.force {
        changes.push(RepoInitChange {
            path: relative,
            action: RepoInitAction::Exists,
        });
        return Ok(());
    }

    let content =
        toml::to_string_pretty(manifest).context("failed to render workspace manifest")?;
    if options.dry_run {
        changes.push(RepoInitChange {
            path: relative,
            action: if manifest_path.exists() {
                RepoInitAction::WouldOverwriteFile
            } else {
                RepoInitAction::WouldWriteFile
            },
        });
        return Ok(());
    }

    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    fs::write(manifest_path, content)
        .with_context(|| format!("failed to write {}", manifest_path.display()))?;
    changes.push(RepoInitChange {
        path: relative,
        action: if options.force {
            RepoInitAction::OverwroteFile
        } else {
            RepoInitAction::WroteFile
        },
    });
    Ok(())
}

fn read_manifest(path: &Path) -> Result<WorkspaceManifest> {
    let content = fs::read_to_string(path)?;
    toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

fn is_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>()
        .is_some_and(|err| err.kind() == std::io::ErrorKind::NotFound)
}

fn validate_manifest(manifest: &WorkspaceManifest, errors: &mut Vec<String>) {
    if manifest.version != 1 {
        errors.push(format!(
            "unsupported_manifest_version: {}",
            manifest.version
        ));
    }
    if manifest.profile != DEFAULT_PROFILE {
        errors.push(format!("unsupported_profile: {}", manifest.profile));
    }
    let mut paths = BTreeMap::<PathBuf, String>::new();
    for (name, component) in &manifest.components {
        if let Err(err) = validate_component_path(&component.path) {
            errors.push(format!("component.{name}.invalid_path: {err}"));
        }
        if let Some(previous) = paths.insert(component.path.clone(), name.clone()) {
            errors.push(format!(
                "duplicate_component_path: {} used by {previous} and {name}",
                component.path.display()
            ));
        }
    }
}

fn validate_component_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("path cannot be empty");
    }
    if path.is_absolute() {
        bail!("path must be relative");
    }
    for component in path.components() {
        match component {
            Component::Normal(_) => {}
            Component::CurDir => {}
            Component::ParentDir => bail!("path cannot contain parent directory segments"),
            _ => bail!("path contains unsupported component"),
        }
    }
    Ok(())
}

fn component_status(
    workspace_root: &Path,
    name: &str,
    component: &WorkspaceComponent,
) -> ComponentStatus {
    let mut status = ComponentStatus {
        name: name.to_string(),
        path: component.path.clone(),
        kind: component.kind,
        exists: false,
        state: ComponentState::Ok,
        warnings: Vec::new(),
        errors: Vec::new(),
        expected_url: component.url.clone(),
        actual_url: None,
        expected_rev: component.rev.clone(),
        expected_commit: component.commit.clone(),
        actual_commit: None,
        expected_tree: component.tree.clone(),
        actual_tree: None,
        submodule_registered: None,
        gitlink_present: None,
        nested_git_top: None,
        tracked_dirty: None,
        untracked_suspicious: None,
    };

    let absolute_path = workspace_root.join(&component.path);
    if component.kind == ComponentKind::Submodule {
        let registered = submodule_registered(workspace_root, &component.path);
        status.submodule_registered = Some(registered);
        if !registered {
            status.warnings.push("not_registered_submodule".to_string());
        }

        let gitlink = gitlink_present(workspace_root, &component.path);
        status.gitlink_present = Some(gitlink);
        if !gitlink {
            status.warnings.push("gitlink_missing".to_string());
        }
    }

    if !absolute_path.exists() {
        status.state = match component.kind {
            ComponentKind::Submodule => ComponentState::Missing,
            _ => ComponentState::Invalid,
        };
        let issue = "missing".to_string();
        if component.kind == ComponentKind::Submodule {
            status.warnings.push(issue);
        } else {
            status.errors.push(issue);
        }
        return status;
    }
    status.exists = true;

    if matches!(
        component.kind,
        ComponentKind::Git | ComponentKind::Submodule
    ) {
        fill_git_status(workspace_root, component, &mut status);
    }

    if !status.errors.is_empty() {
        status.state = ComponentState::Invalid;
    } else if !status.warnings.is_empty() {
        status.state = ComponentState::Warning;
    }

    status
}

fn fill_git_status(
    workspace_root: &Path,
    component: &WorkspaceComponent,
    status: &mut ComponentStatus,
) {
    let absolute_path = workspace_root.join(&component.path);
    match git_output(&absolute_path, ["rev-parse", "--is-inside-work-tree"]) {
        Ok(output) if output.trim() == "true" => {}
        Ok(_) => {
            status.errors.push("not_git_worktree".to_string());
            return;
        }
        Err(err) => {
            status.errors.push(format!("git_unavailable: {err:#}"));
            return;
        }
    }

    match git_output(&absolute_path, ["rev-parse", "--show-toplevel"]) {
        Ok(output) => {
            let top = PathBuf::from(output.trim());
            status.nested_git_top = Some(top.clone());
            let expected_top = absolute_path
                .canonicalize()
                .unwrap_or(absolute_path.clone());
            let actual_top = top.canonicalize().unwrap_or(top);
            if actual_top != expected_top {
                status.errors.push("not_component_git_worktree".to_string());
                return;
            }
        }
        Err(err) => {
            status.errors.push(format!("git_top_unavailable: {err:#}"));
            return;
        }
    }

    match git_output(&absolute_path, ["config", "--get", "remote.origin.url"]) {
        Ok(output) => {
            let actual = output.trim().to_string();
            if !actual.is_empty() {
                if let Some(expected) = &component.url {
                    if expected != &actual {
                        status.errors.push("remote_mismatch".to_string());
                    }
                }
                status.actual_url = Some(actual);
            }
        }
        Err(_) => {
            if component.url.is_some() {
                status.errors.push("remote_missing".to_string());
            }
        }
    }

    match git_output(&absolute_path, ["rev-parse", "HEAD"]) {
        Ok(output) => {
            let actual = output.trim().to_string();
            if let Some(expected) = &component.commit {
                if expected != &actual {
                    status.errors.push("commit_mismatch".to_string());
                }
            }
            status.actual_commit = Some(actual);
        }
        Err(err) => status.errors.push(format!("head_unavailable: {err:#}")),
    }

    match git_output(&absolute_path, ["rev-parse", "HEAD^{tree}"]) {
        Ok(output) => {
            let actual = output.trim().to_string();
            if let Some(expected) = &component.tree {
                if expected != &actual {
                    status.errors.push("tree_mismatch".to_string());
                }
            }
            status.actual_tree = Some(actual);
        }
        Err(err) => status.errors.push(format!("tree_unavailable: {err:#}")),
    }

    match git_output(
        &absolute_path,
        ["status", "--porcelain=v1", "--untracked-files=all"],
    ) {
        Ok(output) => {
            let mut tracked_dirty = false;
            let mut untracked = false;
            for line in output.lines() {
                if line.starts_with("?? ") {
                    untracked = true;
                } else if !line.trim().is_empty() {
                    tracked_dirty = true;
                }
            }
            status.tracked_dirty = Some(tracked_dirty);
            status.untracked_suspicious = Some(untracked);
            if tracked_dirty {
                status.errors.push("dirty_worktree".to_string());
            }
            if untracked {
                status.errors.push("untracked_content".to_string());
            }
        }
        Err(err) => status.errors.push(format!("status_unavailable: {err:#}")),
    }
}

fn submodule_registered(workspace_root: &Path, component_path: &Path) -> bool {
    let Ok(content) = fs::read_to_string(workspace_root.join(".gitmodules")) else {
        return false;
    };
    let expected = format!("path = {}", component_path.display());
    content.lines().any(|line| line.trim() == expected)
}

fn gitlink_present(workspace_root: &Path, component_path: &Path) -> bool {
    let path = component_path.to_string_lossy();
    let Ok(output) = git_output(workspace_root, ["ls-files", "--stage", "--", path.as_ref()])
    else {
        return false;
    };
    output.lines().any(|line| line.starts_with("160000 "))
}

fn git_output<const N: usize>(dir: &Path, args: [&str; N]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("failed to run git in {}", dir.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn install_hook(
    hooks_dir: &Path,
    hook: &str,
    options: &RepoHooksOptions,
    errors: &mut Vec<String>,
) -> Result<HookInstallStatus> {
    let path = hooks_dir.join(hook);
    let content = hook_content(hook);

    if path.exists() {
        let existing = fs::read_to_string(&path).unwrap_or_default();
        let managed = existing.contains(MANAGED_HOOK_MARKER);
        if !managed && !options.force {
            errors.push(format!("hook_conflict: {}", path.display()));
            return Ok(HookInstallStatus {
                hook: hook.to_string(),
                path,
                action: HookInstallAction::Conflict,
            });
        }
        if managed && !options.force {
            return Ok(HookInstallStatus {
                hook: hook.to_string(),
                path,
                action: HookInstallAction::AlreadyManaged,
            });
        }
        if options.dry_run {
            return Ok(HookInstallStatus {
                hook: hook.to_string(),
                path,
                action: HookInstallAction::WouldReplaceManaged,
            });
        }
        fs::create_dir_all(hooks_dir)
            .with_context(|| format!("failed to create hooks directory {}", hooks_dir.display()))?;
        fs::write(&path, content)
            .with_context(|| format!("failed to write hook {}", path.display()))?;
        make_executable(&path)?;
        return Ok(HookInstallStatus {
            hook: hook.to_string(),
            path,
            action: HookInstallAction::ReplacedManaged,
        });
    }

    if options.dry_run {
        return Ok(HookInstallStatus {
            hook: hook.to_string(),
            path,
            action: HookInstallAction::WouldInstall,
        });
    }
    fs::create_dir_all(hooks_dir)
        .with_context(|| format!("failed to create hooks directory {}", hooks_dir.display()))?;
    fs::write(&path, content)
        .with_context(|| format!("failed to write hook {}", path.display()))?;
    make_executable(&path)?;
    Ok(HookInstallStatus {
        hook: hook.to_string(),
        path,
        action: HookInstallAction::Installed,
    })
}

fn hook_content(hook: &str) -> String {
    format!(
        r#"#!/bin/sh
# {MANAGED_HOOK_MARKER}: {hook}
set -eu
agl status --strict
agl skill verify
"#
    )
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to make hook executable {}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("agl-repo-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".git")).unwrap();
        root
    }

    fn init_git_repo(root: &Path) {
        let status = Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(root)
            .status()
            .expect("git init should run");
        assert!(status.success(), "git init failed for {}", root.display());
    }

    #[test]
    fn init_creates_manifest_and_local_component_dirs() {
        let root = temp_root("init");
        let report = init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();

        assert_eq!(report.workspace_root, root);
        assert!(report.manifest_path.exists());
        assert!(report.manifest_path.ends_with(WORKSPACE_MANIFEST_PATH));
        assert!(root.join(".agl/tasks").is_dir());
        assert!(root.join(".agl/reviews").is_dir());
        assert!(root.join(".agl/state").is_dir());
        assert!(!root.join(".agl/skills").exists());

        let manifest = fs::read_to_string(root.join(WORKSPACE_MANIFEST_PATH)).unwrap();
        assert!(manifest.contains("kind = \"submodule\""));
        assert!(manifest.contains(DEFAULT_SKILLS_URL));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn status_missing_manifest_reports_init_next_step() {
        let root = temp_root("missing-manifest");
        let report = status_repo_workspace(
            &root,
            &RepoStatusOptions {
                component: None,
                strict: false,
            },
        )
        .unwrap();

        assert_eq!(report.state, RepoStatusState::Invalid);
        assert!(
            report
                .errors
                .contains(&"workspace_manifest_missing".to_string())
        );
        assert!(report.next_steps.contains(&"agl init".to_string()));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn status_after_init_warns_about_missing_skills_submodule() {
        let root = temp_root("status-warning");
        init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
        let report = status_repo_workspace(
            &root,
            &RepoStatusOptions {
                component: None,
                strict: false,
            },
        )
        .unwrap();

        assert_eq!(report.state, RepoStatusState::Warning);
        assert!(!report.should_fail(false));
        assert!(report.should_fail(true));
        assert!(
            report
                .warnings
                .contains(&"component.skills.missing".to_string())
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_plain_skills_directory_is_not_component_git_worktree() {
        let root = temp_root("plain-skills");
        init_git_repo(&root);
        init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
        fs::create_dir_all(root.join(".agl/skills")).unwrap();

        let report = status_repo_workspace(
            &root,
            &RepoStatusOptions {
                component: Some("skills".to_string()),
                strict: false,
            },
        )
        .unwrap();

        assert_eq!(report.state, RepoStatusState::Invalid);
        let skills = report.components.first().expect("skills status");
        assert!(skills.exists);
        assert_eq!(skills.submodule_registered, Some(false));
        assert_eq!(skills.gitlink_present, Some(false));
        assert!(
            skills
                .errors
                .contains(&"not_component_git_worktree".to_string())
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_hooks_does_not_overwrite_unmanaged_hooks() {
        let root = temp_root("hooks-conflict");
        let hooks = root.join(".git/hooks");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("pre-commit"), "#!/bin/sh\nexit 0\n").unwrap();

        let report = install_repo_hooks(
            &root,
            &RepoHooksOptions {
                dry_run: false,
                force: false,
            },
        )
        .unwrap();

        assert!(report.has_errors());
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains("hook_conflict"))
        );

        fs::remove_dir_all(root).unwrap();
    }
}
