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
    RegisteredSkill, SkillArtifactAccess, SkillArtifactKind, SkillFolderCreateRule,
    SkillFolderCreateSituation, SkillHarness, SkillRegistry, SkillSource, SkillTrustState,
    builtin_registry,
};
use agl_tools::SkillId;

const SKILLS_COMPONENT: &str = "skills";
const SKILLS_LOCK_PATH: &str = ".agl/skills.lock";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SkillLockOptions {
    pub dry_run: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SkillFolderSyncOptions {
    pub dry_run: bool,
    pub situation: SkillFolderCreateSituation,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SkillFolderPrepareOptions {
    pub dry_run: bool,
    pub situation: SkillFolderCreateSituation,
    pub strict: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkspaceSkillReport {
    pub state: SkillReportState,
    pub workspace_root: PathBuf,
    pub component: Option<ComponentStatus>,
    pub lock_path: PathBuf,
    pub skills: Vec<WorkspaceSkillStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<WorkspaceSkillDiagnostic>,
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
pub struct WorkspaceSkillDiagnostic {
    pub severity: WorkspaceSkillDiagnosticSeverity,
    pub scope: WorkspaceSkillDiagnosticScope,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSkillDiagnosticSeverity {
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSkillDiagnosticScope {
    Workspace,
    Component,
    Lock,
    SkillManifest,
    SkillArtifactFolder,
    SkillTrust,
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
    pub allowed_tools: Vec<String>,
    pub requestable_tools: Vec<String>,
    pub denied_tools: Vec<String>,
    pub permission_request_templates: Vec<String>,
    pub memory_read_scopes: Vec<String>,
    pub notes_read: bool,
    pub notes_write: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifact_folders: Vec<SkillArtifactFolderStatus>,
    pub valid: bool,
    pub usable: bool,
    pub shadowed_by_builtin: bool,
    pub overrides_builtin: bool,
    pub broadens_builtin_routing: bool,
    pub trust_state: SkillTrustState,
    #[serde(skip_serializing)]
    pub harness: Option<SkillHarness>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkillArtifactFolderStatus {
    pub id: String,
    pub path: PathBuf,
    pub kind: SkillArtifactKind,
    pub access: SkillArtifactAccess,
    pub create: Vec<SkillFolderCreateRule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readiness: Vec<SkillArtifactFolderReadiness>,
    pub provides: Vec<String>,
    pub schema: Option<String>,
    pub exists: bool,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkillArtifactFolderReadiness {
    pub situation: SkillFolderCreateSituation,
    pub action: SkillFolderSyncActionKind,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkillFolderSyncReport {
    pub workspace_root: PathBuf,
    pub dry_run: bool,
    pub situation: SkillFolderCreateSituation,
    pub actions: Vec<SkillFolderSyncAction>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl SkillFolderSyncReport {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkillFolderSyncAction {
    pub skill: String,
    pub folder_id: String,
    pub path: PathBuf,
    pub kind: SkillArtifactKind,
    pub access: SkillArtifactAccess,
    pub action: SkillFolderSyncActionKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillFolderSyncActionKind {
    Exists,
    SkippedReadOnly,
    SkippedSource,
    SkippedNoCreateRule,
    SkippedSituationMismatch,
    WouldCreateDir,
    CreatedDir,
}

pub type SkillFolderPrepareReport = SkillFolderSyncReport;

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
        if skill.broadens_builtin_routing {
            skill.warnings.push("broadens_builtin_routing".to_string());
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
        diagnostics: Vec::new(),
        warnings,
        errors,
        next_steps: Vec::new(),
    };
    let empty_store = SkillTrustStore::default();
    apply_trust_store(&mut report, &empty_store);
    refresh_workspace_skill_report_derived(&mut report);

    Ok(report)
}

pub fn workspace_skill_report_with_trust(
    start: impl AsRef<Path>,
    trust_store_path: impl AsRef<Path>,
) -> Result<WorkspaceSkillReport> {
    let mut report = workspace_skill_report(start)?;
    let store = read_trust_store(trust_store_path.as_ref())?;
    apply_trust_store(&mut report, &store);
    refresh_workspace_skill_report_derived(&mut report);
    Ok(report)
}

pub fn sync_workspace_skill_folders(
    start: impl AsRef<Path>,
    options: &SkillFolderSyncOptions,
) -> Result<SkillFolderSyncReport> {
    let report = workspace_skill_report(start)?;
    let skill_indexes = report
        .skills
        .iter()
        .enumerate()
        .filter_map(|(index, skill)| skill.valid.then_some(index))
        .collect::<Vec<_>>();
    sync_skill_folder_indexes(&report, &skill_indexes, options, false, false)
}

pub fn prepare_workspace_skill_folders(
    start: impl AsRef<Path>,
    trust_store_path: impl AsRef<Path>,
    selected_skills: &[SkillId],
    options: &SkillFolderPrepareOptions,
) -> Result<SkillFolderPrepareReport> {
    let report = workspace_skill_report_with_trust(start, trust_store_path)?;
    let mut errors = Vec::new();
    let skill_indexes = selected_trusted_workspace_skill_indexes(
        &report,
        selected_skills,
        "runtime_prepare",
        &mut errors,
    )?;
    let sync = sync_skill_folder_indexes(
        &report,
        &skill_indexes,
        &SkillFolderSyncOptions {
            dry_run: options.dry_run,
            situation: options.situation,
        },
        options.strict,
        false,
    )?;
    let SkillFolderSyncReport {
        workspace_root,
        dry_run,
        situation,
        actions,
        warnings,
        errors: sync_errors,
    } = sync;
    errors.extend(sync_errors);
    Ok(SkillFolderSyncReport {
        workspace_root,
        dry_run,
        situation,
        actions,
        warnings,
        errors,
    })
}

pub fn prepare_workspace_skill_artifact_write(
    start: impl AsRef<Path>,
    trust_store_path: impl AsRef<Path>,
    selected_skills: &[SkillId],
    target_relative_path: impl AsRef<Path>,
    options: &SkillFolderPrepareOptions,
) -> Result<SkillFolderPrepareReport> {
    let report = workspace_skill_report_with_trust(start, trust_store_path)?;
    let target_relative_path = target_relative_path.as_ref();
    let mut errors = Vec::new();
    let skill_indexes = selected_trusted_workspace_skill_indexes(
        &report,
        selected_skills,
        "artifact_write",
        &mut errors,
    )?;
    let selected_names = selected_skill_names(&report, &skill_indexes);
    let mut actions = Vec::new();
    for skill_index in skill_indexes {
        let skill = &report.skills[skill_index];
        let skill_name = skill.name.as_deref().unwrap_or("<invalid>");
        for folder in &skill.artifact_folders {
            if !target_relative_path.starts_with(&folder.path) {
                continue;
            }
            let action = apply_skill_folder_action(
                &report.workspace_root,
                skill_name,
                folder,
                &SkillFolderSyncOptions {
                    dry_run: options.dry_run,
                    situation: options.situation,
                },
                options.strict,
                options.strict && !folder.exists,
                &mut errors,
            );
            actions.push(SkillFolderSyncAction {
                skill: skill_name.to_string(),
                folder_id: folder.id.clone(),
                path: folder.path.clone(),
                kind: folder.kind,
                access: folder.access,
                action,
            });
        }
    }

    Ok(SkillFolderSyncReport {
        workspace_root: report.workspace_root,
        dry_run: options.dry_run,
        situation: options.situation,
        actions,
        warnings: skill_folder_warnings(selected_skills_by_names(&report.skills, &selected_names)),
        errors,
    })
}

fn sync_skill_folder_indexes(
    report: &WorkspaceSkillReport,
    skill_indexes: &[usize],
    options: &SkillFolderSyncOptions,
    strict_matching_create: bool,
    require_missing_create_rule: bool,
) -> Result<SkillFolderSyncReport> {
    let mut actions = Vec::new();
    let mut warnings = Vec::new();
    let mut errors = Vec::new();

    for skill in selected_skills_by_index(&report.skills, skill_indexes) {
        let skill_name = skill.name.as_deref().unwrap_or("<invalid>");
        for folder in &skill.artifact_folders {
            let action = apply_skill_folder_action(
                &report.workspace_root,
                skill_name,
                folder,
                options,
                strict_matching_create,
                require_missing_create_rule,
                &mut errors,
            );
            actions.push(SkillFolderSyncAction {
                skill: skill_name.to_string(),
                folder_id: folder.id.clone(),
                path: folder.path.clone(),
                kind: folder.kind,
                access: folder.access,
                action,
            });
        }
    }

    if !options.dry_run {
        let refreshed = workspace_skill_report(&report.workspace_root)?;
        warnings.extend(skill_folder_warnings(selected_skills_by_names(
            &refreshed.skills,
            &selected_skill_names(report, skill_indexes),
        )));
        errors.extend(skill_folder_errors(selected_skills_by_names(
            &refreshed.skills,
            &selected_skill_names(report, skill_indexes),
        )));
    } else {
        warnings.extend(skill_folder_warnings(selected_skills_by_index(
            &report.skills,
            skill_indexes,
        )));
        errors.extend(skill_folder_errors(selected_skills_by_index(
            &report.skills,
            skill_indexes,
        )));
    }

    Ok(SkillFolderSyncReport {
        workspace_root: report.workspace_root.clone(),
        dry_run: options.dry_run,
        situation: options.situation,
        actions,
        warnings,
        errors,
    })
}

fn selected_trusted_workspace_skill_indexes(
    report: &WorkspaceSkillReport,
    selected_skills: &[SkillId],
    situation: &str,
    errors: &mut Vec<String>,
) -> Result<Vec<usize>> {
    let builtins = builtin_registry()?;
    let mut indexes = Vec::new();
    for selected in selected_skills {
        let Some(index) = report
            .skills
            .iter()
            .position(|skill| skill.name.as_deref() == Some(selected.as_str()))
        else {
            continue;
        };
        let skill = &report.skills[index];
        if skill.usable && skill.trust_state == SkillTrustState::TrustedLocal {
            indexes.push(index);
            continue;
        }
        if builtins.get(selected).is_some() {
            continue;
        }
        errors.push(format!(
            "skill.{}.folder_prepare.{situation}.not_trusted: {:?}",
            selected, skill.trust_state
        ));
    }
    Ok(indexes)
}

fn apply_skill_folder_action(
    workspace_root: &Path,
    skill_name: &str,
    folder: &SkillArtifactFolderStatus,
    options: &SkillFolderSyncOptions,
    strict_matching_create: bool,
    require_missing_create_rule: bool,
    errors: &mut Vec<String>,
) -> SkillFolderSyncActionKind {
    let action = planned_skill_folder_action(folder, options.situation);
    match action {
        SkillFolderSyncActionKind::WouldCreateDir if !options.dry_run => {
            let path = workspace_root.join(&folder.path);
            match fs::create_dir_all(&path) {
                Ok(()) => SkillFolderSyncActionKind::CreatedDir,
                Err(err) => {
                    errors.push(format!(
                        "skill.{skill_name}.artifact_folder.{}.create_failed: {}",
                        folder.id, err
                    ));
                    action
                }
            }
        }
        SkillFolderSyncActionKind::SkippedReadOnly | SkillFolderSyncActionKind::SkippedSource
            if strict_matching_create && folder_has_create_situation(folder, options.situation) =>
        {
            errors.push(format!(
                "skill.{skill_name}.artifact_folder.{}.not_creatable_for_{}",
                folder.id,
                skill_folder_create_situation_key(options.situation)
            ));
            action
        }
        SkillFolderSyncActionKind::SkippedNoCreateRule
        | SkillFolderSyncActionKind::SkippedSituationMismatch
            if require_missing_create_rule =>
        {
            errors.push(format!(
                "skill.{skill_name}.artifact_folder.{}.missing_create_rule_for_{}",
                folder.id,
                skill_folder_create_situation_key(options.situation)
            ));
            action
        }
        _ => action,
    }
}

fn planned_skill_folder_action(
    folder: &SkillArtifactFolderStatus,
    situation: SkillFolderCreateSituation,
) -> SkillFolderSyncActionKind {
    if folder.exists {
        SkillFolderSyncActionKind::Exists
    } else if folder.access == SkillArtifactAccess::Read {
        SkillFolderSyncActionKind::SkippedReadOnly
    } else if folder.kind == SkillArtifactKind::Source {
        SkillFolderSyncActionKind::SkippedSource
    } else if folder.create.is_empty() {
        SkillFolderSyncActionKind::SkippedNoCreateRule
    } else if !folder_has_create_situation(folder, situation) {
        SkillFolderSyncActionKind::SkippedSituationMismatch
    } else {
        SkillFolderSyncActionKind::WouldCreateDir
    }
}

fn folder_has_create_situation(
    folder: &SkillArtifactFolderStatus,
    situation: SkillFolderCreateSituation,
) -> bool {
    folder.create.iter().any(|rule| rule.when == situation)
}

fn folder_readiness(folder: &SkillArtifactFolderStatus) -> Vec<SkillArtifactFolderReadiness> {
    [
        SkillFolderCreateSituation::SkillSync,
        SkillFolderCreateSituation::RuntimePrepare,
        SkillFolderCreateSituation::ArtifactWrite,
    ]
    .into_iter()
    .map(|situation| SkillArtifactFolderReadiness {
        situation,
        action: planned_skill_folder_action(folder, situation),
    })
    .collect()
}

fn selected_skill_names(report: &WorkspaceSkillReport, indexes: &[usize]) -> BTreeSet<String> {
    indexes
        .iter()
        .filter_map(|index| report.skills.get(*index))
        .filter_map(|skill| skill.name.clone())
        .collect()
}

fn selected_skills_by_index<'a>(
    skills: &'a [WorkspaceSkillStatus],
    indexes: &[usize],
) -> Vec<&'a WorkspaceSkillStatus> {
    indexes
        .iter()
        .filter_map(|index| skills.get(*index))
        .collect()
}

fn selected_skills_by_names<'a>(
    skills: &'a [WorkspaceSkillStatus],
    names: &BTreeSet<String>,
) -> Vec<&'a WorkspaceSkillStatus> {
    skills
        .iter()
        .filter(|skill| skill.name.as_ref().is_some_and(|name| names.contains(name)))
        .collect()
}

pub fn trusted_workspace_registry(
    start: impl AsRef<Path>,
    trust_store_path: impl AsRef<Path>,
) -> Result<SkillRegistry> {
    let report = workspace_skill_report_with_trust(start, trust_store_path)?;
    let workspace_skills = report
        .skills
        .into_iter()
        .filter(|skill| skill.usable && skill.trust_state == SkillTrustState::TrustedLocal)
        .collect::<Vec<_>>();
    let override_names = workspace_skills
        .iter()
        .filter_map(|skill| {
            skill
                .overrides_builtin
                .then(|| skill.name.clone())
                .flatten()
        })
        .collect::<BTreeSet<_>>();
    let builtins = builtin_registry()?;
    let mut registry = SkillRegistry::new();
    for skill in builtins.skills().iter() {
        if override_names.contains(&skill.harness.name) {
            continue;
        }
        registry.register(skill.clone())?;
    }
    for skill in workspace_skills {
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
        .filter(|skill| skill.valid)
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
            Ok(harness) => skills.push(status_from_harness(workspace_root, path, harness)),
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

fn status_from_harness(
    workspace_root: &Path,
    path: PathBuf,
    harness: SkillHarness,
) -> WorkspaceSkillStatus {
    let artifact_folders = artifact_folder_statuses(workspace_root, &harness);
    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    append_artifact_folder_diagnostics(&artifact_folders, &mut warnings, &mut errors);
    WorkspaceSkillStatus {
        name: Some(harness.name.clone()),
        path,
        source_path: Some(harness.source_path.clone()),
        description: Some(harness.description.clone()),
        version: Some(harness.version),
        source: Some(harness.source.as_str().to_string()),
        pack: Some(harness.pack.clone()),
        allowed_tools: harness
            .allowed_tools
            .iter()
            .map(|tool| tool.as_str().to_string())
            .collect(),
        requestable_tools: harness
            .requestable_tools
            .iter()
            .map(|tool| tool.as_str().to_string())
            .collect(),
        denied_tools: harness
            .denied_tools
            .iter()
            .map(|tool| tool.as_str().to_string())
            .collect(),
        permission_request_templates: harness
            .permission_request_templates
            .iter()
            .map(|template| template.id.clone())
            .collect(),
        memory_read_scopes: harness
            .permissions
            .memory
            .read
            .iter()
            .map(|scope| scope.as_str().to_string())
            .collect(),
        notes_read: harness.permissions.notes.read,
        notes_write: harness.permissions.notes.write,
        artifact_folders,
        valid: true,
        usable: false,
        shadowed_by_builtin: false,
        overrides_builtin: false,
        broadens_builtin_routing: false,
        trust_state: SkillTrustState::Unknown,
        harness: Some(harness),
        warnings,
        errors,
    }
}

fn append_artifact_folder_diagnostics(
    folders: &[SkillArtifactFolderStatus],
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) {
    for folder in folders {
        warnings.extend(
            folder
                .warnings
                .iter()
                .map(|warning| format!("artifact_folder.{}.{}", folder.id, warning)),
        );
        errors.extend(
            folder
                .errors
                .iter()
                .map(|error| format!("artifact_folder.{}.{}", folder.id, error)),
        );
    }
}

fn artifact_folder_statuses(
    workspace_root: &Path,
    harness: &SkillHarness,
) -> Vec<SkillArtifactFolderStatus> {
    harness
        .artifacts
        .iter()
        .map(|artifact| {
            let absolute_path = workspace_root.join(&artifact.path);
            let exists = absolute_path.exists();
            let mut warnings = Vec::new();
            let mut errors = Vec::new();

            if !exists {
                warnings.push("missing".to_string());
            } else if !absolute_path.is_dir() {
                errors.push("not_directory".to_string());
            }

            if artifact.kind == SkillArtifactKind::Source
                && matches!(
                    artifact.access,
                    SkillArtifactAccess::Write | SkillArtifactAccess::ReadWrite
                )
            {
                warnings.push("source_folder_writable".to_string());
            }

            SkillArtifactFolderStatus {
                id: artifact.id.clone(),
                path: artifact.path.clone(),
                kind: artifact.kind,
                access: artifact.access,
                create: artifact.create.clone(),
                readiness: Vec::new(),
                provides: artifact.provides.clone(),
                schema: artifact.schema.clone(),
                exists,
                warnings,
                errors,
            }
        })
        .map(|mut status| {
            status.readiness = folder_readiness(&status);
            status
        })
        .collect()
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
        allowed_tools: Vec::new(),
        requestable_tools: Vec::new(),
        denied_tools: Vec::new(),
        permission_request_templates: Vec::new(),
        memory_read_scopes: Vec::new(),
        notes_read: false,
        notes_write: false,
        artifact_folders: Vec::new(),
        valid: false,
        usable: false,
        shadowed_by_builtin: false,
        overrides_builtin: false,
        broadens_builtin_routing: false,
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
    let builtins = builtin_registry()?;
    let builtin_by_name = builtins
        .skills()
        .iter()
        .map(|skill| (skill.harness.name.clone(), skill.harness.clone()))
        .collect::<BTreeMap<_, _>>();
    for skill in skills {
        let Some(name) = &skill.name else {
            continue;
        };
        let Some(builtin) = builtin_by_name.get(name) else {
            continue;
        };
        skill.shadowed_by_builtin = true;
        if let Some(workspace) = &skill.harness {
            skill.broadens_builtin_routing = routing_broadens_builtin(workspace, builtin);
        }
    }
    Ok(())
}

fn routing_broadens_builtin(workspace: &SkillHarness, builtin: &SkillHarness) -> bool {
    has_extra_tools(&workspace.allowed_tools, &builtin.allowed_tools)
        || has_extra_tools(
            &workspace.requestable_tools,
            &builtin
                .allowed_tools
                .iter()
                .chain(builtin.requestable_tools.iter())
                .cloned()
                .collect::<Vec<_>>(),
        )
        || has_extra_tools(&builtin.denied_tools, &workspace.denied_tools)
}

fn has_extra_tools(left: &[agl_tools::ToolId], right: &[agl_tools::ToolId]) -> bool {
    let right = right.iter().collect::<BTreeSet<_>>();
    left.iter().any(|tool| !right.contains(tool))
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
            let source = skill
                .source
                .clone()
                .unwrap_or_else(|| SkillSource::Workspace.as_str().to_string());
            Some(LockedSkill {
                locked_at: existing_timestamps
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| locked_at.clone()),
                name,
                source,
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
        let overrides_builtin = state.permits_context_injection()
            && report.skills[index].valid
            && report.skills[index].shadowed_by_builtin;
        report.skills[index].trust_state = state;
        report.skills[index].overrides_builtin = overrides_builtin;
        report.skills[index].usable = state.permits_context_injection()
            && report.skills[index].valid
            && (!report.skills[index].shadowed_by_builtin || overrides_builtin);
        if overrides_builtin {
            report.skills[index]
                .warnings
                .retain(|warning| warning != "shadowed_by_builtin");
        }
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
    let source = skill.source.clone().context("skill source is missing")?;
    Ok(TrustedSkillRecord {
        skill_name: name,
        source,
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
    let catalog =
        agl_tools::builtin_tool_catalog().context("failed to register builtin tool catalog")?;
    for hook in &harness.required_hooks {
        if catalog.hook(hook).is_none() {
            bail!("skill `{}` requires missing hook `{hook}`", harness.name);
        }
    }
    validate_trust_tool_refs(
        &harness.name,
        "allowed_tools",
        &harness.allowed_tools,
        &catalog,
    )?;
    validate_trust_tool_refs(
        &harness.name,
        "requestable_tools",
        &harness.requestable_tools,
        &catalog,
    )?;
    validate_trust_tool_refs(
        &harness.name,
        "denied_tools",
        &harness.denied_tools,
        &catalog,
    )?;
    for template in &harness.permission_request_templates {
        validate_trust_tool_refs(
            &harness.name,
            "permission_request_templates.tools",
            &template.tools,
            &catalog,
        )?;
    }
    Ok(())
}

fn validate_trust_tool_refs(
    skill_name: &str,
    field: &str,
    tools: &[agl_tools::ToolId],
    catalog: &agl_tools::ToolCatalog,
) -> Result<()> {
    for tool in tools {
        if catalog.tool(tool).is_none() {
            bail!("skill `{skill_name}` references missing tool `{tool}` in {field}");
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

fn refresh_workspace_skill_report_derived(report: &mut WorkspaceSkillReport) {
    report.state = if !report.errors.is_empty() {
        SkillReportState::Invalid
    } else if !report.warnings.is_empty()
        || report.skills.iter().any(|skill| !skill.warnings.is_empty())
    {
        SkillReportState::Warning
    } else {
        SkillReportState::Ok
    };
    report.next_steps = workspace_skill_next_steps(report);
    report.diagnostics = workspace_skill_diagnostics(report);
}

fn workspace_skill_diagnostics(report: &WorkspaceSkillReport) -> Vec<WorkspaceSkillDiagnostic> {
    let mut diagnostics = Vec::new();
    if let Some(component) = &report.component {
        for warning in &component.warnings {
            diagnostics.push(component_diagnostic(
                WorkspaceSkillDiagnosticSeverity::Warning,
                component,
                warning,
            ));
        }
        for error in &component.errors {
            diagnostics.push(component_diagnostic(
                WorkspaceSkillDiagnosticSeverity::Error,
                component,
                error,
            ));
        }
    }

    for warning in &report.warnings {
        append_report_diagnostic(
            &mut diagnostics,
            report,
            WorkspaceSkillDiagnosticSeverity::Warning,
            warning,
        );
    }
    for error in &report.errors {
        append_report_diagnostic(
            &mut diagnostics,
            report,
            WorkspaceSkillDiagnosticSeverity::Error,
            error,
        );
    }

    for skill in &report.skills {
        let label = skill_diagnostic_label(skill);
        for warning in &skill.warnings {
            if warning.starts_with("artifact_folder.") {
                continue;
            }
            diagnostics.push(skill_diagnostic(
                WorkspaceSkillDiagnosticSeverity::Warning,
                WorkspaceSkillDiagnosticScope::SkillTrust,
                skill,
                &label,
                warning,
                skill_warning_code(warning),
            ));
        }
        for error in &skill.errors {
            if error.starts_with("artifact_folder.") {
                continue;
            }
            diagnostics.push(skill_diagnostic(
                WorkspaceSkillDiagnosticSeverity::Error,
                WorkspaceSkillDiagnosticScope::SkillManifest,
                skill,
                &label,
                error,
                skill_manifest_error_code(error),
            ));
        }
        for folder in &skill.artifact_folders {
            for warning in &folder.warnings {
                diagnostics.push(folder_diagnostic(
                    WorkspaceSkillDiagnosticSeverity::Warning,
                    skill,
                    &label,
                    folder,
                    warning,
                ));
            }
            for error in &folder.errors {
                diagnostics.push(folder_diagnostic(
                    WorkspaceSkillDiagnosticSeverity::Error,
                    skill,
                    &label,
                    folder,
                    error,
                ));
            }
        }
    }

    diagnostics
}

fn append_report_diagnostic(
    diagnostics: &mut Vec<WorkspaceSkillDiagnostic>,
    report: &WorkspaceSkillReport,
    severity: WorkspaceSkillDiagnosticSeverity,
    message: &str,
) {
    if message.starts_with("component.") || message.starts_with("skill.") {
        return;
    }
    if message.starts_with("skills_lock") {
        diagnostics.push(WorkspaceSkillDiagnostic {
            severity,
            scope: WorkspaceSkillDiagnosticScope::Lock,
            code: diagnostic_code(message),
            message: message.to_string(),
            component: None,
            skill: None,
            skill_path: None,
            folder_id: None,
            path: Some(report.lock_path.clone()),
        });
    } else if message == "skills_component_missing" {
        diagnostics.push(WorkspaceSkillDiagnostic {
            severity,
            scope: WorkspaceSkillDiagnosticScope::Component,
            code: message.to_string(),
            message: message.to_string(),
            component: Some(SKILLS_COMPONENT.to_string()),
            skill: None,
            skill_path: None,
            folder_id: None,
            path: None,
        });
    } else {
        diagnostics.push(WorkspaceSkillDiagnostic {
            severity,
            scope: WorkspaceSkillDiagnosticScope::Workspace,
            code: diagnostic_code(message),
            message: message.to_string(),
            component: None,
            skill: None,
            skill_path: None,
            folder_id: None,
            path: Some(report.workspace_root.clone()),
        });
    }
}

fn component_diagnostic(
    severity: WorkspaceSkillDiagnosticSeverity,
    component: &ComponentStatus,
    message: &str,
) -> WorkspaceSkillDiagnostic {
    WorkspaceSkillDiagnostic {
        severity,
        scope: WorkspaceSkillDiagnosticScope::Component,
        code: diagnostic_code(message),
        message: message.to_string(),
        component: Some(component.name.clone()),
        skill: None,
        skill_path: None,
        folder_id: None,
        path: Some(component.path.clone()),
    }
}

fn skill_diagnostic(
    severity: WorkspaceSkillDiagnosticSeverity,
    scope: WorkspaceSkillDiagnosticScope,
    skill: &WorkspaceSkillStatus,
    label: &str,
    message: &str,
    code: String,
) -> WorkspaceSkillDiagnostic {
    WorkspaceSkillDiagnostic {
        severity,
        scope,
        code,
        message: message.to_string(),
        component: None,
        skill: Some(label.to_string()),
        skill_path: Some(skill.path.clone()),
        folder_id: None,
        path: None,
    }
}

fn folder_diagnostic(
    severity: WorkspaceSkillDiagnosticSeverity,
    skill: &WorkspaceSkillStatus,
    label: &str,
    folder: &SkillArtifactFolderStatus,
    message: &str,
) -> WorkspaceSkillDiagnostic {
    WorkspaceSkillDiagnostic {
        severity,
        scope: WorkspaceSkillDiagnosticScope::SkillArtifactFolder,
        code: diagnostic_code(message),
        message: message.to_string(),
        component: None,
        skill: Some(label.to_string()),
        skill_path: Some(skill.path.clone()),
        folder_id: Some(folder.id.clone()),
        path: Some(folder.path.clone()),
    }
}

fn skill_diagnostic_label(skill: &WorkspaceSkillStatus) -> String {
    skill
        .name
        .clone()
        .unwrap_or_else(|| slash_path(&skill.path))
}

fn skill_error_key_label(skill: &WorkspaceSkillStatus) -> String {
    skill
        .name
        .clone()
        .unwrap_or_else(|| format!("path:{}", slash_path(&skill.path)))
}

fn skill_warning_code(message: &str) -> String {
    match message {
        "component_not_usable" => "component_not_usable".to_string(),
        "shadowed_by_builtin" => "shadowed_by_builtin".to_string(),
        "broadens_builtin_routing" => "broadens_builtin_routing".to_string(),
        _ => diagnostic_code(message),
    }
}

fn skill_manifest_error_code(message: &str) -> String {
    if message.contains("duplicate value") {
        "duplicate_value".to_string()
    } else if message.contains("artifact path is invalid") {
        "invalid_artifact_path".to_string()
    } else if message.contains("missing field") {
        "missing_field".to_string()
    } else if message.contains("list `") && message.contains(" is empty") {
        "empty_list".to_string()
    } else if message.contains("duplicate_skill_name") {
        "duplicate_skill_name".to_string()
    } else {
        "invalid_manifest".to_string()
    }
}

fn diagnostic_code(message: &str) -> String {
    let raw = message.split(':').next().unwrap_or(message).trim();
    let mut code = String::new();
    let mut last_was_separator = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            code.push(ch.to_ascii_lowercase());
            last_was_separator = false;
        } else if !last_was_separator {
            code.push('_');
            last_was_separator = true;
        }
    }
    let code = code.trim_matches('_').to_string();
    if code.is_empty() {
        "unknown".to_string()
    } else {
        code
    }
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
    let label = skill_error_key_label(skill);
    skill
        .errors
        .iter()
        .map(|error| format!("skill.{label}.{error}"))
        .collect()
}

fn skill_folder_warnings<'a>(
    skills: impl IntoIterator<Item = &'a WorkspaceSkillStatus>,
) -> Vec<String> {
    skills
        .into_iter()
        .flat_map(|skill| {
            let label = skill
                .name
                .clone()
                .unwrap_or_else(|| skill.path.display().to_string());
            skill.artifact_folders.iter().flat_map(move |folder| {
                let label = label.clone();
                folder.warnings.iter().map(move |warning| {
                    format!("skill.{label}.artifact_folder.{}.{}", folder.id, warning)
                })
            })
        })
        .collect()
}

fn skill_folder_errors<'a>(
    skills: impl IntoIterator<Item = &'a WorkspaceSkillStatus>,
) -> Vec<String> {
    skills
        .into_iter()
        .flat_map(|skill| {
            let label = skill
                .name
                .clone()
                .unwrap_or_else(|| skill.path.display().to_string());
            skill.artifact_folders.iter().flat_map(move |folder| {
                let label = label.clone();
                folder.errors.iter().map(move |error| {
                    format!("skill.{label}.artifact_folder.{}.{}", folder.id, error)
                })
            })
        })
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
    if report.skills.iter().any(|skill| {
        skill.artifact_folders.iter().any(|folder| {
            !folder.exists
                && folder.access != SkillArtifactAccess::Read
                && folder.kind != SkillArtifactKind::Source
                && folder
                    .create
                    .iter()
                    .any(|rule| rule.when == SkillFolderCreateSituation::SkillSync)
        })
    }) {
        next_steps.push("agl skill sync-folders".to_string());
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

fn skill_folder_create_situation_key(situation: SkillFolderCreateSituation) -> &'static str {
    match situation {
        SkillFolderCreateSituation::SkillSync => "skill_sync",
        SkillFolderCreateSituation::RuntimePrepare => "runtime_prepare",
        SkillFolderCreateSituation::ArtifactWrite => "artifact_write",
    }
}

fn lock_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("unix:{seconds}")
}

#[cfg(test)]
mod tests;
