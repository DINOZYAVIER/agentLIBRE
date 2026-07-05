use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const AGL_DIR: &str = ".agl";
pub const WORKSPACE_MANIFEST_PATH: &str = ".agl/workspace.toml";
pub const ARTIFACT_LOCK_PATH: &str = ".agl/artifact-lock.toml";
pub const DEFAULT_PROFILE: &str = "repo-workflow";
pub const DEFAULT_SKILLS_URL: &str = "git@github.com:agentlibre/agl-skills.git";
pub const DEFAULT_SKILLS_REV: &str = "v0.1.0";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepoInitOptions {
    pub profile: String,
    pub profile_file: Option<PathBuf>,
    pub skills_url: Option<String>,
    pub skills_rev: Option<String>,
    pub tasks_url: Option<String>,
    pub tasks_rev: Option<String>,
    pub dry_run: bool,
    pub force: bool,
}

impl Default for RepoInitOptions {
    fn default() -> Self {
        Self {
            profile: DEFAULT_PROFILE.to_string(),
            profile_file: None,
            skills_url: None,
            skills_rev: None,
            tasks_url: None,
            tasks_rev: None,
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
pub struct RepoComponentInitOptions {
    pub component: String,
    pub dry_run: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSpecVerifyOptions {
    pub strict: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactStatusOptions {
    pub artifact: Option<String>,
    pub strict: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactSyncOptions {
    pub dry_run: bool,
    pub strict: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactLockOptions {
    pub dry_run: bool,
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
pub struct RepoComponentInitReport {
    pub workspace_root: PathBuf,
    pub manifest_path: PathBuf,
    pub component: String,
    pub path: PathBuf,
    pub dry_run: bool,
    pub actions: Vec<RepoComponentInitAction>,
    pub errors: Vec<String>,
}

impl RepoComponentInitReport {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoComponentInitAction {
    WouldAddSubmodule,
    AddedSubmodule,
    WouldUpdateSubmodule,
    UpdatedSubmodule,
    WouldCheckoutRev,
    CheckedOutRev,
    AlreadyInitialized,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TaskSpecValidation {
    pub missing_sections: Vec<String>,
}

impl TaskSpecValidation {
    pub fn is_valid(&self) -> bool {
        self.missing_sections.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TaskSpecVerifyReport {
    pub state: TaskSpecVerifyState,
    pub workspace_root: PathBuf,
    pub component: Option<ComponentStatus>,
    pub root: PathBuf,
    pub files: Vec<TaskSpecFileStatus>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl TaskSpecVerifyReport {
    pub fn should_fail(&self, strict: bool) -> bool {
        !self.errors.is_empty() || (strict && !self.warnings.is_empty())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskSpecVerifyState {
    Ok,
    Warning,
    Invalid,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TaskSpecFileStatus {
    pub path: PathBuf,
    pub valid: bool,
    pub missing_sections: Vec<String>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ArtifactStatusReport {
    pub state: ArtifactReportState,
    pub workspace_root: PathBuf,
    pub manifest_path: PathBuf,
    pub lock_path: PathBuf,
    pub artifacts: Vec<ArtifactStatus>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub next_steps: Vec<String>,
}

impl ArtifactStatusReport {
    pub fn should_fail(&self, strict: bool) -> bool {
        !self.errors.is_empty() || (strict && !self.warnings.is_empty())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ArtifactStatus {
    pub id: String,
    pub source_id: String,
    pub path: PathBuf,
    pub kind: ArtifactKind,
    pub access: ArtifactAccess,
    pub provides: Vec<String>,
    pub schema: Option<String>,
    pub state: ArtifactState,
    pub exists: bool,
    pub contract_hash: String,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactReportState {
    Ok,
    Warning,
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactState {
    Ok,
    Missing,
    Warning,
    Invalid,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ArtifactSyncReport {
    pub workspace_root: PathBuf,
    pub manifest_path: PathBuf,
    pub dry_run: bool,
    pub actions: Vec<ArtifactSyncAction>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl ArtifactSyncReport {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ArtifactSyncAction {
    pub artifact_id: String,
    pub path: PathBuf,
    pub action: ArtifactSyncActionKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactSyncActionKind {
    Exists,
    WouldCreateDir,
    CreatedDir,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ArtifactLockReport {
    pub workspace_root: PathBuf,
    pub lock_path: PathBuf,
    pub dry_run: bool,
    pub wrote: bool,
    pub lock: ArtifactLockFile,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl ArtifactLockReport {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactLockFile {
    pub version: u32,
    pub artifacts: BTreeMap<String, LockedArtifact>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedArtifact {
    pub id: String,
    pub source_id: String,
    pub path: PathBuf,
    pub kind: ArtifactKind,
    pub access: ArtifactAccess,
    pub provides: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub contract_hash: String,
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifact_sources: BTreeMap<String, ArtifactSource>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceProfile {
    pub version: u32,
    pub name: String,
    pub components: BTreeMap<String, WorkspaceComponent>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub artifact_sources: BTreeMap<String, ArtifactSource>,
    #[serde(default)]
    pub policy: WorkspaceProfilePolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_pack: Option<WorkspaceSkillPackIdentity>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceProfilePolicy {
    #[serde(default)]
    pub hooks: WorkspaceHookPolicy,
    #[serde(default)]
    pub trust: WorkspaceTrustPolicy,
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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceTrustPolicy {
    pub import_local_trust: bool,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactSource {
    pub role: ArtifactSourceRole,
    pub kind: ArtifactSourceKind,
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub provides: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactContract>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactSourceRole {
    Core,
    Community,
    Local,
    Planning,
    Generated,
    State,
    Compatibility,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactSourceKind {
    Git,
    Submodule,
    Local,
    Generated,
    Ignored,
    Compatibility,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactContract {
    pub id: String,
    pub kind: ArtifactKind,
    pub path: PathBuf,
    pub access: ArtifactAccess,
    #[serde(default)]
    pub provides: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(default)]
    pub create: Vec<ArtifactCreateRule>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub shared: bool,
    #[serde(default)]
    pub conflict_policy: ArtifactConflictPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Source,
    Generated,
    State,
    Cache,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactAccess {
    Read,
    Write,
    ReadWrite,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactCreateRule {
    pub dir: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactConflictPolicy {
    Identical,
    SourcePriority,
    Reject,
}

impl Default for ArtifactConflictPolicy {
    fn default() -> Self {
        Self::Reject
    }
}
