use std::path::PathBuf;

use clap::ValueEnum;
use clap_complete::Shell;

pub(crate) const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CliInvocation {
    pub(crate) command: CliCommand,
    pub(crate) home: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CliCommand {
    Help { bin_name: &'static str },
    HelpPrinted,
    Completion { shell: Shell },
    Config(ConfigCommand),
    Cron(CronCommand),
    Store(StoreCommand),
    Memory(MemoryCommand),
    Notes(NotesCommand),
    Repo(RepoCommand),
    Skill(SkillCommand),
    DaemonStatus(DaemonStatusOptions),
    Serve(ServeOptions),
    Infer(RunOptions),
    Chat(RunOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConfigCommand {
    Paths,
    Init { force: bool },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StoreCommand {
    Status(StoreStatusOptions),
    Migrate(StoreMigrateOptions),
    Export(StoreExportCliOptions),
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
    ImportProfile(RepoImportProfileOptions),
    Status(RepoStatusOptions),
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
    List(SkillListOptions),
    Inspect(SkillInspectOptions),
    Status(SkillStatusOptions),
    Verify(SkillVerifyOptions),
    Lock(SkillLockOptions),
    Trust(SkillTrustOptions),
    Revoke(SkillRevokeOptions),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepoInitOptions {
    pub(crate) profile: String,
    pub(crate) profile_file: Option<PathBuf>,
    pub(crate) dry_run: bool,
    pub(crate) force: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepoStatusOptions {
    pub(crate) json: bool,
    pub(crate) component: Option<String>,
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
    Builtin,
    Workspace,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RunOptions {
    pub(crate) config: Option<PathBuf>,
    pub(crate) artifact_root: Option<PathBuf>,
    pub(crate) run_id: Option<String>,
    pub(crate) workspace_root: Option<PathBuf>,
    pub(crate) session_id: Option<String>,
    pub(crate) no_history: bool,
    pub(crate) new_session: bool,
    pub(crate) max_output_tokens: u32,
    pub(crate) tool_mode: ToolAccessMode,
    pub(crate) skills: Vec<String>,
    pub(crate) memory: bool,
    pub(crate) prompt: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ServeOptions {
    pub(crate) socket_path: Option<PathBuf>,
    pub(crate) config: Option<PathBuf>,
    pub(crate) artifact_root: Option<PathBuf>,
    pub(crate) run_id: Option<String>,
    pub(crate) workspace_root: Option<PathBuf>,
    pub(crate) max_output_tokens: u32,
    pub(crate) tool_mode: ToolAccessMode,
    pub(crate) skills: Vec<String>,
    pub(crate) memory: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DaemonStatusOptions {
    pub(crate) socket_path: Option<PathBuf>,
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

impl ToolAccessMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Write => "write",
            Self::Execute => "execute",
            Self::Approve => "approve",
            Self::Admin => "admin",
        }
    }
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            config: None,
            artifact_root: None,
            run_id: None,
            workspace_root: None,
            session_id: None,
            no_history: false,
            new_session: false,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            tool_mode: ToolAccessMode::ReadOnly,
            skills: Vec::new(),
            memory: false,
            prompt: None,
        }
    }
}
