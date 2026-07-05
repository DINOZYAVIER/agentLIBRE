use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

mod artifacts;
mod hooks;
mod types;

pub use artifacts::{
    lock_artifacts, resolve_artifact_path_handle, status_artifacts, sync_artifacts,
};
pub use hooks::install_repo_hooks;
pub use types::*;

#[cfg(test)]
pub(crate) use hooks::hook_content;

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

    let mut next_steps = component_init_next_steps(&components);
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

fn component_init_next_steps(components: &[ComponentStatus]) -> Vec<String> {
    let mut steps = BTreeSet::new();
    for component in components {
        if component.kind != ComponentKind::Submodule {
            continue;
        }
        let needs_init = matches!(component.state, ComponentState::Missing)
            || component.warnings.iter().any(|warning| {
                matches!(
                    warning.as_str(),
                    "not_registered_submodule" | "gitlink_missing"
                )
            });
        if !needs_init {
            continue;
        }
        if component.name == "skills" {
            steps.insert("agl skill init".to_string());
        } else {
            steps.insert(format!("agl repo init-component {}", component.name));
        }
    }
    steps.into_iter().collect()
}

pub fn init_repo_component(
    start: impl AsRef<Path>,
    options: &RepoComponentInitOptions,
) -> Result<RepoComponentInitReport> {
    let workspace_root = resolve_repo_root(start)?;
    let manifest_path = workspace_root.join(WORKSPACE_MANIFEST_PATH);
    let manifest = read_manifest(&manifest_path).with_context(|| {
        format!(
            "failed to read workspace manifest {}",
            manifest_path.display()
        )
    })?;
    let mut errors = Vec::new();
    validate_manifest(&manifest, &mut errors);

    let Some(component) = manifest.components.get(&options.component) else {
        errors.push(format!("component_not_found: {}", options.component));
        return Ok(component_init_error_report(
            workspace_root,
            manifest_path,
            options,
            PathBuf::new(),
            errors,
        ));
    };
    let component_path = component.path.clone();

    if component.kind != ComponentKind::Submodule {
        errors.push(format!(
            "component_not_submodule: {} is {:?}",
            options.component, component.kind
        ));
    }
    if component.url.as_deref().is_none_or(str::is_empty) {
        errors.push(format!("component_url_missing: {}", options.component));
    }
    if !errors.is_empty() {
        return Ok(component_init_error_report(
            workspace_root,
            manifest_path,
            options,
            component_path,
            errors,
        ));
    }

    let mut actions = Vec::new();
    let registered = submodule_registered(&workspace_root, &component.path);
    let gitlink = gitlink_present(&workspace_root, &component.path);
    let exists = workspace_root.join(&component.path).exists();

    if registered || gitlink {
        if options.dry_run {
            actions.push(RepoComponentInitAction::WouldUpdateSubmodule);
        } else {
            git_run_with_file_protocol(
                &workspace_root,
                &[
                    "submodule",
                    "update",
                    "--init",
                    "--recursive",
                    "--",
                    &slash_path(&component.path),
                ],
            )
            .with_context(|| {
                format!(
                    "failed to initialize submodule {}",
                    component.path.display()
                )
            })?;
            actions.push(RepoComponentInitAction::UpdatedSubmodule);
        }
    } else if exists {
        errors.push(format!(
            "component_path_exists_not_submodule: {}",
            component.path.display()
        ));
        return Ok(component_init_error_report(
            workspace_root,
            manifest_path,
            options,
            component_path,
            errors,
        ));
    } else {
        let url = component.url.as_deref().expect("url checked above");
        if options.dry_run {
            actions.push(RepoComponentInitAction::WouldAddSubmodule);
        } else {
            git_run_with_file_protocol(
                &workspace_root,
                &[
                    "submodule",
                    "add",
                    "--name",
                    &options.component,
                    url,
                    &slash_path(&component.path),
                ],
            )
            .with_context(|| format!("failed to add submodule {}", component.path.display()))?;
            actions.push(RepoComponentInitAction::AddedSubmodule);
        }
    }

    if let Some(rev) = component.rev.as_deref().filter(|rev| !rev.is_empty()) {
        if options.dry_run {
            actions.push(RepoComponentInitAction::WouldCheckoutRev);
        } else {
            let submodule_root = workspace_root.join(&component.path);
            git_run_with_file_protocol(&submodule_root, &["fetch", "--tags", "--quiet"])
                .with_context(|| format!("failed to fetch {}", component.path.display()))?;
            git_run(&submodule_root, &["checkout", "--quiet", rev]).with_context(|| {
                format!(
                    "failed to checkout rev {} in {}",
                    rev,
                    component.path.display()
                )
            })?;
            actions.push(RepoComponentInitAction::CheckedOutRev);
        }
    }

    if actions.is_empty() {
        actions.push(RepoComponentInitAction::AlreadyInitialized);
    }

    Ok(RepoComponentInitReport {
        workspace_root,
        manifest_path,
        component: options.component.clone(),
        path: component_path,
        dry_run: options.dry_run,
        actions,
        errors,
    })
}

