use std::path::PathBuf;

use agl_ids::{ExecutionId, RunId, SessionId};
use clap::ValueEnum;
use clap_complete::Shell;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CliInvocation {
    pub(crate) command: CliCommand,
    pub(crate) home: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CliCommand {
    Interactive(InteractiveOptions),
    Help { bin_name: &'static str },
    HelpPrinted,
    Completion { shell: Shell },
    Config(ConfigCommand),
    Cron(CronCommand),
    Store(StoreCommand),
    Function(FunctionCommand),
    Inference(InferenceCommand),
    Init(SetupInitOptions),
    Model(ModelCommand),
    Memory(MemoryCommand),
    Notes(NotesCommand),
    Repo(RepoCommand),
    Skill(SkillCommand),
    Process(ProcessCommand),
    DaemonStatus(DaemonStatusOptions),
    Serve(ServeOptions),
    Run(RunOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InteractiveOptions {
    pub(crate) resume: Option<String>,
    pub(crate) input_history: bool,
    pub(crate) socket_path: Option<PathBuf>,
    pub(crate) workspace_root: Option<PathBuf>,
    pub(crate) function_ref: Option<String>,
    pub(crate) model_id: Option<String>,
    pub(crate) operation_mode: Option<ToolAccessMode>,
    pub(crate) skills: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SetupInitOptions {
    pub(crate) model: Option<String>,
    pub(crate) yes: bool,
    pub(crate) non_interactive: bool,
    pub(crate) dry_run: bool,
    pub(crate) offline: bool,
    pub(crate) json: bool,
    pub(crate) allow_low_memory: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ModelCommand {
    Pull(ModelPullOptions),
    Import(ModelImportOptions),
    List(ModelListOptions),
    Status(ModelStatusOptions),
    Verify(ModelStatusOptions),
    Unbind(ModelMutationOptions),
    Remove(ModelMutationOptions),
    Prune(ModelPruneOptions),
    Unload(ModelUnloadOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelPullOptions {
    pub(crate) source: String,
    pub(crate) id: Option<String>,
    pub(crate) mmproj: Option<String>,
    pub(crate) replace: bool,
    pub(crate) yes: bool,
    pub(crate) non_interactive: bool,
    pub(crate) dry_run: bool,
    pub(crate) offline: bool,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelImportOptions {
    pub(crate) path: PathBuf,
    pub(crate) id: Option<String>,
    pub(crate) mmproj: Option<PathBuf>,
    pub(crate) replace: bool,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelListOptions {
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelStatusOptions {
    pub(crate) model_id: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelMutationOptions {
    pub(crate) model_id: String,
    pub(crate) yes: bool,
    pub(crate) dry_run: bool,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelPruneOptions {
    pub(crate) yes: bool,
    pub(crate) dry_run: bool,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelUnloadOptions {
    pub(crate) target: agl_protocol::ModelUnloadTarget,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConfigCommand {
    Paths,
    Status(ConfigStatusOptions),
    Init { force: bool },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StoreCommand {
    Status(StoreStatusOptions),
    Migrate(StoreMigrateOptions),
    Export(StoreExportCliOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FunctionCommand {
    List(FunctionListOptions),
    Show(FunctionShowOptions),
    Status(FunctionStatusOptions),
    Init(FunctionInitOptions),
    Doctor(FunctionDoctorOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InferenceCommand {
    Run(RunOptions),
    Serve(ServeOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CronCommand {
    Add(CronAddOptions),
    List(CronListOptions),
    Show(CronShowOptions),
    Enable(CronEnableOptions),
    Disable(CronDisableOptions),
    Run(CronRunOptions),
    Tick(CronTickOptions),
    History(CronHistoryOptions),
    Delete(CronDeleteOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RepoCommand {
    Init(RepoInitOptions),
    InitComponent(RepoComponentInitOptions),
    ImportProfile(RepoImportProfileOptions),
    Status(RepoStatusOptions),
    VerifyTasks(TaskSpecVerifyOptions),
    Artifact(ArtifactCommand),
    InstallHooks(RepoHooksOptions),
    ExportProfile(RepoExportProfileOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MemoryCommand {
    Add(MemoryAddOptions),
    List(MemoryListOptions),
    Search(MemorySearchOptions),
    Show(MemoryShowOptions),
    Delete(MemoryDeleteOptions),
    Suggest(MemorySuggestOptions),
    ListSuggestions(MemoryListSuggestionsOptions),
    Approve(MemoryApproveOptions),
    Reject(MemoryRejectOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum NotesCommand {
    Add(NotesAddOptions),
    List(NotesListOptions),
    Search(NotesSearchOptions),
    Show(NotesShowOptions),
    Update(NotesUpdateOptions),
    Delete(NotesDeleteOptions),
    Link(NotesLinkOptions),
    Remember(NotesRememberOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SkillCommand {
    Init(SkillInitOptions),
    List(SkillListOptions),
    Inspect(SkillInspectOptions),
    Status(SkillStatusOptions),
    Verify(SkillVerifyOptions),
    SyncFolders(SkillFolderSyncOptions),
    Lock(SkillLockOptions),
    Trust(SkillTrustOptions),
    Revoke(SkillRevokeOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProcessCommand {
    List(ProcessListOptions),
    Status(ProcessStatusOptions),
    Read(ProcessReadOptions),
    Attach(ProcessAttachOptions),
    Kill(ProcessKillOptions),
    Doctor(ProcessDoctorOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProcessListOptions {
    pub(crate) session_id: Option<SessionId>,
    pub(crate) root_run_id: Option<RunId>,
    pub(crate) include_finished: bool,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProcessStatusOptions {
    pub(crate) execution_id: ExecutionId,
    pub(crate) private_command: bool,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProcessReadOptions {
    pub(crate) execution_id: ExecutionId,
    pub(crate) after_sequence: u64,
    pub(crate) max_bytes: usize,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProcessAttachOptions {
    pub(crate) execution_id: ExecutionId,
    pub(crate) after_sequence: u64,
    pub(crate) read_only: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProcessKillOptions {
    pub(crate) execution_id: ExecutionId,
    pub(crate) immediate: bool,
    pub(crate) yes: bool,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProcessDoctorOptions {
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepoComponentInitOptions {
    pub(crate) component: String,
    pub(crate) dry_run: bool,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepoInitOptions {
    pub(crate) profile: String,
    pub(crate) profile_file: Option<PathBuf>,
    pub(crate) artifacts: Vec<RepoArtifactOverride>,
    pub(crate) skills_url: Option<String>,
    pub(crate) skills_rev: Option<String>,
    pub(crate) tasks_url: Option<String>,
    pub(crate) tasks_rev: Option<String>,
    pub(crate) dry_run: bool,
    pub(crate) force: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepoArtifactOverride {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) rev: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepoStatusOptions {
    pub(crate) json: bool,
    pub(crate) component: Option<String>,
    pub(crate) strict: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TaskSpecVerifyOptions {
    pub(crate) json: bool,
    pub(crate) strict: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ArtifactCommand {
    Status(ArtifactStatusOptions),
    Verify(ArtifactStatusOptions),
    Sync(ArtifactSyncOptions),
    Lock(ArtifactLockOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ArtifactStatusOptions {
    pub(crate) json: bool,
    pub(crate) artifact: Option<String>,
    pub(crate) strict: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ArtifactSyncOptions {
    pub(crate) json: bool,
    pub(crate) dry_run: bool,
    pub(crate) strict: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ArtifactLockOptions {
    pub(crate) json: bool,
    pub(crate) dry_run: bool,
    pub(crate) strict: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepoHooksOptions {
    pub(crate) dry_run: bool,
    pub(crate) force: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepoExportProfileOptions {
    pub(crate) out: PathBuf,
    pub(crate) force: bool,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepoImportProfileOptions {
    pub(crate) profile_file: PathBuf,
    pub(crate) dry_run: bool,
    pub(crate) force: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillListOptions {
    pub(crate) json: bool,
    pub(crate) source: SkillListSourceArg,
    pub(crate) trusted_only: bool,
    pub(crate) limit: Option<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub(crate) enum SkillListSourceArg {
    All,
    Core,
    Community,
    Local,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillInitOptions {
    pub(crate) dry_run: bool,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigStatusOptions {
    pub(crate) config: Option<PathBuf>,
    pub(crate) json: bool,
    pub(crate) strict: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemoryAddOptions {
    pub(crate) scope: MemoryScopeArg,
    pub(crate) scope_key: Option<String>,
    pub(crate) kind: MemoryKindArg,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) source_ref: Option<String>,
    pub(crate) confidence: u8,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemoryListOptions {
    pub(crate) scope: MemoryScopeArg,
    pub(crate) scope_key: Option<String>,
    pub(crate) include_deleted: bool,
    pub(crate) limit: usize,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemorySearchOptions {
    pub(crate) query: String,
    pub(crate) scope: MemoryScopeArg,
    pub(crate) scope_key: Option<String>,
    pub(crate) include_deleted: bool,
    pub(crate) limit: usize,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemoryShowOptions {
    pub(crate) id: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemoryDeleteOptions {
    pub(crate) id: String,
    pub(crate) json: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub(crate) enum MemorySuggestionStatusArg {
    Pending,
    Approved,
    Rejected,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemorySuggestOptions {
    pub(crate) scope: MemoryScopeArg,
    pub(crate) scope_key: Option<String>,
    pub(crate) kind: MemoryKindArg,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) source_ref: String,
    pub(crate) confidence: u8,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemoryListSuggestionsOptions {
    pub(crate) scope: MemoryScopeArg,
    pub(crate) scope_key: Option<String>,
    pub(crate) status: Option<MemorySuggestionStatusArg>,
    pub(crate) all_scopes: bool,
    pub(crate) limit: usize,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemoryApproveOptions {
    pub(crate) id: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MemoryRejectOptions {
    pub(crate) id: String,
    pub(crate) reason: Option<String>,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotesAddOptions {
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotesListOptions {
    pub(crate) include_deleted: bool,
    pub(crate) limit: usize,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotesSearchOptions {
    pub(crate) query: String,
    pub(crate) include_deleted: bool,
    pub(crate) limit: usize,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotesShowOptions {
    pub(crate) id: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotesUpdateOptions {
    pub(crate) id: String,
    pub(crate) title: Option<String>,
    pub(crate) body: Option<String>,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotesDeleteOptions {
    pub(crate) id: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotesLinkOptions {
    pub(crate) id: String,
    pub(crate) target_ref: String,
    pub(crate) label: Option<String>,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotesRememberOptions {
    pub(crate) id: String,
    pub(crate) scope: MemoryScopeArg,
    pub(crate) scope_key: Option<String>,
    pub(crate) kind: MemoryKindArg,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillInspectOptions {
    pub(crate) name: String,
    pub(crate) json: bool,
    pub(crate) runtime: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillStatusOptions {
    pub(crate) json: bool,
    pub(crate) strict: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillVerifyOptions {
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillFolderSyncOptions {
    pub(crate) json: bool,
    pub(crate) dry_run: bool,
    pub(crate) when: SkillFolderSyncSituationArg,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum SkillFolderSyncSituationArg {
    SkillSync,
    RuntimePrepare,
    ArtifactWrite,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillLockOptions {
    pub(crate) json: bool,
    pub(crate) dry_run: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillTrustOptions {
    pub(crate) name: String,
    pub(crate) json: bool,
    pub(crate) yes: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SkillRevokeOptions {
    pub(crate) name: String,
    pub(crate) json: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub(crate) enum StoreDomainArg {
    Memory,
    Notes,
    Cron,
    Permissions,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoreStatusOptions {
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoreMigrateOptions {
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoreExportCliOptions {
    pub(crate) domain: StoreDomainArg,
    pub(crate) out: PathBuf,
    pub(crate) include_deleted: bool,
    pub(crate) force: bool,
    pub(crate) json: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CronTargetKindArg {
    Skill,
    Builtin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CronTargetArg {
    pub(crate) kind: CronTargetKindArg,
    pub(crate) target_ref: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CronAddOptions {
    pub(crate) name: String,
    pub(crate) schedule: String,
    pub(crate) target: CronTargetArg,
    pub(crate) enabled: bool,
    pub(crate) timezone: Option<String>,
    pub(crate) notify_ref: Option<String>,
    pub(crate) prompt: Option<String>,
    pub(crate) input: Option<String>,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CronListOptions {
    pub(crate) include_deleted: bool,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CronShowOptions {
    pub(crate) id: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CronEnableOptions {
    pub(crate) id: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CronDisableOptions {
    pub(crate) id: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CronRunOptions {
    pub(crate) id: String,
    pub(crate) now: bool,
    pub(crate) preflight: bool,
    pub(crate) mock_skill_execution: bool,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CronTickOptions {
    pub(crate) at: Option<u64>,
    pub(crate) mock_skill_execution: bool,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CronHistoryOptions {
    pub(crate) id: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CronDeleteOptions {
    pub(crate) id: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RunOptions {
    pub(crate) config: Option<PathBuf>,
    pub(crate) function_ref: Option<String>,
    pub(crate) artifact_root: Option<PathBuf>,
    pub(crate) workspace_root: Option<PathBuf>,
    pub(crate) session_id: Option<SessionId>,
    pub(crate) no_history: bool,
    pub(crate) new_session: bool,
    pub(crate) max_output_tokens: Option<u32>,
    pub(crate) tool_mode: Option<ToolAccessMode>,
    pub(crate) skills: Vec<String>,
    pub(crate) memory: bool,
    pub(crate) prompt: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FunctionListOptions {
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FunctionShowOptions {
    pub(crate) reference: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FunctionStatusOptions {
    pub(crate) reference: String,
    pub(crate) json: bool,
    pub(crate) strict: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FunctionInitOptions {
    pub(crate) id: String,
    pub(crate) workspace: bool,
    pub(crate) model_profile: Option<String>,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FunctionDoctorOptions {
    pub(crate) reference: String,
    pub(crate) json: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ServeOptions {
    pub(crate) socket_path: Option<PathBuf>,
    pub(crate) systemd_activation: bool,
    pub(crate) config: Option<PathBuf>,
    pub(crate) function_ref: Option<String>,
    pub(crate) artifact_root: Option<PathBuf>,
    pub(crate) workspace_root: Option<PathBuf>,
    pub(crate) max_output_tokens: Option<u32>,
    pub(crate) tool_mode: Option<ToolAccessMode>,
    pub(crate) skills: Vec<String>,
    pub(crate) memory: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DaemonStatusOptions {
    pub(crate) socket_path: Option<PathBuf>,
    pub(crate) detail: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub(crate) enum ToolAccessMode {
    ReadOnly,
    Write,
    Execute,
    Approve,
    Admin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub(crate) enum MemoryScopeArg {
    User,
    Repo,
    MatrixRoom,
    MatrixUser,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub(crate) enum MemoryKindArg {
    Fact,
    Preference,
    Summary,
    Decision,
    WorkingNote,
}
