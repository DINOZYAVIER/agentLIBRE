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
    pub profile_file: Option<PathBuf>,
    pub dry_run: bool,
    pub force: bool,
}

impl Default for RepoInitOptions {
    fn default() -> Self {
        Self {
            profile: DEFAULT_PROFILE.to_string(),
            profile_file: None,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoExportProfileOptions {
    pub out: PathBuf,
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
    DeclaredGitComponent,
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
    WouldReplaceUnmanaged,
    ReplacedUnmanaged,
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
pub struct WorkspaceProfile {
    pub version: u32,
    pub name: String,
    pub components: BTreeMap<String, WorkspaceComponent>,
    #[serde(default)]
    pub policy: WorkspaceProfilePolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_pack: Option<WorkspaceSkillPackIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceProfilePolicy {
    #[serde(default)]
    pub hooks: WorkspaceHookPolicy,
    #[serde(default)]
    pub trust: WorkspaceTrustPolicy,
}

impl Default for WorkspaceProfilePolicy {
    fn default() -> Self {
        Self {
            hooks: WorkspaceHookPolicy::default(),
            trust: WorkspaceTrustPolicy::default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceHookPolicy {
    pub managed: bool,
    pub install: Vec<String>,
}

impl Default for WorkspaceHookPolicy {
    fn default() -> Self {
        Self {
            managed: true,
            install: vec!["pre-commit".to_string(), "pre-push".to_string()],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceTrustPolicy {
    pub import_local_trust: bool,
}

impl Default for WorkspaceTrustPolicy {
    fn default() -> Self {
        Self {
            import_local_trust: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSkillPackIdentity {
    pub component: String,
    pub path: PathBuf,
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
    pub same_ids_when_pinned: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RepoExportProfileReport {
    pub workspace_root: PathBuf,
    pub profile_path: PathBuf,
    pub wrote: bool,
    pub profile: WorkspaceProfile,
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
    WorkspaceProfile {
        version: manifest.version,
        name: manifest.profile.clone(),
        components: manifest.components.clone(),
        policy: WorkspaceProfilePolicy::default(),
        skill_pack: workspace_skill_pack_identity(manifest, status),
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
    if !errors.is_empty() {
        bail!("workspace profile is invalid: {}", errors.join(", "));
    }
    Ok(())
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
    fn init_can_apply_local_workspace_profile_file() {
        let root = temp_root("profile-file");
        let profile_path = root.join("portable-profile.toml");
        fs::write(
            &profile_path,
            r#"
version = 1
name = "portable-repo-workflow"

[components.skills]
path = ".agl/skills"
kind = "submodule"
url = "git@example.com:agentlibre/agl-skills.git"
rev = "v0.2.0"
lock = ".agl/skills.lock"

[components.tasks]
path = ".agl/tasks"
kind = "git"
url = "git@example.com:agentlibre/tasks.git"
rev = "main"

[components.reviews]
path = ".agl/reviews"
kind = "submodule"
url = "git@example.com:agentlibre/reviews.git"
rev = "main"

[components.state]
path = ".agl/state"
kind = "ignored"
"#,
        )
        .unwrap();

        let report = init_repo_workspace(
            &root,
            &RepoInitOptions {
                profile: DEFAULT_PROFILE.to_string(),
                profile_file: Some(profile_path),
                dry_run: false,
                force: false,
            },
        )
        .unwrap();
        let manifest = read_manifest(&root.join(WORKSPACE_MANIFEST_PATH)).unwrap();

        assert_eq!(manifest.profile, "portable-repo-workflow");
        assert_eq!(manifest.components["tasks"].kind, ComponentKind::Git);
        assert_eq!(
            manifest.components["reviews"].kind,
            ComponentKind::Submodule
        );
        assert!(root.join(".agl/state").is_dir());
        assert!(!root.join(".agl/tasks").exists());
        assert!(report.changes.iter().any(|change| {
            change.path == PathBuf::from(".agl/tasks")
                && change.action == RepoInitAction::DeclaredGitComponent
        }));
        assert!(report.changes.iter().any(|change| {
            change.path == PathBuf::from(".agl/reviews")
                && change.action == RepoInitAction::DeclaredSubmodule
        }));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn export_profile_writes_policy_and_excludes_local_state() {
        let root = temp_root("export-profile");
        init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
        fs::write(
            root.join(".agl/skill-trust.toml"),
            "SECRET_LOCAL_TRUST_SHOULD_NOT_EXPORT",
        )
        .unwrap();
        fs::write(
            root.join(".agl/state/cache"),
            "SECRET_STATE_SHOULD_NOT_EXPORT",
        )
        .unwrap();
        let out = root.join("repo-workflow.toml");

        let report = export_repo_profile(
            &root,
            &RepoExportProfileOptions {
                out: out.clone(),
                force: false,
            },
        )
        .unwrap();
        let content = fs::read_to_string(&out).unwrap();
        let profile = read_workspace_profile(&out).unwrap();

        assert!(report.wrote);
        assert_eq!(profile.name, DEFAULT_PROFILE);
        assert!(profile.components.contains_key("skills"));
        assert!(profile.policy.hooks.managed);
        assert_eq!(
            profile.policy.hooks.install,
            vec!["pre-commit".to_string(), "pre-push".to_string()]
        );
        assert!(!profile.policy.trust.import_local_trust);
        assert!(
            profile
                .skill_pack
                .as_ref()
                .is_some_and(|identity| identity.same_ids_when_pinned)
        );
        assert!(!content.contains("SECRET_LOCAL_TRUST_SHOULD_NOT_EXPORT"));
        assert!(!content.contains("SECRET_STATE_SHOULD_NOT_EXPORT"));

        let overwrite = export_repo_profile(&root, &RepoExportProfileOptions { out, force: false })
            .unwrap_err();
        assert!(
            overwrite
                .to_string()
                .contains("failed to create profile export")
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn profile_file_name_must_match_requested_non_default_profile() {
        let root = temp_root("profile-name-mismatch");
        let profile_path = root.join("profile.toml");
        fs::write(
            &profile_path,
            r#"
version = 1
name = "actual-profile"

[components.state]
path = ".agl/state"
kind = "ignored"
"#,
        )
        .unwrap();

        let err = init_repo_workspace(
            &root,
            &RepoInitOptions {
                profile: "requested-profile".to_string(),
                profile_file: Some(profile_path),
                dry_run: false,
                force: false,
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("does not match requested profile"));

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
        assert_eq!(
            report.hooks[0].action,
            HookInstallAction::Conflict,
            "pre-commit should report conflict"
        );
        assert_eq!(
            report.hooks[1].action,
            HookInstallAction::WouldInstall,
            "pre-push should be planned but not written when another hook conflicts"
        );
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains("hook_conflict"))
        );
        assert!(
            !hooks.join("pre-push").exists(),
            "hook install must be atomic when conflicts are present"
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn install_hooks_labels_unmanaged_force_replacement() {
        let root = temp_root("hooks-force-unmanaged");
        let hooks = root.join(".git/hooks");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(hooks.join("pre-commit"), "#!/bin/sh\nexit 0\n").unwrap();

        let report = install_repo_hooks(
            &root,
            &RepoHooksOptions {
                dry_run: true,
                force: true,
            },
        )
        .unwrap();

        assert!(!report.has_errors());
        assert_eq!(
            report.hooks[0].action,
            HookInstallAction::WouldReplaceUnmanaged
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_hook_checks_agl_binary_before_running() {
        let content = hook_content("pre-commit");

        assert!(content.contains("command -v \"$AGL_BIN\""));
        assert!(content.contains("agentLIBRE hook error"));
        assert!(content.contains("\"$AGL_BIN\" status --strict"));
        assert!(content.contains("\"$AGL_BIN\" skill verify"));
    }
}