pub fn verify_task_specs(
    start: impl AsRef<Path>,
    options: &TaskSpecVerifyOptions,
) -> Result<TaskSpecVerifyReport> {
    let status = status_repo_workspace(
        start,
        &RepoStatusOptions {
            component: Some("tasks".to_string()),
            strict: options.strict,
        },
    )?;
    let workspace_root = status.workspace_root;
    let component = status.components.into_iter().next();
    let root = component
        .as_ref()
        .map(|component| workspace_root.join(&component.path))
        .unwrap_or_else(|| workspace_root.join(".agl/tasks"));
    let warnings = status.warnings;
    let mut errors = status.errors;

    if component.is_none() {
        errors.push("tasks_component_missing".to_string());
    }

    let files = if root.is_dir() {
        let mut paths = Vec::new();
        collect_markdown_files(&root, &mut paths)?;
        paths.sort();
        paths
            .into_iter()
            .map(|path| task_spec_file_status(&workspace_root, &path))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    if root.exists() && files.is_empty() {
        errors.push("no_task_spec_markdown_files".to_string());
    }
    for file in &files {
        if !file.errors.is_empty() || !file.valid {
            errors.push(format!("invalid_task_spec: {}", file.path.display()));
        }
    }

    let state = if !errors.is_empty() {
        TaskSpecVerifyState::Invalid
    } else if !warnings.is_empty() || files.iter().any(|file| !file.warnings.is_empty()) {
        TaskSpecVerifyState::Warning
    } else {
        TaskSpecVerifyState::Ok
    };

    Ok(TaskSpecVerifyReport {
        state,
        workspace_root,
        component,
        root,
        files,
        warnings,
        errors,
    })
}

pub fn validate_task_spec_markdown(markdown: &str) -> TaskSpecValidation {
    let lower = markdown.to_ascii_lowercase();
    let required = [
        ("problem", &["problem"][..]),
        ("goal", &["goal"][..]),
        ("scope", &["scope"][..]),
        ("non-goals", &["non-goals", "non goals"][..]),
        (
            "implementation",
            &["implementation", "implementation steps"][..],
        ),
        (
            "acceptance criteria",
            &["acceptance criteria", "acceptance"][..],
        ),
        (
            "verification",
            &["verification", "verification commands"][..],
        ),
    ];
    let missing_sections = required
        .into_iter()
        .filter(|(_canonical, aliases)| !aliases.iter().any(|alias| lower.contains(alias)))
        .map(|(canonical, _aliases)| canonical.to_string())
        .collect();
    TaskSpecValidation { missing_sections }
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
    let mut manifest = if let Some(profile_file) = &options.profile_file {
        let profile = read_workspace_profile(profile_file)?;
        if options.profile != DEFAULT_PROFILE && options.profile != profile.name {
            bail!(
                "profile file name {} does not match requested profile {}",
                profile.name,
                options.profile
            );
        }
        WorkspaceManifest {
            version: profile.version,
            profile: profile.name,
            components: profile.components,
            artifact_sources: profile.artifact_sources,
        }
    } else {
        if options.profile != DEFAULT_PROFILE {
            bail!("unsupported repo workflow profile: {}", options.profile);
        }
        default_manifest()
    };

    apply_init_component_overrides(&mut manifest, options)?;
    Ok(manifest)
}

pub(crate) fn default_manifest() -> WorkspaceManifest {
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
        artifact_sources: artifacts::default_artifact_sources(),
    }
}

