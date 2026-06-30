use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

mod types;

pub use types::*;

const MANAGED_HOOK_MARKER: &str = "agentLIBRE managed hook";

pub fn init_repo_workspace(
    start: impl AsRef<Path>,
    options: &RepoInitOptions,
) -> Result<RepoInitReport> {
    let workspace_root = resolve_repo_root(start)?;
    let manifest_path = workspace_root.join(WORKSPACE_MANIFEST_PATH);
    let mut changes = Vec::new();

    let manifest = init_manifest(options)?;

    create_dir_change(
        &workspace_root,
        Path::new(AGL_DIR),
        options.dry_run,
        &mut changes,
    )?;

    write_manifest_change(&manifest_path, &manifest, options, &mut changes)?;

    for component in manifest.components.values() {
        match component.kind {
            ComponentKind::Local | ComponentKind::Generated | ComponentKind::Ignored => {
                create_dir_change(
                    &workspace_root,
                    &component.path,
                    options.dry_run,
                    &mut changes,
                )?;
            }
            ComponentKind::Submodule => changes.push(RepoInitChange {
                path: component.path.clone(),
                action: RepoInitAction::DeclaredSubmodule,
            }),
            ComponentKind::Git => changes.push(RepoInitChange {
                path: component.path.clone(),
                action: RepoInitAction::DeclaredGitComponent,
            }),
        }
    }

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

pub fn read_workspace_profile(path: impl AsRef<Path>) -> Result<WorkspaceProfile> {
    let path = path.as_ref();
    let content =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let profile: WorkspaceProfile =
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))?;
    validate_profile(&profile)?;
    Ok(profile)
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
        if let Some(requested) = &options.component
            && requested != name
        {
            continue;
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
        let status = plan_hook_install(&hooks_dir, hook, options)?;
        if status.action == HookInstallAction::Conflict {
            errors.push(format!("hook_conflict: {}", status.path.display()));
        }
        hooks.push(status);
    }

    if !errors.is_empty() {
        for status in &mut hooks {
            status.action = dry_run_hook_action(status.action);
        }
    } else if !options.dry_run {
        for status in &mut hooks {
            apply_hook_install(&hooks_dir, status)?;
        }
    }

    Ok(HookInstallReport {
        workspace_root,
        dry_run: options.dry_run,
        hooks,
        errors,
    })
}

pub fn export_repo_profile(
    start: impl AsRef<Path>,
    options: &RepoExportProfileOptions,
) -> Result<RepoExportProfileReport> {
    let workspace_root = resolve_repo_root(start)?;
    let manifest_path = workspace_root.join(WORKSPACE_MANIFEST_PATH);
    let manifest = read_manifest(&manifest_path).with_context(|| {
        format!(
            "failed to read workspace manifest {}",
            manifest_path.display()
        )
    })?;
    let status = status_repo_workspace(
        &workspace_root,
        &RepoStatusOptions {
            component: None,
            strict: false,
        },
    )?;
    let profile = profile_from_workspace_manifest(&manifest, &status);
    validate_profile(&profile)?;

    let content = toml::to_string_pretty(&profile).context("failed to render workspace profile")?;
    if let Some(parent) = options.out.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    let mut open = fs::OpenOptions::new();
    open.write(true).create(true);
    if options.force {
        open.truncate(true);
    } else {
        open.create_new(true);
    }
    let mut file = open
        .open(&options.out)
        .with_context(|| format!("failed to create profile export {}", options.out.display()))?;
    use std::io::Write;
    file.write_all(content.as_bytes())
        .with_context(|| format!("failed to write profile export {}", options.out.display()))?;

    Ok(RepoExportProfileReport {
        workspace_root,
        profile_path: options.out.clone(),
        wrote: true,
        profile,
    })
}

pub fn render_repo_profile(start: impl AsRef<Path>) -> Result<WorkspaceProfile> {
    let workspace_root = resolve_repo_root(start)?;
    let manifest_path = workspace_root.join(WORKSPACE_MANIFEST_PATH);
    let manifest = read_manifest(&manifest_path).with_context(|| {
        format!(
            "failed to read workspace manifest {}",
            manifest_path.display()
        )
    })?;
    let status = status_repo_workspace(
        &workspace_root,
        &RepoStatusOptions {
            component: None,
            strict: false,
        },
    )?;
    let profile = profile_from_workspace_manifest(&manifest, &status);
    validate_profile(&profile)?;
    Ok(profile)
}

