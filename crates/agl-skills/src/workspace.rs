use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agl_repo::{
    ComponentKind, ComponentState, ComponentStatus, RepoStatusOptions, status_repo_workspace,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    RegisteredSkill, SkillHarness, SkillRegistry, SkillSource, SkillTrustState, builtin_registry,
};

const SKILLS_COMPONENT: &str = "skills";
const SKILLS_LOCK_PATH: &str = ".agl/skills.lock";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SkillLockOptions {
    pub dry_run: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkspaceSkillReport {
    pub state: SkillReportState,
    pub workspace_root: PathBuf,
    pub component: Option<ComponentStatus>,
    pub lock_path: PathBuf,
    pub skills: Vec<WorkspaceSkillStatus>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub next_steps: Vec<String>,
}

impl WorkspaceSkillReport {
    pub fn should_fail(&self, strict: bool) -> bool {
        !self.errors.is_empty() || (strict && !self.warnings.is_empty())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillReportState {
    Ok,
    Warning,
    Invalid,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkspaceSkillStatus {
    pub name: Option<String>,
    pub path: PathBuf,
    pub source_path: Option<String>,
    pub description: Option<String>,
    pub version: Option<u64>,
    pub source: Option<String>,
    pub pack: Option<String>,
    pub valid: bool,
    pub usable: bool,
    pub shadowed_by_builtin: bool,
    pub trust_state: SkillTrustState,
    #[serde(skip_serializing)]
    pub harness: Option<SkillHarness>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkillLockReport {
    pub workspace_root: PathBuf,
    pub lock_path: PathBuf,
    pub dry_run: bool,
    pub wrote: bool,
    pub lock: Option<SkillsLockFile>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl SkillLockReport {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillsLockFile {
    pub version: u32,
    pub locked_at: String,
    #[serde(default)]
    pub skills: Vec<LockedSkill>,
    pub components: BTreeMap<String, LockedComponent>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedComponent {
    pub path: PathBuf,
    pub kind: ComponentKind,
    pub remote: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub commit: String,
    pub tree: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedSkill {
    pub name: String,
    pub source: String,
    pub path: PathBuf,
    pub component: String,
    pub locked_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillTrustOptions {
    pub approve: bool,
    pub agentlibre_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkillTrustUpdateReport {
    pub workspace_root: PathBuf,
    pub trust_store_path: PathBuf,
    pub skill_name: String,
    pub action: SkillTrustAction,
    pub dry_run: bool,
    pub wrote: bool,
    pub record: Option<TrustedSkillRecord>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl SkillTrustUpdateReport {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillTrustAction {
    NeedsApproval,
    Trusted,
    Revoked,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillTrustStore {
    pub version: u32,
    #[serde(default)]
    pub records: Vec<TrustedSkillRecord>,
}

impl Default for SkillTrustStore {
    fn default() -> Self {
        Self {
            version: 1,
            records: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedSkillRecord {
    pub skill_name: String,
    pub source: String,
    pub workspace_root: PathBuf,
    pub remote: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub commit: String,
    pub tree: String,
    pub approved_at: String,
    pub agentlibre_version: String,
    #[serde(default)]
    pub revoked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<String>,
}

pub fn workspace_skill_report(start: impl AsRef<Path>) -> Result<WorkspaceSkillReport> {
    let status = status_repo_workspace(
        start,
        &RepoStatusOptions {
            component: Some(SKILLS_COMPONENT.to_string()),
            strict: false,
        },
    )?;
    let workspace_root = status.workspace_root.clone();
    let lock_path = workspace_root.join(SKILLS_LOCK_PATH);
    let component = status.components.into_iter().next();
    let mut warnings = status.warnings;
    let mut errors = status.errors;

    if component.is_none() {
        errors.push("skills_component_missing".to_string());
    }

    let mut skills = if let Some(component) = &component {
        let component_path = workspace_root.join(&component.path);
        if component_path.is_dir() {
            discover_workspace_skills(&workspace_root, component)?
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    mark_duplicate_skills(&mut skills);
    mark_builtin_shadows(&mut skills)?;

    let component_usable = component.as_ref().is_some_and(component_git_usable);
    for skill in &mut skills {
        if skill.valid && !component_usable {
            skill.warnings.push("component_not_usable".to_string());
        }
        if skill.shadowed_by_builtin {
            skill.warnings.push("shadowed_by_builtin".to_string());
        }
        if !skill.errors.is_empty() {
            errors.extend(skill_error_keys(skill));
        }
    }

    append_lock_diagnostics(
        component.as_ref(),
        &skills,
        &lock_path,
        &mut warnings,
        &mut errors,
    );

    let mut report = WorkspaceSkillReport {
        state: SkillReportState::Ok,
        workspace_root,
        component,
        lock_path,
        skills,
        warnings,
        errors,
        next_steps: Vec::new(),
    };
    let empty_store = SkillTrustStore::default();
    apply_trust_store(&mut report, &empty_store);

    report.state = if !report.errors.is_empty() {
        SkillReportState::Invalid
    } else if !report.warnings.is_empty()
        || report.skills.iter().any(|skill| !skill.warnings.is_empty())
    {
        SkillReportState::Warning
    } else {
        SkillReportState::Ok
    };

    report.next_steps = workspace_skill_next_steps(&report);

    Ok(report)
}

pub fn workspace_skill_report_with_trust(
    start: impl AsRef<Path>,
    trust_store_path: impl AsRef<Path>,
) -> Result<WorkspaceSkillReport> {
    let mut report = workspace_skill_report(start)?;
    let store = read_trust_store(trust_store_path.as_ref())?;
    apply_trust_store(&mut report, &store);
    Ok(report)
}

pub fn trusted_workspace_registry(
    start: impl AsRef<Path>,
    trust_store_path: impl AsRef<Path>,
) -> Result<SkillRegistry> {
    let report = workspace_skill_report_with_trust(start, trust_store_path)?;
    let mut registry = builtin_registry()?;
    for skill in report
        .skills
        .into_iter()
        .filter(|skill| skill.usable && skill.trust_state == SkillTrustState::TrustedLocal)
    {
        let Some(harness) = skill.harness else {
            continue;
        };
        registry.register(RegisteredSkill {
            harness,
            trust: SkillTrustState::TrustedLocal,
        })?;
    }
    Ok(registry)
}

pub fn lock_workspace_skills(
    start: impl AsRef<Path>,
    options: &SkillLockOptions,
) -> Result<SkillLockReport> {
    let report = workspace_skill_report(start)?;
    let mut errors = Vec::new();

    let Some(component) = report.component.as_ref() else {
        errors.push("skills_component_missing".to_string());
        return lock_error_report(report, options, errors);
    };

    if !component_git_usable(component) {
        errors.push("skills_component_not_usable".to_string());
    }

    let valid_skills = report
        .skills
        .iter()
        .filter(|skill| skill.valid && !skill.shadowed_by_builtin)
        .collect::<Vec<_>>();
    if valid_skills.is_empty() {
        errors.push("no_valid_workspace_skills".to_string());
    }
    for skill in &report.skills {
        if !skill.valid {
            errors.extend(skill_error_keys(skill));
        }
    }

    if !errors.is_empty() {
        return lock_error_report(report, options, errors);
    }

    let existing = read_skills_lock(&report.lock_path).ok();
    let locked_at = existing
        .as_ref()
        .map(|lock| lock.locked_at.clone())
        .unwrap_or_else(lock_timestamp);
    let lock = build_skills_lock(component, &valid_skills, existing.as_ref(), locked_at)?;
    let content = toml::to_string_pretty(&lock).context("failed to render skills lock")?;
    let existing_content = fs::read_to_string(&report.lock_path).ok();
    let wrote = existing_content.as_deref() != Some(content.as_str()) && !options.dry_run;

    if wrote {
        if let Some(parent) = report.lock_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
        fs::write(&report.lock_path, content)
            .with_context(|| format!("failed to write {}", report.lock_path.display()))?;
    }

    let warnings = if options.dry_run {
        report.warnings
    } else {
        report
            .warnings
            .into_iter()
            .filter(|warning| warning != "skills_lock_missing")
            .collect()
    };

    Ok(SkillLockReport {
        workspace_root: report.workspace_root,
        lock_path: report.lock_path,
        dry_run: options.dry_run,
        wrote,
        lock: Some(lock),
        warnings,
        errors: Vec::new(),
    })
}

pub fn trust_workspace_skill(
    start: impl AsRef<Path>,
    trust_store_path: impl AsRef<Path>,
    name: &str,
    options: &SkillTrustOptions,
) -> Result<SkillTrustUpdateReport> {
    let report = workspace_skill_report(start)?;
    let trust_store_path = trust_store_path.as_ref().to_path_buf();
    let mut errors = Vec::new();
    let skill = find_trust_target(&report, name, &mut errors);
    let record = skill
        .and_then(|skill| build_trust_record(&report, skill, &options.agentlibre_version).ok());

    if report.should_fail(true) {
        errors.extend(report.errors.iter().cloned());
        errors.extend(report.warnings.iter().cloned());
    }
    if record.is_none() {
        errors.push("trust_identity_unavailable".to_string());
    }
    if !options.approve {
        errors.push("approval_required".to_string());
    }
    if !errors.is_empty() {
        return Ok(SkillTrustUpdateReport {
            workspace_root: report.workspace_root,
            trust_store_path,
            skill_name: name.to_string(),
            action: SkillTrustAction::NeedsApproval,
            dry_run: true,
            wrote: false,
            record,
            warnings: report.warnings,
            errors,
        });
    }

    let record = record.expect("record checked above");
    validate_trust_target_tools(skill.expect("skill checked above"))?;
    let mut store = read_trust_store(&trust_store_path)?;
    upsert_trust_record(&mut store, record.clone());
    write_trust_store(&trust_store_path, &store)?;

    Ok(SkillTrustUpdateReport {
        workspace_root: report.workspace_root,
        trust_store_path,
        skill_name: name.to_string(),
        action: SkillTrustAction::Trusted,
        dry_run: false,
        wrote: true,
        record: Some(record),
        warnings: report.warnings,
        errors: Vec::new(),
    })
}

pub fn revoke_workspace_skill(
    start: impl AsRef<Path>,
    trust_store_path: impl AsRef<Path>,
    name: &str,
) -> Result<SkillTrustUpdateReport> {
    let report = workspace_skill_report(start)?;
    let trust_store_path = trust_store_path.as_ref().to_path_buf();
    let mut errors = Vec::new();
    let skill = find_trust_target(&report, name, &mut errors);
    let record = skill.and_then(|skill| build_trust_record(&report, skill, "").ok());
    if record.is_none() {
        errors.push("trust_identity_unavailable".to_string());
    }
    if !errors.is_empty() {
        return Ok(SkillTrustUpdateReport {
            workspace_root: report.workspace_root,
            trust_store_path,
            skill_name: name.to_string(),
            action: SkillTrustAction::Revoked,
            dry_run: false,
            wrote: false,
            record,
            warnings: report.warnings,
            errors,
        });
    }

    let identity = record.expect("record checked above");
    let mut store = read_trust_store(&trust_store_path)?;
    let revoked = revoke_trust_record(&mut store, &identity);
    if revoked.is_none() {
        errors.push("trust_record_not_found".to_string());
        return Ok(SkillTrustUpdateReport {
            workspace_root: report.workspace_root,
            trust_store_path,
            skill_name: name.to_string(),
            action: SkillTrustAction::Revoked,
            dry_run: false,
            wrote: false,
            record: Some(identity),
            warnings: report.warnings,
            errors,
        });
    }
    write_trust_store(&trust_store_path, &store)?;
    let record = revoked.expect("revoked record checked above");

    Ok(SkillTrustUpdateReport {
        workspace_root: report.workspace_root,
        trust_store_path,
        skill_name: name.to_string(),
        action: SkillTrustAction::Revoked,
        dry_run: false,
        wrote: true,
        record: Some(record),
        warnings: report.warnings,
        errors: Vec::new(),
    })
}

fn lock_error_report(
    report: WorkspaceSkillReport,
    options: &SkillLockOptions,
    errors: Vec<String>,
) -> Result<SkillLockReport> {
    Ok(SkillLockReport {
        workspace_root: report.workspace_root,
        lock_path: report.lock_path,
        dry_run: options.dry_run,
        wrote: false,
        lock: None,
        warnings: report.warnings,
        errors,
    })
}

fn discover_workspace_skills(
    workspace_root: &Path,
    component: &ComponentStatus,
) -> Result<Vec<WorkspaceSkillStatus>> {
    let component_root = workspace_root.join(&component.path);
    let canonical_component_root = component_root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", component_root.display()))?;
    let mut manifests = Vec::new();
    collect_skill_manifests(&component_root, &mut manifests)?;
    manifests.sort();

    let tree = component.actual_tree.as_deref().unwrap_or("unknown");
    let mut skills = Vec::with_capacity(manifests.len());
    for manifest in manifests {
        let skill_dir = manifest.parent().unwrap_or(&component_root);
        let path = match skill_dir.canonicalize() {
            Ok(path) if path.starts_with(&canonical_component_root) => {
                relative_path(workspace_root, &path).unwrap_or_else(|| component.path.clone())
            }
            Ok(_) => {
                skills.push(invalid_skill_status(
                    workspace_root,
                    skill_dir,
                    "skill_path_escapes_component",
                ));
                continue;
            }
            Err(err) => {
                skills.push(invalid_skill_status(
                    workspace_root,
                    skill_dir,
                    &format!("skill_path_unavailable: {err}"),
                ));
                continue;
            }
        };

        match SkillHarness::parse_workspace_dir(skill_dir, &component_root, tree) {
            Ok(harness) => skills.push(status_from_harness(path, harness)),
            Err(err) => {
                let mut status = invalid_skill_status(workspace_root, skill_dir, &err.to_string());
                status.source_path = Some(slash_path(
                    skill_dir
                        .join("SKILL.md")
                        .strip_prefix(&component_root)
                        .unwrap_or(&skill_dir.join("SKILL.md")),
                ));
                skills.push(status);
            }
        }
    }

    Ok(skills)
}

fn collect_skill_manifests(dir: &Path, manifests: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();
        if file_name == ".git" {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_file() && file_name == "SKILL.md" {
            manifests.push(path);
        } else if file_type.is_dir() {
            collect_skill_manifests(&path, manifests)?;
        }
    }
    Ok(())
}

fn status_from_harness(path: PathBuf, harness: SkillHarness) -> WorkspaceSkillStatus {
    WorkspaceSkillStatus {
        name: Some(harness.name.clone()),
        path,
        source_path: Some(harness.source_path.clone()),
        description: Some(harness.description.clone()),
        version: Some(harness.version),
        source: Some(harness.source.as_str().to_string()),
        pack: Some(harness.pack.clone()),
        valid: true,
        usable: false,
        shadowed_by_builtin: false,
        trust_state: SkillTrustState::Unknown,
        harness: Some(harness),
        warnings: Vec::new(),
        errors: Vec::new(),
    }
}

fn invalid_skill_status(
    workspace_root: &Path,
    skill_dir: &Path,
    error: &str,
) -> WorkspaceSkillStatus {
    WorkspaceSkillStatus {
        name: None,
        path: relative_path(workspace_root, skill_dir)
            .unwrap_or_else(|| PathBuf::from(skill_dir.file_name().unwrap_or_default())),
        source_path: None,
        description: None,
        version: None,
        source: None,
        pack: None,
        valid: false,
        usable: false,
        shadowed_by_builtin: false,
        trust_state: SkillTrustState::Invalid,
        harness: None,
        warnings: Vec::new(),
        errors: vec![error.to_string()],
    }
}

fn mark_duplicate_skills(skills: &mut [WorkspaceSkillStatus]) {
    let mut counts = BTreeMap::<String, usize>::new();
    for name in skills.iter().filter_map(|skill| skill.name.as_ref()) {
        *counts.entry(name.clone()).or_default() += 1;
    }
    for skill in skills {
        if let Some(name) = &skill.name
            && counts.get(name).copied().unwrap_or_default() > 1
        {
            skill.valid = false;
            skill.errors.push(format!("duplicate_skill_name: {name}"));
        }
    }
}

fn mark_builtin_shadows(skills: &mut [WorkspaceSkillStatus]) -> Result<()> {
    let builtin_names = builtin_registry()?
        .skills()
        .iter()
        .map(|skill| skill.harness.name.clone())
        .collect::<BTreeSet<_>>();
    for skill in skills {
        if skill
            .name
            .as_ref()
            .is_some_and(|name| builtin_names.contains(name))
        {
            skill.shadowed_by_builtin = true;
        }
    }
    Ok(())
}

fn append_lock_diagnostics(
    component: Option<&ComponentStatus>,
    skills: &[WorkspaceSkillStatus],
    lock_path: &Path,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) {
    if !lock_path.exists() {
        warnings.push("skills_lock_missing".to_string());
        return;
    }

    let lock = match read_skills_lock(lock_path) {
        Ok(lock) => lock,
        Err(err) => {
            errors.push(format!("skills_lock_invalid: {err:#}"));
            return;
        }
    };

    let Some(component) = component else {
        errors.push("skills_lock_without_component".to_string());
        return;
    };
    let Some(locked_component) = lock.components.get(SKILLS_COMPONENT) else {
        errors.push("skills_lock_component_missing".to_string());
        return;
    };

    if Some(&locked_component.remote) != component.actual_url.as_ref() {
        errors.push("skills_lock_remote_mismatch".to_string());
    }
    if Some(&locked_component.commit) != component.actual_commit.as_ref() {
        errors.push("skills_lock_commit_mismatch".to_string());
    }
    if Some(&locked_component.tree) != component.actual_tree.as_ref() {
        errors.push("skills_lock_tree_mismatch".to_string());
    }

    let locked_skills = lock
        .skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect::<BTreeSet<_>>();
    for skill in skills.iter().filter(|skill| skill.valid) {
        if let Some(name) = &skill.name
            && !locked_skills.contains(name.as_str())
        {
            errors.push(format!("skills_lock_entry_missing: {name}"));
        }
    }
}

fn build_skills_lock(
    component: &ComponentStatus,
    skills: &[&WorkspaceSkillStatus],
    existing: Option<&SkillsLockFile>,
    locked_at: String,
) -> Result<SkillsLockFile> {
    if component.kind != ComponentKind::Submodule {
        bail!("skills component must be a submodule");
    }
    let remote = component
        .actual_url
        .clone()
        .context("skills component has no origin remote")?;
    let commit = component
        .actual_commit
        .clone()
        .context("skills component has no HEAD commit")?;
    let tree = component
        .actual_tree
        .clone()
        .context("skills component has no HEAD tree")?;
    let ref_name = component
        .expected_rev
        .clone()
        .unwrap_or_else(|| "HEAD".to_string());

    let existing_timestamps = existing
        .map(|lock| {
            lock.skills
                .iter()
                .map(|skill| (skill.name.clone(), skill.locked_at.clone()))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();

    let mut locked_skills = skills
        .iter()
        .filter_map(|skill| {
            let name = skill.name.clone()?;
            Some(LockedSkill {
                locked_at: existing_timestamps
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| locked_at.clone()),
                name,
                source: SkillSource::Workspace.as_str().to_string(),
                path: skill.path.clone(),
                component: SKILLS_COMPONENT.to_string(),
            })
        })
        .collect::<Vec<_>>();
    locked_skills.sort_by(|left, right| left.name.cmp(&right.name));

    Ok(SkillsLockFile {
        version: 1,
        locked_at,
        skills: locked_skills,
        components: BTreeMap::from([(
            SKILLS_COMPONENT.to_string(),
            LockedComponent {
                path: component.path.clone(),
                kind: component.kind,
                remote,
                ref_name,
                commit,
                tree,
            },
        )]),
    })
}

fn read_skills_lock(path: &Path) -> Result<SkillsLockFile> {
    let content = fs::read_to_string(path)?;
    toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
}

fn read_trust_store(path: &Path) -> Result<SkillTrustStore> {
    match fs::read_to_string(path) {
        Ok(content) => {
            toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(SkillTrustStore::default()),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}

fn write_trust_store(path: &Path, store: &SkillTrustStore) -> Result<()> {
    let content = toml::to_string_pretty(store).context("failed to render skill trust store")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))
}

fn apply_trust_store(report: &mut WorkspaceSkillReport, store: &SkillTrustStore) {
    for index in 0..report.skills.len() {
        let state = classify_trust_state(report, &report.skills[index], store);
        report.skills[index].trust_state = state;
        report.skills[index].usable = state.permits_context_injection()
            && report.skills[index].valid
            && !report.skills[index].shadowed_by_builtin;
    }
}

fn classify_trust_state(
    report: &WorkspaceSkillReport,
    skill: &WorkspaceSkillStatus,
    store: &SkillTrustStore,
) -> SkillTrustState {
    if !skill.valid {
        return SkillTrustState::Invalid;
    }
    if skill.shadowed_by_builtin {
        return SkillTrustState::Unsupported;
    }
    if report
        .errors
        .iter()
        .any(|error| error.contains("remote_mismatch"))
    {
        return SkillTrustState::RemoteMismatch;
    }
    if report
        .errors
        .iter()
        .any(|error| error.contains("commit_mismatch") || error.contains("tree_mismatch"))
    {
        return SkillTrustState::RevMismatch;
    }
    if report
        .errors
        .iter()
        .any(|error| error.contains("dirty_worktree"))
    {
        return SkillTrustState::DirtyWorkingTree;
    }
    if report
        .errors
        .iter()
        .any(|error| error.contains("untracked_content"))
    {
        return SkillTrustState::UntrackedContent;
    }
    if report.should_fail(true) {
        return SkillTrustState::Unsupported;
    }

    let Ok(identity) = build_trust_record(report, skill, "") else {
        return SkillTrustState::Unknown;
    };
    let same_identity = store
        .records
        .iter()
        .find(|record| trust_identity_matches(record, &identity));
    if let Some(record) = same_identity {
        if record.revoked {
            SkillTrustState::Revoked
        } else {
            SkillTrustState::TrustedLocal
        }
    } else if store.records.iter().any(|record| {
        record.skill_name == identity.skill_name
            && record.source == identity.source
            && record.workspace_root == identity.workspace_root
    }) {
        SkillTrustState::Changed
    } else {
        SkillTrustState::Unknown
    }
}

fn find_trust_target<'a>(
    report: &'a WorkspaceSkillReport,
    name: &str,
    errors: &mut Vec<String>,
) -> Option<&'a WorkspaceSkillStatus> {
    let matches = report
        .skills
        .iter()
        .filter(|skill| skill.name.as_deref() == Some(name))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [skill] => {
            if !skill.valid {
                errors.push("skill_invalid".to_string());
            }
            if skill.shadowed_by_builtin {
                errors.push("skill_shadowed_by_builtin".to_string());
            }
            Some(*skill)
        }
        [] => {
            errors.push(format!("skill_not_found: {name}"));
            None
        }
        _ => {
            errors.push(format!("skill_name_ambiguous: {name}"));
            None
        }
    }
}

fn build_trust_record(
    report: &WorkspaceSkillReport,
    skill: &WorkspaceSkillStatus,
    agentlibre_version: &str,
) -> Result<TrustedSkillRecord> {
    let component = report
        .component
        .as_ref()
        .context("skills component is missing")?;
    let name = skill.name.clone().context("skill name is missing")?;
    Ok(TrustedSkillRecord {
        skill_name: name,
        source: SkillSource::Workspace.as_str().to_string(),
        workspace_root: report.workspace_root.clone(),
        remote: component
            .actual_url
            .clone()
            .context("skills component has no origin remote")?,
        ref_name: component
            .expected_rev
            .clone()
            .unwrap_or_else(|| "HEAD".to_string()),
        commit: component
            .actual_commit
            .clone()
            .context("skills component has no HEAD commit")?,
        tree: component
            .actual_tree
            .clone()
            .context("skills component has no HEAD tree")?,
        approved_at: lock_timestamp(),
        agentlibre_version: agentlibre_version.to_string(),
        revoked: false,
        revoked_at: None,
    })
}

fn validate_trust_target_tools(skill: &WorkspaceSkillStatus) -> Result<()> {
    let harness = skill.harness.as_ref().context("skill harness is missing")?;
    let mut catalog = agl_tools::ToolCatalog::new();
    agl_tools::guards::register(&mut catalog)
        .context("failed to register builtin guard provider")?;
    agl_tools::fs::register(&mut catalog).context("failed to register builtin tool provider")?;
    for hook in &harness.required_hooks {
        if catalog.hook(hook).is_none() {
            bail!("skill `{}` requires missing hook `{hook}`", harness.name);
        }
    }
    for tool in &harness.allowed_tools {
        if catalog.tool(tool).is_none() {
            bail!("skill `{}` allows missing tool `{tool}`", harness.name);
        }
    }
    Ok(())
}

fn upsert_trust_record(store: &mut SkillTrustStore, record: TrustedSkillRecord) {
    if let Some(existing) = store
        .records
        .iter_mut()
        .find(|existing| trust_identity_matches(existing, &record))
    {
        *existing = record;
    } else {
        store.records.push(record);
        store.records.sort_by(|left, right| {
            left.workspace_root
                .cmp(&right.workspace_root)
                .then_with(|| left.skill_name.cmp(&right.skill_name))
                .then_with(|| left.commit.cmp(&right.commit))
                .then_with(|| left.tree.cmp(&right.tree))
        });
    }
}

fn revoke_trust_record(
    store: &mut SkillTrustStore,
    identity: &TrustedSkillRecord,
) -> Option<TrustedSkillRecord> {
    let mut revoked = None;
    let revoked_at = lock_timestamp();
    for record in &mut store.records {
        if trust_identity_matches(record, identity) {
            record.revoked = true;
            record.revoked_at = Some(revoked_at.clone());
            revoked = Some(record.clone());
        }
    }
    revoked
}

fn trust_identity_matches(left: &TrustedSkillRecord, right: &TrustedSkillRecord) -> bool {
    left.skill_name == right.skill_name
        && left.source == right.source
        && left.workspace_root == right.workspace_root
        && left.remote == right.remote
        && left.ref_name == right.ref_name
        && left.commit == right.commit
        && left.tree == right.tree
}

fn component_git_usable(component: &ComponentStatus) -> bool {
    component.kind == ComponentKind::Submodule
        && component.exists
        && component.state == ComponentState::Ok
        && component.submodule_registered == Some(true)
        && component.gitlink_present == Some(true)
        && component.tracked_dirty == Some(false)
        && component.untracked_suspicious == Some(false)
        && component.actual_url.is_some()
        && component.actual_commit.is_some()
        && component.actual_tree.is_some()
}

fn skill_error_keys(skill: &WorkspaceSkillStatus) -> Vec<String> {
    let label = skill
        .name
        .clone()
        .unwrap_or_else(|| skill.path.display().to_string());
    skill
        .errors
        .iter()
        .map(|error| format!("skill.{label}.{error}"))
        .collect()
}

fn workspace_skill_next_steps(report: &WorkspaceSkillReport) -> Vec<String> {
    let mut next_steps = Vec::new();
    if report
        .warnings
        .iter()
        .any(|warning| warning == "skills_lock_missing")
    {
        next_steps.push("agl skill lock".to_string());
    }
    if report.errors.iter().any(|error| {
        error.contains("skills_lock_commit_mismatch")
            || error.contains("skills_lock_tree_mismatch")
            || error.contains("skills_lock_remote_mismatch")
            || error.contains("skills_lock_entry_missing")
    }) {
        next_steps.push("review .agl/skills and run agl skill lock".to_string());
    }
    if report
        .errors
        .iter()
        .any(|error| error.contains("dirty_worktree") || error.contains("untracked_content"))
    {
        next_steps.push("clean .agl/skills worktree".to_string());
    }
    if report
        .errors
        .iter()
        .any(|error| error.contains("remote_mismatch"))
    {
        next_steps.push("restore .agl/skills remote URL".to_string());
    }
    if report.warnings.iter().any(|warning| {
        matches!(
            warning.as_str(),
            "component.skills.missing"
                | "component.skills.not_registered_submodule"
                | "component.skills.gitlink_missing"
        )
    }) || report.errors.iter().any(|error| {
        error.contains("not_component_git_worktree") || error.contains("remote_missing")
    }) {
        next_steps.push("initialize .agl/skills submodule".to_string());
    }
    if !report.errors.is_empty() {
        next_steps.push("agl skill status --json".to_string());
    }
    next_steps.dedup();
    next_steps
}

fn relative_path(root: &Path, path: &Path) -> Option<PathBuf> {
    path.strip_prefix(root).ok().map(Path::to_path_buf)
}

fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn lock_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("unix:{seconds}")
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use agl_repo::{DEFAULT_SKILLS_URL, RepoInitOptions, init_repo_workspace};

    use super::*;

    #[test]
    fn plain_skills_dir_is_discovered_but_not_usable() {
        let root = temp_root("plain-skills");
        init_git_repo(&root);
        init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
        let skill_dir = root.join(".agl/skills/agl/repo-change");
        write_workspace_skill(&skill_dir, "repo-change");

        let report = workspace_skill_report(&root).unwrap();

        assert_eq!(report.state, SkillReportState::Invalid);
        assert_eq!(report.skills.len(), 1);
        assert_eq!(report.skills[0].name.as_deref(), Some("repo-change"));
        assert!(report.skills[0].valid);
        assert!(!report.skills[0].usable);
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains("not_component_git_worktree"))
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_workspace_manifest_is_reported() {
        let root = temp_root("invalid-skill");
        init_git_repo(&root);
        init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
        let skill_dir = root.join(".agl/skills/agl/bad-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            r#"---
name: bad-skill
description: Bad.
---
Body.
"#,
        )
        .unwrap();

        let report = workspace_skill_report(&root).unwrap();

        assert_eq!(report.skills.len(), 1);
        assert!(!report.skills[0].valid);
        assert!(
            report.skills[0]
                .errors
                .iter()
                .any(|error| error.contains("missing field"))
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lock_refuses_unusable_component() {
        let root = temp_root("lock-refuses");
        init_git_repo(&root);
        init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
        write_workspace_skill(&root.join(".agl/skills/agl/repo-change"), "repo-change");

        let report = lock_workspace_skills(&root, &SkillLockOptions { dry_run: false }).unwrap();

        assert!(report.has_errors());
        assert!(
            report
                .errors
                .contains(&"skills_component_not_usable".to_string())
        );
        assert!(!report.lock_path.exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lock_roundtrip_accepts_clean_skills_submodule() {
        let (root, source) = clean_skills_submodule_fixture("lock-roundtrip");

        let unlocked = workspace_skill_report(&root).unwrap();
        assert_eq!(unlocked.state, SkillReportState::Warning);
        assert!(
            unlocked
                .warnings
                .contains(&"skills_lock_missing".to_string())
        );
        assert_eq!(unlocked.skills[0].name.as_deref(), Some("repo-change"));
        assert!(!unlocked.skills[0].usable);
        assert_eq!(unlocked.skills[0].trust_state, SkillTrustState::Unsupported);

        let first_lock =
            lock_workspace_skills(&root, &SkillLockOptions { dry_run: false }).unwrap();
        assert!(!first_lock.has_errors());
        assert!(first_lock.wrote);
        assert!(
            !first_lock
                .warnings
                .contains(&"skills_lock_missing".to_string())
        );
        assert!(first_lock.lock_path.exists());

        let locked = workspace_skill_report(&root).unwrap();
        assert_eq!(locked.state, SkillReportState::Ok);
        assert!(!locked.skills[0].usable);
        assert_eq!(locked.skills[0].trust_state, SkillTrustState::Unknown);

        let second_lock =
            lock_workspace_skills(&root, &SkillLockOptions { dry_run: false }).unwrap();
        assert!(!second_lock.has_errors());
        assert!(!second_lock.wrote);

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(source).unwrap();
    }

    #[test]
    fn lock_mismatch_marks_workspace_skills_not_usable() {
        let (root, source) = clean_skills_submodule_fixture("lock-mismatch");
        lock_workspace_skills(&root, &SkillLockOptions { dry_run: false }).unwrap();
        let lock_path = root.join(SKILLS_LOCK_PATH);
        let lock = fs::read_to_string(&lock_path).unwrap();
        let lock = lock
            .lines()
            .map(|line| {
                if line.starts_with("commit = ") {
                    "commit = \"0000000000000000000000000000000000000000\""
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&lock_path, format!("{lock}\n")).unwrap();

        let report = workspace_skill_report(&root).unwrap();

        assert_eq!(report.state, SkillReportState::Invalid);
        assert!(
            report
                .errors
                .contains(&"skills_lock_commit_mismatch".to_string())
        );
        assert_eq!(report.skills[0].trust_state, SkillTrustState::RevMismatch);
        assert!(!report.skills[0].usable);
        assert!(
            !report
                .next_steps
                .contains(&"initialize .agl/skills submodule".to_string())
        );
        assert!(
            report
                .next_steps
                .contains(&"review .agl/skills and run agl skill lock".to_string())
        );

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(source).unwrap();
    }

    #[test]
    fn trust_promotes_changes_and_revokes_locked_workspace_skill() {
        let (root, source) = clean_skills_submodule_fixture("trust");
        lock_workspace_skills(&root, &SkillLockOptions { dry_run: false }).unwrap();
        let trust_store = root.join("state/skill-trust.toml");

        let pending = workspace_skill_report_with_trust(&root, &trust_store).unwrap();
        assert_eq!(pending.skills[0].trust_state, SkillTrustState::Unknown);
        assert!(!pending.skills[0].usable);

        let approval = trust_workspace_skill(
            &root,
            &trust_store,
            "repo-change",
            &SkillTrustOptions {
                approve: true,
                agentlibre_version: "test-version".to_string(),
            },
        )
        .unwrap();
        assert!(!approval.has_errors());
        assert!(approval.wrote);

        let trusted = workspace_skill_report_with_trust(&root, &trust_store).unwrap();
        assert_eq!(trusted.skills[0].trust_state, SkillTrustState::TrustedLocal);
        assert!(trusted.skills[0].usable);

        let registry = trusted_workspace_registry(&root, &trust_store).unwrap();
        let trusted_skill = registry
            .get(&agl_tools::SkillId::new("repo-change").unwrap())
            .expect("trusted workspace skill should be registered");
        assert!(trusted_skill.permits_context_injection());

        let revoke = revoke_workspace_skill(&root, &trust_store, "repo-change").unwrap();
        assert!(!revoke.has_errors());
        assert!(revoke.wrote);
        let record = revoke
            .record
            .expect("revoke should return persisted record");
        assert!(record.revoked);
        assert!(record.revoked_at.is_some());
        assert_eq!(record.agentlibre_version, "test-version");
        let revoked = workspace_skill_report_with_trust(&root, &trust_store).unwrap();
        assert_eq!(revoked.skills[0].trust_state, SkillTrustState::Revoked);
        assert!(!revoked.skills[0].usable);

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(source).unwrap();
    }

    fn clean_skills_submodule_fixture(label: &str) -> (PathBuf, PathBuf) {
        let source = temp_root(&format!("{label}-skills-source"));
        init_git_repo(&source);
        write_workspace_skill(&source.join("agl/repo-change"), "repo-change");
        git_run(&source, ["add", "."]);
        git_run(
            &source,
            [
                "-c",
                "user.name=AgentLIBRE Test",
                "-c",
                "user.email=agentlibre-test@example.invalid",
                "commit",
                "-q",
                "-m",
                "add workspace skill",
            ],
        );

        let root = temp_root(&format!("{label}-skills-submodule"));
        init_git_repo(&root);
        init_repo_workspace(&root, &RepoInitOptions::default()).unwrap();
        git_run(
            &root,
            [
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                source.to_str().unwrap(),
                ".agl/skills",
            ],
        );
        let manifest_path = root.join(agl_repo::WORKSPACE_MANIFEST_PATH);
        let manifest = fs::read_to_string(&manifest_path)
            .unwrap()
            .replace(DEFAULT_SKILLS_URL, source.to_str().unwrap());
        fs::write(&manifest_path, manifest).unwrap();

        (root, source)
    }

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agl-skills-workspace-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
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

    fn git_run<const N: usize>(root: &Path, args: [&str; N]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap_or_else(|err| panic!("failed to run git in {}: {err}", root.display()));
        assert!(
            output.status.success(),
            "git failed in {}\nstdout:\n{}\nstderr:\n{}",
            root.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write_workspace_skill(skill_dir: &Path, name: &str) {
        fs::create_dir_all(skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                r#"---
name: {name}
description: Review repository changes.
version: 1
source: workspace
pack: agl
required_hooks:
  - repo_path.validate
allowed_tools: []
context_budget_tokens: 256
references:
  include: []
guarantees:
  - repository paths are checked
---
Body.
"#
            ),
        )
        .unwrap();
    }
}