fn apply_init_component_overrides(
    manifest: &mut WorkspaceManifest,
    options: &RepoInitOptions,
) -> Result<()> {
    if options.tasks_rev.is_some() && options.tasks_url.is_none() {
        bail!("--tasks-rev requires --tasks-url");
    }

    if options.skills_url.is_some() || options.skills_rev.is_some() {
        let skills = manifest
            .components
            .entry("skills".to_string())
            .or_insert_with(|| WorkspaceComponent {
                path: PathBuf::from(".agl/skills"),
                kind: ComponentKind::Submodule,
                url: None,
                rev: None,
                commit: None,
                tree: None,
                lock: Some(PathBuf::from(".agl/skills.lock")),
            });
        skills.kind = ComponentKind::Submodule;
        skills.url = options.skills_url.clone().or_else(|| skills.url.clone());
        skills.rev = options.skills_rev.clone().or_else(|| skills.rev.clone());
        skills.commit = None;
        skills.tree = None;
        skills
            .lock
            .get_or_insert_with(|| PathBuf::from(".agl/skills.lock"));
    }

    if let Some(tasks_url) = &options.tasks_url {
        let tasks = manifest
            .components
            .entry("tasks".to_string())
            .or_insert_with(|| WorkspaceComponent {
                path: PathBuf::from(".agl/tasks"),
                kind: ComponentKind::Submodule,
                url: None,
                rev: None,
                commit: None,
                tree: None,
                lock: Some(PathBuf::from(".agl/tasks.lock")),
            });
        tasks.kind = ComponentKind::Submodule;
        tasks.url = Some(tasks_url.clone());
        tasks.rev = options.tasks_rev.clone();
        tasks.commit = None;
        tasks.tree = None;
        tasks.lock = Some(PathBuf::from(".agl/tasks.lock"));
    }

    Ok(())
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
        artifact_sources: manifest.artifact_sources.clone(),
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

pub(crate) fn resolve_repo_root(start: impl AsRef<Path>) -> Result<PathBuf> {
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

pub(crate) fn read_manifest(path: &Path) -> Result<WorkspaceManifest> {
    let content = fs::read_to_string(path)?;
    toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

pub(crate) fn is_not_found(err: &anyhow::Error) -> bool {
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
            artifact_sources: profile.artifact_sources.clone(),
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

pub(crate) fn validate_component_path(path: &Path) -> Result<()> {
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

pub(crate) fn component_status(
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

fn component_init_error_report(
    workspace_root: PathBuf,
    manifest_path: PathBuf,
    options: &RepoComponentInitOptions,
    path: PathBuf,
    errors: Vec<String>,
) -> RepoComponentInitReport {
    RepoComponentInitReport {
        workspace_root,
        manifest_path,
        component: options.component.clone(),
        path,
        dry_run: options.dry_run,
        actions: Vec::new(),
        errors,
    }
}

fn collect_markdown_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        if file_name == ".git" {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_markdown_files(&path, files)?;
        } else if file_type.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        {
            files.push(path);
        }
    }
    Ok(())
}

fn task_spec_file_status(workspace_root: &Path, path: &Path) -> TaskSpecFileStatus {
    let relative = relative_path(workspace_root, path).unwrap_or_else(|| path.to_path_buf());
    match fs::read_to_string(path) {
        Ok(content) => {
            let validation = validate_task_spec_markdown(&content);
            TaskSpecFileStatus {
                path: relative,
                valid: validation.is_valid(),
                missing_sections: validation.missing_sections,
                warnings: Vec::new(),
                errors: Vec::new(),
            }
        }
        Err(err) => TaskSpecFileStatus {
            path: relative,
            valid: false,
            missing_sections: Vec::new(),
            warnings: Vec::new(),
            errors: vec![format!("read_failed: {err}")],
        },
    }
}

fn relative_path(root: &Path, path: &Path) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let path = path.canonicalize().ok()?;
    path.strip_prefix(root).ok().map(PathBuf::from)
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn git_run(dir: &Path, args: &[&str]) -> Result<()> {
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
    Ok(())
}

fn git_run_with_file_protocol(dir: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("-c")
        .arg("protocol.file.allow=always")
        .args(args)
        .output()
        .with_context(|| format!("failed to run git in {}", dir.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{}", stderr.trim());
    }
    Ok(())
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

#[cfg(test)]
mod tests;