pub fn render_repo_profile_toml(start: impl AsRef<Path>) -> Result<String> {
    let profile = render_repo_profile(start)?;
    toml::to_string_pretty(&profile).context("failed to render workspace profile")
}

fn init_manifest(options: &RepoInitOptions) -> Result<WorkspaceManifest> {
    if let Some(profile_file) = &options.profile_file {
        let profile = read_workspace_profile(profile_file)?;
        if options.profile != DEFAULT_PROFILE && options.profile != profile.name {
            bail!(
                "profile file name {} does not match requested profile {}",
                profile.name,
                options.profile
            );
        }
        return Ok(WorkspaceManifest {
            version: profile.version,
            profile: profile.name,
            components: profile.components,
        });
    }

    if options.profile != DEFAULT_PROFILE {
        bail!("unsupported repo workflow profile: {}", options.profile);
    }
    Ok(default_manifest())
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

fn profile_from_workspace_manifest(
    manifest: &WorkspaceManifest,
    status: &RepoStatusReport,
) -> WorkspaceProfile {
    let skill_pack = workspace_skill_pack_identity(manifest, status);
    let mut components = manifest.components.clone();
    if let Some(identity) = &skill_pack
        && let Some(component) = components.get_mut(&identity.component)
    {
        component.path = identity.path.clone();
        component.url = identity.url.clone();
        component.rev = identity.rev.clone();
        component.commit = identity.commit.clone();
        component.tree = identity.tree.clone();
        component.lock = identity.lock.clone();
    }
    WorkspaceProfile {
        version: manifest.version,
        name: manifest.profile.clone(),
        components,
        policy: WorkspaceProfilePolicy::default(),
        skill_pack,
    }
}

fn workspace_skill_pack_identity(
    manifest: &WorkspaceManifest,
    status: &RepoStatusReport,
) -> Option<WorkspaceSkillPackIdentity> {
    let component = manifest.components.get("skills")?;
    let component_status = status
        .components
        .iter()
        .find(|status| status.name == "skills");
    Some(WorkspaceSkillPackIdentity {
        component: "skills".to_string(),
        path: component.path.clone(),
        url: component_status
            .and_then(|status| status.actual_url.clone())
            .or_else(|| component.url.clone()),
        rev: component.rev.clone(),
        commit: component_status
            .and_then(|status| status.actual_commit.clone())
            .or_else(|| component.commit.clone()),
        tree: component_status
            .and_then(|status| status.actual_tree.clone())
            .or_else(|| component.tree.clone()),
        lock: component.lock.clone(),
        same_ids_when_pinned: true,
    })
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
    if manifest.profile.trim().is_empty() {
        errors.push("profile_empty".to_string());
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

fn validate_profile(profile: &WorkspaceProfile) -> Result<()> {
    if profile.version != 1 {
        bail!("unsupported workspace profile version: {}", profile.version);
    }
    if profile.name.trim().is_empty() {
        bail!("workspace profile name cannot be blank");
    }
    if profile.components.is_empty() {
        bail!("workspace profile must define at least one component");
    }
    if profile.policy.trust.import_local_trust {
        bail!("workspace profile must not import local trust");
    }
    let mut errors = Vec::new();
    validate_manifest(
        &WorkspaceManifest {
            version: profile.version,
            profile: profile.name.clone(),
            components: profile.components.clone(),
        },
        &mut errors,
    );
    if let Some(skill_pack) = &profile.skill_pack {
        match profile.components.get(&skill_pack.component) {
            Some(component) => validate_skill_pack_matches_component(
                &mut errors,
                &skill_pack.component,
                skill_pack,
                component,
            ),
            None => errors.push(format!(
                "skill_pack_component_missing: {}",
                skill_pack.component
            )),
        }
    }
    if !errors.is_empty() {
        bail!("workspace profile is invalid: {}", errors.join(", "));
    }
    Ok(())
}

fn validate_skill_pack_matches_component(
    errors: &mut Vec<String>,
    component_name: &str,
    skill_pack: &WorkspaceSkillPackIdentity,
    component: &WorkspaceComponent,
) {
    if skill_pack.path != component.path {
        errors.push(format!("skill_pack.{component_name}.path_mismatch"));
    }
    if skill_pack.url != component.url {
        errors.push(format!("skill_pack.{component_name}.url_mismatch"));
    }
    if skill_pack.rev != component.rev {
        errors.push(format!("skill_pack.{component_name}.rev_mismatch"));
    }
    if skill_pack.commit != component.commit {
        errors.push(format!("skill_pack.{component_name}.commit_mismatch"));
    }
    if skill_pack.tree != component.tree {
        errors.push(format!("skill_pack.{component_name}.tree_mismatch"));
    }
    if skill_pack.lock != component.lock {
        errors.push(format!("skill_pack.{component_name}.lock_mismatch"));
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
                if let Some(expected) = &component.url
                    && expected != &actual
                {
                    status.errors.push("remote_mismatch".to_string());
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
            if let Some(expected) = &component.commit
                && expected != &actual
            {
                status.errors.push("commit_mismatch".to_string());
            }
            status.actual_commit = Some(actual);
        }
        Err(err) => status.errors.push(format!("head_unavailable: {err:#}")),
    }

    match git_output(&absolute_path, ["rev-parse", "HEAD^{tree}"]) {
        Ok(output) => {
            let actual = output.trim().to_string();
            if let Some(expected) = &component.tree
                && expected != &actual
            {
                status.errors.push("tree_mismatch".to_string());
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

fn plan_hook_install(
    hooks_dir: &Path,
    hook: &str,
    options: &RepoHooksOptions,
) -> Result<HookInstallStatus> {
    let path = hooks_dir.join(hook);

    if path.exists() {
        let existing = fs::read_to_string(&path).unwrap_or_default();
        let managed = existing.contains(MANAGED_HOOK_MARKER);
        if !managed && !options.force {
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
                action: if managed {
                    HookInstallAction::WouldReplaceManaged
                } else {
                    HookInstallAction::WouldReplaceUnmanaged
                },
            });
        }
        return Ok(HookInstallStatus {
            hook: hook.to_string(),
            path,
            action: if managed {
                HookInstallAction::ReplacedManaged
            } else {
                HookInstallAction::ReplacedUnmanaged
            },
        });
    }

    if options.dry_run {
        return Ok(HookInstallStatus {
            hook: hook.to_string(),
            path,
            action: HookInstallAction::WouldInstall,
        });
    }
    Ok(HookInstallStatus {
        hook: hook.to_string(),
        path,
        action: HookInstallAction::Installed,
    })
}

fn apply_hook_install(hooks_dir: &Path, status: &mut HookInstallStatus) -> Result<()> {
    if matches!(
        status.action,
        HookInstallAction::AlreadyManaged | HookInstallAction::Conflict
    ) {
        return Ok(());
    }
    let content = hook_content(&status.hook);
    fs::create_dir_all(hooks_dir)
        .with_context(|| format!("failed to create hooks directory {}", hooks_dir.display()))?;
    fs::write(&status.path, content)
        .with_context(|| format!("failed to write hook {}", status.path.display()))?;
    make_executable(&status.path)
}

fn dry_run_hook_action(action: HookInstallAction) -> HookInstallAction {
    match action {
        HookInstallAction::Installed => HookInstallAction::WouldInstall,
        HookInstallAction::ReplacedManaged => HookInstallAction::WouldReplaceManaged,
        HookInstallAction::ReplacedUnmanaged => HookInstallAction::WouldReplaceUnmanaged,
        other => other,
    }
}

fn hook_content(hook: &str) -> String {
    format!(
        r#"#!/bin/sh
# {MANAGED_HOOK_MARKER}: {hook}
set -eu
AGL_BIN="${{AGL_BIN:-agl}}"
if ! command -v "$AGL_BIN" >/dev/null 2>&1; then
  echo "agentLIBRE hook error: $AGL_BIN not found on PATH; install agl or set AGL_BIN." >&2
  exit 127
fi
"$AGL_BIN" status --strict
"$AGL_BIN" skill verify
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
mod tests;
