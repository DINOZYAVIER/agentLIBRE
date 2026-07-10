use std::io::IsTerminal;
use std::path::PathBuf;

use agl_capabilities::SkillId;
use agl_ids::SessionId;
use anyhow::{Context, Result, bail};
use clap::error::ErrorKind;
use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use clap_complete::{Shell, generate};

mod model;

pub(crate) use model::*;

const CLI_DISPLAY_NAME: &str = "agl";

macro_rules! cli_help {
    ($path:literal) => {
        include_str!(concat!("../../../assets/cli-help/", $path, ".md"))
    };
}

mod help {
    pub(super) const WELCOME: &str = cli_help!("welcome");
    pub(super) const AGL: &str = cli_help!("agl");
    pub(super) const CHAT: &str = cli_help!("chat");
    pub(super) const COMPLETION: &str = cli_help!("completion");
    pub(super) const CONFIG: &str = cli_help!("config");
    pub(super) const CONFIG_INIT: &str = cli_help!("config/init");
    pub(super) const CONFIG_PATHS: &str = cli_help!("config/paths");
    pub(super) const CONFIG_STATUS: &str = cli_help!("config/status");
    pub(super) const FUNCTION: &str = cli_help!("function");
    pub(super) const FUNCTION_DOCTOR: &str = cli_help!("function/doctor");
    pub(super) const FUNCTION_INIT: &str = cli_help!("function/init");
    pub(super) const FUNCTION_LIST: &str = cli_help!("function/list");
    pub(super) const FUNCTION_SHOW: &str = cli_help!("function/show");
    pub(super) const FUNCTION_STATUS: &str = cli_help!("function/status");
    pub(super) const CRON: &str = cli_help!("cron");
    pub(super) const CRON_ADD: &str = cli_help!("cron/add");
    pub(super) const CRON_DELETE: &str = cli_help!("cron/delete");
    pub(super) const CRON_DISABLE: &str = cli_help!("cron/disable");
    pub(super) const CRON_ENABLE: &str = cli_help!("cron/enable");
    pub(super) const CRON_HISTORY: &str = cli_help!("cron/history");
    pub(super) const CRON_LIST: &str = cli_help!("cron/list");
    pub(super) const CRON_RUN: &str = cli_help!("cron/run");
    pub(super) const CRON_SHOW: &str = cli_help!("cron/show");
    pub(super) const CRON_TICK: &str = cli_help!("cron/tick");
    pub(super) const DAEMON: &str = cli_help!("daemon");
    pub(super) const DAEMON_STATUS: &str = cli_help!("daemon/status");
    pub(super) const INIT: &str = cli_help!("init");
    pub(super) const INFERENCE: &str = cli_help!("inference");
    pub(super) const INFERENCE_CHAT: &str = cli_help!("inference/chat");
    pub(super) const INFERENCE_RUN: &str = cli_help!("inference/run");
    pub(super) const INFERENCE_SERVE: &str = cli_help!("inference/serve");
    pub(super) const INSTALL_HOOKS: &str = cli_help!("install-hooks");
    pub(super) const MEMORY: &str = cli_help!("memory");
    pub(super) const MEMORY_ADD: &str = cli_help!("memory/add");
    pub(super) const MEMORY_APPROVE: &str = cli_help!("memory/approve");
    pub(super) const MEMORY_DELETE: &str = cli_help!("memory/delete");
    pub(super) const MEMORY_LIST: &str = cli_help!("memory/list");
    pub(super) const MEMORY_LIST_SUGGESTIONS: &str = cli_help!("memory/list-suggestions");
    pub(super) const MEMORY_REJECT: &str = cli_help!("memory/reject");
    pub(super) const MEMORY_SEARCH: &str = cli_help!("memory/search");
    pub(super) const MEMORY_SHOW: &str = cli_help!("memory/show");
    pub(super) const MEMORY_SUGGEST: &str = cli_help!("memory/suggest");
    pub(super) const NOTES: &str = cli_help!("notes");
    pub(super) const NOTES_ADD: &str = cli_help!("notes/add");
    pub(super) const NOTES_DELETE: &str = cli_help!("notes/delete");
    pub(super) const NOTES_LINK: &str = cli_help!("notes/link");
    pub(super) const NOTES_LIST: &str = cli_help!("notes/list");
    pub(super) const NOTES_REMEMBER: &str = cli_help!("notes/remember");
    pub(super) const NOTES_SEARCH: &str = cli_help!("notes/search");
    pub(super) const NOTES_SHOW: &str = cli_help!("notes/show");
    pub(super) const NOTES_UPDATE: &str = cli_help!("notes/update");
    pub(super) const REPO: &str = cli_help!("repo");
    pub(super) const REPO_ARTIFACT: &str = cli_help!("repo/artifact");
    pub(super) const REPO_ARTIFACT_LOCK: &str = cli_help!("repo/artifact/lock");
    pub(super) const REPO_ARTIFACT_STATUS: &str = cli_help!("repo/artifact/status");
    pub(super) const REPO_ARTIFACT_SYNC: &str = cli_help!("repo/artifact/sync");
    pub(super) const REPO_ARTIFACT_VERIFY: &str = cli_help!("repo/artifact/verify");
    pub(super) const REPO_EXPORT_PROFILE: &str = cli_help!("repo/export-profile");
    pub(super) const REPO_IMPORT_PROFILE: &str = cli_help!("repo/import-profile");
    pub(super) const REPO_INIT: &str = cli_help!("repo/init");
    pub(super) const REPO_INIT_COMPONENT: &str = cli_help!("repo/init-component");
    pub(super) const REPO_INSTALL_HOOKS: &str = cli_help!("repo/install-hooks");
    pub(super) const REPO_STATUS: &str = cli_help!("repo/status");
    pub(super) const REPO_VERIFY_TASKS: &str = cli_help!("repo/verify-tasks");
    pub(super) const RUN: &str = cli_help!("run");
    pub(super) const SERVE: &str = cli_help!("serve");
    pub(super) const SKILL: &str = cli_help!("skill");
    pub(super) const SKILL_INIT: &str = cli_help!("skill/init");
    pub(super) const SKILL_INSPECT: &str = cli_help!("skill/inspect");
    pub(super) const SKILL_LIST: &str = cli_help!("skill/list");
    pub(super) const SKILL_LOCK: &str = cli_help!("skill/lock");
    pub(super) const SKILL_REVOKE: &str = cli_help!("skill/revoke");
    pub(super) const SKILL_STATUS: &str = cli_help!("skill/status");
    pub(super) const SKILL_SYNC_FOLDERS: &str = cli_help!("skill/sync-folders");
    pub(super) const SKILL_TRUST: &str = cli_help!("skill/trust");
    pub(super) const SKILL_VERIFY: &str = cli_help!("skill/verify");
    pub(super) const STATUS: &str = cli_help!("status");
    pub(super) const STORE: &str = cli_help!("store");
    pub(super) const STORE_EXPORT: &str = cli_help!("store/export");
    pub(super) const STORE_MIGRATE: &str = cli_help!("store/migrate");
    pub(super) const STORE_STATUS: &str = cli_help!("store/status");
}

#[derive(Debug, Parser)]
#[command(
    name = "agl",
    bin_name = "agl",
    version,
    about = "agentLIBRE CLI - local-first agentic system",
    long_about = help::AGL
)]
struct Cli {
    /// Override AGL_HOME for this invocation.
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,

    /// Prompt text for a one-shot run.
    #[arg(value_name = "PROMPT", num_args = 1.., trailing_var_arg = true)]
    prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Write shell completion scripts to stdout.
    #[command(long_about = help::COMPLETION)]
    Completion {
        /// Shell to generate completions for.
        #[arg(value_enum, default_value_t = Shell::Bash)]
        shell: Shell,
    },
    /// Runtime configuration commands.
    #[command(long_about = help::CONFIG)]
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Inspect and export the local AgentLIBRE store.
    #[command(long_about = help::STORE)]
    Store {
        #[command(subcommand)]
        command: StoreCommands,
    },
    /// Inspect and create agentFUNCTION artifacts.
    #[command(long_about = help::FUNCTION)]
    Function {
        #[command(subcommand)]
        command: FunctionCommands,
    },
    /// Low-level direct local inference commands.
    #[command(long_about = help::INFERENCE)]
    Inference {
        #[command(subcommand)]
        command: InferenceCommands,
    },
    /// Manage local scheduled AgentLIBRE jobs.
    #[command(long_about = help::CRON)]
    Cron {
        #[command(subcommand)]
        command: CronCommands,
    },
    /// Manage local AgentLIBRE memory.
    #[command(long_about = help::MEMORY)]
    Memory {
        #[command(subcommand)]
        command: MemoryCommands,
    },
    /// Manage local AgentLIBRE notes.
    #[command(long_about = help::NOTES)]
    Notes {
        #[command(subcommand)]
        command: NotesCommands,
    },
    /// Initialize the repo-local AgentLIBRE workspace.
    #[command(long_about = help::INIT)]
    Init(RepoInitArgs),
    /// Run one prompt and print the final answer.
    #[command(long_about = help::RUN)]
    Run(RunArgs),
    /// Start an interactive chat session.
    #[command(long_about = help::CHAT)]
    Chat(ChatArgs),
    /// Run the local agent runtime daemon in the foreground.
    #[command(long_about = help::SERVE)]
    Serve(ServeArgs),
    /// Report repo-local AgentLIBRE workspace status.
    #[command(long_about = help::STATUS)]
    Status(RepoStatusArgs),
    /// Inspect and verify AgentLIBRE skills.
    #[command(long_about = help::SKILL)]
    Skill {
        #[command(subcommand)]
        command: SkillCommands,
    },
    /// Install AgentLIBRE git hooks for this repository.
    #[command(long_about = help::INSTALL_HOOKS)]
    InstallHooks(RepoHooksArgs),
    /// Advanced repo workspace commands.
    #[command(hide = true, long_about = help::REPO)]
    Repo {
        #[command(subcommand)]
        command: RepoCommands,
    },
    /// Advanced daemon commands.
    #[command(hide = true, long_about = help::DAEMON)]
    Daemon {
        #[command(subcommand)]
        command: DaemonCommands,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommands {
    /// Print resolved config, data, state, cache, log, and session paths.
    #[command(long_about = help::CONFIG_PATHS)]
    Paths,
    /// Report runtime config, local inference profile, logs, and repair hints.
    #[command(long_about = help::CONFIG_STATUS)]
    Status(ConfigStatusArgs),
    /// Write a default runtime config.
    #[command(long_about = help::CONFIG_INIT)]
    Init {
        /// Overwrite an existing runtime config.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
enum StoreCommands {
    /// Report local store health.
    #[command(long_about = help::STORE_STATUS)]
    Status(StoreStatusArgs),
    /// Run local store migrations.
    #[command(long_about = help::STORE_MIGRATE)]
    Migrate(StoreMigrateArgs),
    /// Export one store domain as JSONL records.
    #[command(long_about = help::STORE_EXPORT)]
    Export(StoreExportArgs),
}

#[derive(Debug, Subcommand)]
enum FunctionCommands {
    /// List workspace and global agentFUNCTIONs.
    #[command(long_about = help::FUNCTION_LIST)]
    List(FunctionListArgs),
    /// Show one resolved agentFUNCTION.
    #[command(long_about = help::FUNCTION_SHOW)]
    Show(FunctionShowArgs),
    /// Validate one agentFUNCTION without running inference.
    #[command(long_about = help::FUNCTION_STATUS)]
    Status(FunctionStatusArgs),
    /// Create a starter agentFUNCTION.
    #[command(long_about = help::FUNCTION_INIT)]
    Init(FunctionInitArgs),
    /// Validate and print doctor hints for one agentFUNCTION.
    #[command(long_about = help::FUNCTION_DOCTOR)]
    Doctor(FunctionDoctorArgs),
}

#[derive(Debug, Subcommand)]
enum InferenceCommands {
    /// Run one direct inference prompt and print the final answer.
    #[command(long_about = help::INFERENCE_RUN)]
    Run(InferenceRunArgs),
    /// Start a direct inference chat session.
    #[command(long_about = help::INFERENCE_CHAT)]
    Chat(InferenceChatArgs),
    /// Run the direct inference daemon in the foreground.
    #[command(long_about = help::INFERENCE_SERVE)]
    Serve(InferenceServeArgs),
}

#[derive(Debug, Subcommand)]
enum CronCommands {
    /// Add a scheduled job.
    #[command(long_about = help::CRON_ADD)]
    Add(CronAddArgs),
    /// List scheduled jobs.
    #[command(long_about = help::CRON_LIST)]
    List(CronListArgs),
    /// Show one scheduled job.
    #[command(long_about = help::CRON_SHOW)]
    Show(CronShowArgs),
    /// Enable a scheduled job.
    #[command(long_about = help::CRON_ENABLE)]
    Enable(CronEnableArgs),
    /// Disable a scheduled job.
    #[command(long_about = help::CRON_DISABLE)]
    Disable(CronDisableArgs),
    /// Run a scheduled job once.
    #[command(long_about = help::CRON_RUN)]
    Run(CronRunArgs),
    /// Run one scheduler tick.
    #[command(hide = true, long_about = help::CRON_TICK)]
    Tick(CronTickArgs),
    /// Show run history for one scheduled job.
    #[command(long_about = help::CRON_HISTORY)]
    History(CronHistoryArgs),
    /// Tombstone a scheduled job.
    #[command(long_about = help::CRON_DELETE)]
    Delete(CronDeleteArgs),
}

#[derive(Debug, Subcommand)]
enum RepoCommands {
    /// Initialize the repo-local AgentLIBRE workspace.
    #[command(long_about = help::REPO_INIT)]
    Init(RepoInitArgs),
    /// Initialize a declared submodule component.
    #[command(long_about = help::REPO_INIT_COMPONENT)]
    InitComponent(RepoComponentInitArgs),
    /// Apply an explicit workspace profile file.
    #[command(hide = true, long_about = help::REPO_IMPORT_PROFILE)]
    ImportProfile(RepoImportProfileArgs),
    /// Report repo-local AgentLIBRE workspace status.
    #[command(long_about = help::REPO_STATUS)]
    Status(RepoStatusArgs),
    /// Verify planned task overview files in the tasks component.
    #[command(long_about = help::REPO_VERIFY_TASKS)]
    VerifyTasks(TaskSpecVerifyArgs),
    /// Inspect and create declared .agl artifacts.
    #[command(long_about = help::REPO_ARTIFACT)]
    Artifact {
        #[command(subcommand)]
        command: ArtifactCommands,
    },
    /// Install AgentLIBRE git hooks for this repository.
    #[command(long_about = help::REPO_INSTALL_HOOKS)]
    InstallHooks(RepoHooksArgs),
    /// Export a portable workspace profile manifest.
    #[command(long_about = help::REPO_EXPORT_PROFILE)]
    ExportProfile(RepoExportProfileArgs),
}

#[derive(Debug, Subcommand)]
enum ArtifactCommands {
    /// Report declared .agl artifact status.
    #[command(long_about = help::REPO_ARTIFACT_STATUS)]
    Status(ArtifactStatusArgs),
    /// Verify declared .agl artifacts.
    #[command(long_about = help::REPO_ARTIFACT_VERIFY)]
    Verify(ArtifactStatusArgs),
    /// Create missing declared artifact directories.
    #[command(long_about = help::REPO_ARTIFACT_SYNC)]
    Sync(ArtifactSyncArgs),
    /// Write the artifact lock file.
    #[command(long_about = help::REPO_ARTIFACT_LOCK)]
    Lock(ArtifactLockArgs),
}

#[derive(Debug, Subcommand)]
enum MemoryCommands {
    /// Add an explicit memory entry.
    #[command(long_about = help::MEMORY_ADD)]
    Add(MemoryAddArgs),
    /// List memory entries in one scope.
    #[command(long_about = help::MEMORY_LIST)]
    List(MemoryListArgs),
    /// Search memory entries in one scope.
    #[command(long_about = help::MEMORY_SEARCH)]
    Search(MemorySearchArgs),
    /// Show one memory entry.
    #[command(long_about = help::MEMORY_SHOW)]
    Show(MemoryShowArgs),
    /// Tombstone one memory entry.
    #[command(long_about = help::MEMORY_DELETE)]
    Delete(MemoryDeleteArgs),
    /// Create a pending memory suggestion.
    #[command(long_about = help::MEMORY_SUGGEST)]
    Suggest(MemorySuggestArgs),
    /// List memory suggestions.
    #[command(long_about = help::MEMORY_LIST_SUGGESTIONS)]
    ListSuggestions(MemoryListSuggestionsArgs),
    /// Approve a pending memory suggestion.
    #[command(long_about = help::MEMORY_APPROVE)]
    Approve(MemoryApproveArgs),
    /// Reject a pending memory suggestion.
    #[command(long_about = help::MEMORY_REJECT)]
    Reject(MemoryRejectArgs),
}

#[derive(Debug, Subcommand)]
enum NotesCommands {
    /// Add a note.
    #[command(long_about = help::NOTES_ADD)]
    Add(NotesAddArgs),
    /// List notes.
    #[command(long_about = help::NOTES_LIST)]
    List(NotesListArgs),
    /// Search notes.
    #[command(long_about = help::NOTES_SEARCH)]
    Search(NotesSearchArgs),
    /// Show one note.
    #[command(long_about = help::NOTES_SHOW)]
    Show(NotesShowArgs),
    /// Update a note.
    #[command(long_about = help::NOTES_UPDATE)]
    Update(NotesUpdateArgs),
    /// Tombstone one note.
    #[command(long_about = help::NOTES_DELETE)]
    Delete(NotesDeleteArgs),
    /// Link a note to another local reference.
    #[command(long_about = help::NOTES_LINK)]
    Link(NotesLinkArgs),
    /// Promote a note into memory.
    #[command(long_about = help::NOTES_REMEMBER)]
    Remember(NotesRememberArgs),
}

#[derive(Debug, Subcommand)]
enum SkillCommands {
    /// Initialize the workspace skills submodule declared in .agl/workspace.toml.
    #[command(long_about = help::SKILL_INIT)]
    Init(SkillInitArgs),
    /// List core and workspace skills.
    #[command(long_about = help::SKILL_LIST)]
    List(SkillListArgs),
    /// Inspect one skill by name.
    #[command(long_about = help::SKILL_INSPECT)]
    Inspect(SkillInspectArgs),
    /// Report workspace skill component and lock status.
    #[command(long_about = help::SKILL_STATUS)]
    Status(SkillStatusArgs),
    /// Verify workspace skills and lock state.
    #[command(long_about = help::SKILL_VERIFY)]
    Verify(SkillVerifyArgs),
    /// Create missing writable folders declared by workspace skills.
    #[command(long_about = help::SKILL_SYNC_FOLDERS)]
    SyncFolders(SkillFolderSyncArgs),
    /// Write or preview .agl/skills.lock.
    #[command(long_about = help::SKILL_LOCK)]
    Lock(SkillLockArgs),
    /// Locally approve a locked workspace skill identity.
    #[command(long_about = help::SKILL_TRUST)]
    Trust(SkillTrustArgs),
    /// Revoke local approval for a workspace skill identity.
    #[command(long_about = help::SKILL_REVOKE)]
    Revoke(SkillRevokeArgs),
}

#[derive(Debug, Subcommand)]
enum DaemonCommands {
    /// Report local agent runtime daemon status.
    #[command(long_about = help::DAEMON_STATUS)]
    Status(StatusArgs),
}

#[derive(Debug, Args)]
struct RepoInitArgs {
    /// Repo workflow profile to initialize.
    #[arg(long, default_value = "repo-workflow")]
    profile: String,

    /// Local workspace profile manifest to apply.
    #[arg(long, value_name = "PATH")]
    profile_file: Option<PathBuf>,

    /// Generic artifact source override as NAME=URL[@REV].
    #[arg(long = "artifact-source", value_name = "NAME=URL[@REV]")]
    artifact_sources: Vec<String>,

    /// Skills repository URL for the .agl/skills submodule.
    #[arg(long, value_name = "URL")]
    skills_url: Option<String>,

    /// Skills repository revision to pin in the workspace manifest.
    #[arg(long, value_name = "REV")]
    skills_rev: Option<String>,

    /// Task/spec repository URL for the .agl/tasks submodule.
    #[arg(long, value_name = "URL")]
    tasks_url: Option<String>,

    /// Task/spec repository revision to pin in the workspace manifest.
    #[arg(long, value_name = "REV", requires = "tasks_url")]
    tasks_rev: Option<String>,

    /// Print planned changes without writing files.
    #[arg(long)]
    dry_run: bool,

    /// Repair or replace AgentLIBRE-managed files.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct ConfigStatusArgs {
    /// Local inference config TOML path to inspect.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Treat missing or invalid runtime/inference config as a failure.
    #[arg(long)]
    strict: bool,
}

#[derive(Debug, Args)]
struct RepoImportProfileArgs {
    /// Local workspace profile manifest to apply.
    #[arg(long, value_name = "PATH")]
    profile_file: PathBuf,

    /// Print planned changes without writing files.
    #[arg(long)]
    dry_run: bool,

    /// Repair or replace AgentLIBRE-managed files.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct RepoComponentInitArgs {
    /// Workspace component name to initialize.
    #[arg(value_name = "NAME")]
    component: String,

    /// Print planned git operations without writing.
    #[arg(long)]
    dry_run: bool,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RepoStatusArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Limit status to one component.
    #[arg(long, value_name = "NAME")]
    component: Option<String>,

    /// Treat warnings as failures.
    #[arg(long)]
    strict: bool,
}

#[derive(Debug, Args)]
struct TaskSpecVerifyArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Treat warnings as failures.
    #[arg(long)]
    strict: bool,
}

#[derive(Debug, Args)]
struct ArtifactStatusArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Limit status to one artifact ID.
    #[arg(long, value_name = "ID")]
    artifact: Option<String>,

    /// Treat warnings as failures.
    #[arg(long)]
    strict: bool,
}

#[derive(Debug, Args)]
struct ArtifactSyncArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Print planned directory creation without writing.
    #[arg(long)]
    dry_run: bool,

    /// Treat warnings as failures.
    #[arg(long)]
    strict: bool,
}

#[derive(Debug, Args)]
struct ArtifactLockArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Print the lock payload without writing.
    #[arg(long)]
    dry_run: bool,

    /// Treat warnings as failures.
    #[arg(long)]
    strict: bool,
}

#[derive(Debug, Args)]
struct RepoHooksArgs {
    /// Print planned hook changes without writing files.
    #[arg(long)]
    dry_run: bool,

    /// Replace AgentLIBRE-managed hooks or overwrite conflicts.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct RepoExportProfileArgs {
    /// Destination workspace profile TOML path.
    #[arg(long, value_name = "PATH")]
    out: PathBuf,

    /// Overwrite an existing output file.
    #[arg(long)]
    force: bool,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct StoreStatusArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct StoreMigrateArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct StoreExportArgs {
    /// Domain to export. Domains may include multiple record_type values.
    #[arg(long, value_enum)]
    domain: StoreDomainArg,

    /// Destination JSONL path.
    #[arg(long, value_name = "PATH")]
    out: PathBuf,

    /// Include tombstoned records.
    #[arg(long)]
    include_deleted: bool,

    /// Overwrite an existing output file.
    #[arg(long)]
    force: bool,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct FunctionListArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct FunctionShowArgs {
    /// Function id or path to FUNCTION.md.
    #[arg(value_name = "ID_OR_PATH")]
    reference: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct FunctionStatusArgs {
    /// Function id or path to FUNCTION.md.
    #[arg(value_name = "ID_OR_PATH")]
    reference: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Treat warnings as failures.
    #[arg(long)]
    strict: bool,
}

#[derive(Debug, Args)]
struct FunctionInitArgs {
    /// Function id.
    #[arg(value_name = "ID")]
    id: String,

    /// Create the function in the current workspace .agl directory.
    #[arg(long)]
    workspace: bool,

    /// Named inference profile to reference.
    #[arg(long = "model-profile", value_name = "ID")]
    model_profile: Option<String>,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct FunctionDoctorArgs {
    /// Function id or path to FUNCTION.md.
    #[arg(value_name = "ID_OR_PATH")]
    reference: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CronAddArgs {
    /// Job name.
    #[arg(long, value_name = "TEXT")]
    name: String,

    /// Schedule, such as hourly, daily HH:MM, weekly mon HH:MM, or a 5-field cron expression.
    #[arg(long, value_name = "EXPR")]
    schedule: String,

    /// Trusted skill id to run.
    #[arg(long, value_name = "ID", conflicts_with = "builtin")]
    skill: Option<String>,

    /// Builtin cron target id to run.
    #[arg(long, value_name = "ID", conflicts_with = "skill")]
    builtin: Option<String>,

    /// Create the job disabled.
    #[arg(long)]
    disabled: bool,

    /// Timezone label for human schedules.
    #[arg(long, value_name = "TZ")]
    timezone: Option<String>,

    /// Optional notification reference, such as matrix-room:<room_id>.
    #[arg(long, value_name = "REF")]
    notify: Option<String>,

    /// Stored prompt used when this cron job executes a skill.
    #[arg(long, value_name = "TEXT")]
    prompt: Option<String>,

    /// Optional stored input appended to the cron prompt.
    #[arg(long, value_name = "TEXT")]
    input: Option<String>,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CronListArgs {
    /// Include tombstoned jobs.
    #[arg(long)]
    include_deleted: bool,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CronShowArgs {
    /// Cron job id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CronEnableArgs {
    /// Cron job id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CronDisableArgs {
    /// Cron job id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CronRunArgs {
    /// Cron job id.
    #[arg(value_name = "ID")]
    id: String,

    /// Run the job immediately.
    #[arg(long)]
    now: bool,

    /// Validate the job without recording or executing it.
    #[arg(long, conflicts_with = "now")]
    preflight: bool,

    /// Use deterministic mock execution for skill targets.
    #[arg(long, hide = true)]
    mock_skill_execution: bool,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CronTickArgs {
    /// Unix timestamp used for due-job calculation. Defaults to current time.
    #[arg(long, value_name = "SECONDS")]
    at: Option<u64>,

    /// Use deterministic mock execution for skill targets.
    #[arg(long, hide = true)]
    mock_skill_execution: bool,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CronHistoryArgs {
    /// Cron job id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CronDeleteArgs {
    /// Cron job id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SkillListArgs {
    /// Skills to list.
    #[arg(long, value_enum, default_value_t = SkillListSourceArg::All)]
    source: SkillListSourceArg,

    /// Only list skills currently usable by the runtime trust policy.
    #[arg(long)]
    trusted_only: bool,

    /// Maximum number of skills to print.
    #[arg(long, value_name = "N")]
    limit: Option<usize>,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SkillInitArgs {
    /// Print planned git operations without writing.
    #[arg(long)]
    dry_run: bool,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MemoryScopeArgs {
    /// Memory scope.
    #[arg(long, value_enum, default_value_t = MemoryScopeArg::User)]
    scope: MemoryScopeArg,

    /// Scope key. Defaults to `default` for user scope; required for repo, matrix-room, and matrix-user scopes.
    #[arg(long, value_name = "KEY")]
    scope_key: Option<String>,
}

#[derive(Debug, Args)]
struct MemoryAddArgs {
    #[command(flatten)]
    scope: MemoryScopeArgs,

    /// Memory kind.
    #[arg(long, value_enum, default_value_t = MemoryKindArg::Fact)]
    kind: MemoryKindArg,

    /// Memory title.
    #[arg(long, value_name = "TEXT")]
    title: String,

    /// Memory body.
    #[arg(long, value_name = "TEXT")]
    body: String,

    /// Optional source reference.
    #[arg(long, value_name = "REF")]
    source_ref: Option<String>,

    /// Confidence from 0 to 100.
    #[arg(long, value_name = "N", default_value_t = 100)]
    confidence: u8,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MemoryListArgs {
    #[command(flatten)]
    scope: MemoryScopeArgs,

    /// Include tombstoned entries.
    #[arg(long)]
    include_deleted: bool,

    /// Maximum entries to print.
    #[arg(long, value_name = "N", default_value_t = 50)]
    limit: usize,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MemorySearchArgs {
    #[command(flatten)]
    scope: MemoryScopeArgs,

    /// Include tombstoned entries.
    #[arg(long)]
    include_deleted: bool,

    /// Maximum entries to print.
    #[arg(long, value_name = "N", default_value_t = 50)]
    limit: usize,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Query text.
    #[arg(value_name = "QUERY")]
    query: String,
}

#[derive(Debug, Args)]
struct MemoryShowArgs {
    /// Memory entry id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MemoryDeleteArgs {
    /// Memory entry id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MemorySuggestArgs {
    #[command(flatten)]
    scope: MemoryScopeArgs,

    /// Suggested memory kind.
    #[arg(long, value_enum, default_value_t = MemoryKindArg::Fact)]
    kind: MemoryKindArg,

    /// Suggested memory title.
    #[arg(long, value_name = "TEXT")]
    title: String,

    /// Suggested memory body.
    #[arg(long, value_name = "TEXT")]
    body: String,

    /// Required source reference.
    #[arg(long, value_name = "REF")]
    source_ref: String,

    /// Confidence from 0 to 100.
    #[arg(long, value_name = "N", default_value_t = 100)]
    confidence: u8,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MemoryListSuggestionsArgs {
    #[command(flatten)]
    scope: MemoryScopeArgs,

    /// Suggestion status to list. Defaults to pending.
    #[arg(long, value_enum)]
    status: Option<MemorySuggestionStatusArg>,

    /// List suggestions across every scope.
    #[arg(long)]
    all_scopes: bool,

    /// Maximum suggestions to print.
    #[arg(long, value_name = "N", default_value_t = 50)]
    limit: usize,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MemoryApproveArgs {
    /// Memory suggestion id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct MemoryRejectArgs {
    /// Memory suggestion id.
    #[arg(value_name = "ID")]
    id: String,

    /// Optional rejection reason.
    #[arg(long, value_name = "TEXT")]
    reason: Option<String>,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct NotesAddArgs {
    /// Note title.
    #[arg(long, value_name = "TEXT")]
    title: String,

    /// Note body.
    #[arg(long, value_name = "TEXT")]
    body: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct NotesListArgs {
    /// Include tombstoned notes.
    #[arg(long)]
    include_deleted: bool,

    /// Maximum notes to print.
    #[arg(long, value_name = "N", default_value_t = 50)]
    limit: usize,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct NotesSearchArgs {
    /// Include tombstoned notes.
    #[arg(long)]
    include_deleted: bool,

    /// Maximum notes to print.
    #[arg(long, value_name = "N", default_value_t = 50)]
    limit: usize,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Query text.
    #[arg(value_name = "QUERY")]
    query: String,
}

#[derive(Debug, Args)]
struct NotesShowArgs {
    /// Note id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct NotesUpdateArgs {
    /// Note id.
    #[arg(value_name = "ID")]
    id: String,

    /// New note title.
    #[arg(long, value_name = "TEXT")]
    title: Option<String>,

    /// New note body.
    #[arg(long, value_name = "TEXT")]
    body: Option<String>,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct NotesDeleteArgs {
    /// Note id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct NotesLinkArgs {
    /// Note id.
    #[arg(value_name = "ID")]
    id: String,

    /// Target reference, such as memory:<id> or task:<id>.
    #[arg(long = "to", value_name = "REF")]
    target_ref: String,

    /// Link label.
    #[arg(long, value_name = "TEXT")]
    label: Option<String>,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct NotesRememberArgs {
    #[command(flatten)]
    scope: MemoryScopeArgs,

    /// Memory kind for the promoted note.
    #[arg(long, value_enum, default_value_t = MemoryKindArg::WorkingNote)]
    kind: MemoryKindArg,

    /// Note id.
    #[arg(value_name = "ID")]
    id: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SkillInspectArgs {
    /// Skill name to inspect.
    #[arg(value_name = "NAME")]
    name: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Require the skill to be injectable by the runtime now.
    #[arg(long)]
    runtime: bool,
}

#[derive(Debug, Args)]
struct SkillStatusArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Treat warnings as failures.
    #[arg(long)]
    strict: bool,
}

#[derive(Debug, Args)]
struct SkillVerifyArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SkillFolderSyncArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Print planned folder changes without writing files.
    #[arg(long)]
    dry_run: bool,

    /// Folder creation reason to run.
    #[arg(long, value_enum, default_value_t = SkillFolderSyncSituationArg::SkillSync)]
    when: SkillFolderSyncSituationArg,
}

#[derive(Debug, Args)]
struct SkillLockArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Print planned lock changes without writing files.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct SkillTrustArgs {
    /// Workspace skill name to trust.
    #[arg(value_name = "NAME")]
    name: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,

    /// Approve after reviewing the printed git identity.
    #[arg(long)]
    yes: bool,
}

#[derive(Debug, Args)]
struct SkillRevokeArgs {
    /// Workspace skill name to revoke.
    #[arg(value_name = "NAME")]
    name: String,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CommonRunArgs {
    /// Local inference config TOML path.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// agentFUNCTION id or path to load for this turn/session.
    #[arg(long = "function", value_name = "ID_OR_PATH")]
    function_ref: Option<String>,

    /// Inference artifact root directory.
    #[arg(long, value_name = "DIR")]
    artifact_root: Option<PathBuf>,

    /// Workspace root for filesystem tools.
    #[arg(long, value_name = "DIR")]
    workspace_root: Option<PathBuf>,

    /// Maximum response tokens.
    #[arg(long, value_name = "N")]
    max_output_tokens: Option<u32>,

    /// Filesystem tool access mode.
    #[arg(long, value_enum)]
    tool_mode: Option<ToolAccessMode>,

    /// Core or trusted workspace skill id to inject for this turn/session.
    #[arg(long = "skill", value_name = "ID")]
    skills: Vec<String>,

    /// Inject explicit user memory into the model context.
    #[arg(long)]
    memory: bool,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[command(flatten)]
    common: CommonRunArgs,

    /// Prompt text.
    #[arg(long = "prompt", value_name = "TEXT", conflicts_with = "prompt")]
    prompt_option: Option<String>,

    /// Prompt text.
    #[arg(value_name = "PROMPT", num_args = 1.., trailing_var_arg = true)]
    prompt: Vec<String>,
}

#[derive(Debug, Args)]
struct ChatArgs {
    #[command(flatten)]
    common: CommonRunArgs,

    /// Resume or write a specific chat session id.
    #[arg(long, value_name = "ID")]
    session_id: Option<SessionId>,

    /// Start a new chat session even when a session id is configured.
    #[arg(long)]
    new_session: bool,

    /// Disable persisted chat history for this process.
    #[arg(long)]
    no_history: bool,
}

#[derive(Debug, Args)]
struct ServeArgs {
    #[command(flatten)]
    common: CommonRunArgs,

    /// Unix socket path for the daemon.
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct CommonInferenceArgs {
    /// Local inference config TOML path.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Inference artifact root directory.
    #[arg(long, value_name = "DIR")]
    artifact_root: Option<PathBuf>,

    /// Workspace root for filesystem tools.
    #[arg(long, value_name = "DIR")]
    workspace_root: Option<PathBuf>,

    /// Maximum response tokens.
    #[arg(long, value_name = "N")]
    max_output_tokens: Option<u32>,

    /// Filesystem tool access mode.
    #[arg(long, value_enum)]
    tool_mode: Option<ToolAccessMode>,

    /// Core or trusted workspace skill id to inject for this turn/session.
    #[arg(long = "skill", value_name = "ID")]
    skills: Vec<String>,

    /// Inject explicit user memory into the model context.
    #[arg(long)]
    memory: bool,
}

#[derive(Debug, Args)]
struct InferenceRunArgs {
    #[command(flatten)]
    common: CommonInferenceArgs,

    /// Prompt text.
    #[arg(long = "prompt", value_name = "TEXT", conflicts_with = "prompt")]
    prompt_option: Option<String>,

    /// Prompt text.
    #[arg(value_name = "PROMPT", num_args = 1.., trailing_var_arg = true)]
    prompt: Vec<String>,
}

#[derive(Debug, Args)]
struct InferenceChatArgs {
    #[command(flatten)]
    common: CommonInferenceArgs,

    /// Resume or write a specific chat session id.
    #[arg(long, value_name = "ID")]
    session_id: Option<SessionId>,

    /// Start a new chat session even when a session id is configured.
    #[arg(long)]
    new_session: bool,

    /// Disable persisted chat history for this process.
    #[arg(long)]
    no_history: bool,
}

#[derive(Debug, Args)]
struct InferenceServeArgs {
    #[command(flatten)]
    common: CommonInferenceArgs,

    /// Unix socket path for the daemon.
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct StatusArgs {
    /// Unix socket path for the daemon.
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
}

pub(crate) fn parse_cli(args: impl IntoIterator<Item = String>) -> Result<CliInvocation> {
    let args = args.into_iter().collect::<Vec<_>>();
    let display_name = cli_display_name();
    let command = Cli::command().name(display_name).bin_name(display_name);

    match command.try_get_matches_from(args) {
        Ok(matches) => Cli::from_arg_matches(&matches)
            .map_err(anyhow::Error::from)
            .and_then(|cli| cli.into_invocation(display_name)),
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            err.print().context("failed to print CLI help")?;
            Ok(CliInvocation {
                command: CliCommand::HelpPrinted,
                home: None,
            })
        }
        Err(err) => Err(err.into()),
    }
}

impl Cli {
    fn into_invocation(self, display_name: &'static str) -> Result<CliInvocation> {
        let command = match self.command {
            Some(Commands::Completion { shell }) => CliCommand::Completion { shell },
            Some(Commands::Config { command }) => CliCommand::Config(match command {
                ConfigCommands::Paths => ConfigCommand::Paths,
                ConfigCommands::Status(args) => ConfigCommand::Status(ConfigStatusOptions {
                    config: args.config,
                    json: args.json,
                    strict: args.strict,
                }),
                ConfigCommands::Init { force } => ConfigCommand::Init { force },
            }),
            Some(Commands::Store { command }) => CliCommand::Store(store_command(command)),
            Some(Commands::Function { command }) => {
                CliCommand::Function(function_command(command)?)
            }
            Some(Commands::Inference { command }) => {
                CliCommand::Inference(inference_command(command)?)
            }
            Some(Commands::Cron { command }) => CliCommand::Cron(cron_command(command)?),
            Some(Commands::Memory { command }) => CliCommand::Memory(memory_command(command)?),
            Some(Commands::Notes { command }) => CliCommand::Notes(notes_command(command)?),
            Some(Commands::Init(args)) => CliCommand::Init(repo_init_options(args)?),
            Some(Commands::Run(args)) => CliCommand::Run(run_options_from_args(args)?),
            Some(Commands::Chat(args)) => CliCommand::Chat(chat_options_from_args(args)?),
            Some(Commands::Serve(args)) => CliCommand::Serve(serve_options_from_args(args)?),
            Some(Commands::Status(args)) => {
                CliCommand::Repo(RepoCommand::Status(repo_status_options(args)))
            }
            Some(Commands::Skill { command }) => CliCommand::Skill(skill_command(command)),
            Some(Commands::InstallHooks(args)) => {
                CliCommand::Repo(RepoCommand::InstallHooks(repo_hooks_options(args)))
            }
            Some(Commands::Repo { command }) => CliCommand::Repo(match command {
                RepoCommands::Init(args) => RepoCommand::Init(repo_init_options(args)?),
                RepoCommands::InitComponent(args) => {
                    RepoCommand::InitComponent(repo_component_init_options(args))
                }
                RepoCommands::ImportProfile(args) => {
                    RepoCommand::ImportProfile(repo_import_profile_options(args))
                }
                RepoCommands::Status(args) => RepoCommand::Status(repo_status_options(args)),
                RepoCommands::VerifyTasks(args) => {
                    RepoCommand::VerifyTasks(task_spec_verify_options(args))
                }
                RepoCommands::Artifact { command } => {
                    RepoCommand::Artifact(artifact_command(command))
                }
                RepoCommands::InstallHooks(args) => {
                    RepoCommand::InstallHooks(repo_hooks_options(args))
                }
                RepoCommands::ExportProfile(args) => {
                    RepoCommand::ExportProfile(repo_export_profile_options(args))
                }
            }),
            Some(Commands::Daemon { command }) => match command {
                DaemonCommands::Status(args) => CliCommand::DaemonStatus(DaemonStatusOptions {
                    socket_path: args.socket,
                }),
            },
            None if self.prompt.is_empty() => CliCommand::Help {
                bin_name: display_name,
            },
            None => top_level_prompt_command(self.prompt)?,
        };

        Ok(CliInvocation {
            command,
            home: self.home,
        })
    }
}

fn repo_init_options(args: RepoInitArgs) -> Result<RepoInitOptions> {
    Ok(RepoInitOptions {
        profile: args.profile,
        profile_file: args.profile_file,
        artifact_sources: args
            .artifact_sources
            .iter()
            .map(|source| parse_repo_artifact_source(source))
            .collect::<Result<Vec<_>>>()?,
        skills_url: args.skills_url,
        skills_rev: args.skills_rev,
        tasks_url: args.tasks_url,
        tasks_rev: args.tasks_rev,
        dry_run: args.dry_run,
        force: args.force,
    })
}

fn parse_repo_artifact_source(input: &str) -> Result<RepoArtifactSourceOverride> {
    let Some((name, source)) = input.split_once('=') else {
        bail!("artifact source must be NAME=URL[@REV]: {input}");
    };
    let name = name.trim();
    if name.is_empty() {
        bail!("artifact source name cannot be blank: {input}");
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        bail!("artifact source name contains invalid characters: {name}");
    }

    let source = source.trim();
    if source.is_empty() {
        bail!("artifact source URL cannot be blank: {input}");
    }

    let split_index = source.rfind('@').filter(|index| {
        let boundary = source.rfind(['/', ':']).unwrap_or(0);
        *index > boundary
    });
    let (url, rev) = if let Some(index) = split_index {
        let (url, rev) = source.split_at(index);
        let rev = rev.trim_start_matches('@').trim();
        if rev.is_empty() {
            bail!("artifact source revision cannot be blank: {input}");
        }
        (url.trim().to_string(), Some(rev.to_string()))
    } else {
        (source.to_string(), None)
    };

    if url.is_empty() {
        bail!("artifact source URL cannot be blank: {input}");
    }

    Ok(RepoArtifactSourceOverride {
        name: name.to_string(),
        url,
        rev,
    })
}

fn repo_component_init_options(args: RepoComponentInitArgs) -> RepoComponentInitOptions {
    RepoComponentInitOptions {
        component: args.component,
        dry_run: args.dry_run,
        json: args.json,
    }
}

fn repo_status_options(args: RepoStatusArgs) -> RepoStatusOptions {
    RepoStatusOptions {
        json: args.json,
        component: args.component,
        strict: args.strict,
    }
}

fn task_spec_verify_options(args: TaskSpecVerifyArgs) -> TaskSpecVerifyOptions {
    TaskSpecVerifyOptions {
        json: args.json,
        strict: args.strict,
    }
}

fn artifact_command(command: ArtifactCommands) -> ArtifactCommand {
    match command {
        ArtifactCommands::Status(args) => ArtifactCommand::Status(artifact_status_options(args)),
        ArtifactCommands::Verify(args) => ArtifactCommand::Verify(artifact_status_options(args)),
        ArtifactCommands::Sync(args) => ArtifactCommand::Sync(ArtifactSyncOptions {
            json: args.json,
            dry_run: args.dry_run,
            strict: args.strict,
        }),
        ArtifactCommands::Lock(args) => ArtifactCommand::Lock(ArtifactLockOptions {
            json: args.json,
            dry_run: args.dry_run,
            strict: args.strict,
        }),
    }
}

fn artifact_status_options(args: ArtifactStatusArgs) -> ArtifactStatusOptions {
    ArtifactStatusOptions {
        json: args.json,
        artifact: args.artifact,
        strict: args.strict,
    }
}

fn repo_hooks_options(args: RepoHooksArgs) -> RepoHooksOptions {
    RepoHooksOptions {
        dry_run: args.dry_run,
        force: args.force,
    }
}

fn repo_export_profile_options(args: RepoExportProfileArgs) -> RepoExportProfileOptions {
    RepoExportProfileOptions {
        out: args.out,
        force: args.force,
        json: args.json,
    }
}

fn repo_import_profile_options(args: RepoImportProfileArgs) -> RepoImportProfileOptions {
    RepoImportProfileOptions {
        profile_file: args.profile_file,
        dry_run: args.dry_run,
        force: args.force,
    }
}

fn store_command(command: StoreCommands) -> StoreCommand {
    match command {
        StoreCommands::Status(args) => StoreCommand::Status(StoreStatusOptions { json: args.json }),
        StoreCommands::Migrate(args) => {
            StoreCommand::Migrate(StoreMigrateOptions { json: args.json })
        }
        StoreCommands::Export(args) => StoreCommand::Export(StoreExportCliOptions {
            domain: args.domain,
            out: args.out,
            include_deleted: args.include_deleted,
            force: args.force,
            json: args.json,
        }),
    }
}

fn function_command(command: FunctionCommands) -> Result<FunctionCommand> {
    Ok(match command {
        FunctionCommands::List(args) => {
            FunctionCommand::List(FunctionListOptions { json: args.json })
        }
        FunctionCommands::Show(args) => {
            validate_prompt(&args.reference)?;
            FunctionCommand::Show(FunctionShowOptions {
                reference: args.reference,
                json: args.json,
            })
        }
        FunctionCommands::Status(args) => {
            validate_prompt(&args.reference)?;
            FunctionCommand::Status(FunctionStatusOptions {
                reference: args.reference,
                json: args.json,
                strict: args.strict,
            })
        }
        FunctionCommands::Init(args) => {
            agl_functions::validate_function_id("function id", &args.id)?;
            if let Some(profile) = &args.model_profile {
                agl_functions::validate_function_id("model profile", profile)?;
            }
            FunctionCommand::Init(FunctionInitOptions {
                id: args.id,
                workspace: args.workspace,
                model_profile: args.model_profile,
                json: args.json,
            })
        }
        FunctionCommands::Doctor(args) => {
            validate_prompt(&args.reference)?;
            FunctionCommand::Doctor(FunctionDoctorOptions {
                reference: args.reference,
                json: args.json,
            })
        }
    })
}

fn inference_command(command: InferenceCommands) -> Result<InferenceCommand> {
    Ok(match command {
        InferenceCommands::Run(args) => {
            InferenceCommand::Run(inference_run_options_from_args(args)?)
        }
        InferenceCommands::Chat(args) => {
            InferenceCommand::Chat(inference_chat_options_from_args(args)?)
        }
        InferenceCommands::Serve(args) => {
            InferenceCommand::Serve(inference_serve_options_from_args(args)?)
        }
    })
}

fn cron_command(command: CronCommands) -> Result<CronCommand> {
    Ok(match command {
        CronCommands::Add(args) => {
            if let Some(prompt) = &args.prompt {
                validate_prompt(prompt)?;
            }
            if let Some(input) = &args.input {
                validate_prompt(input)?;
            }
            let target = cron_target(args.skill, args.builtin)?;
            if target.kind == CronTargetKindArg::Skill && args.prompt.is_none() {
                bail!("--prompt is required when --skill is used");
            }
            CronCommand::Add(CronAddOptions {
                name: args.name,
                schedule: args.schedule,
                target,
                enabled: !args.disabled,
                timezone: args.timezone,
                notify_ref: args.notify,
                prompt: args.prompt,
                input: args.input,
                json: args.json,
            })
        }
        CronCommands::List(args) => CronCommand::List(CronListOptions {
            include_deleted: args.include_deleted,
            json: args.json,
        }),
        CronCommands::Show(args) => {
            validate_prompt(&args.id)?;
            CronCommand::Show(CronShowOptions {
                id: args.id,
                json: args.json,
            })
        }
        CronCommands::Enable(args) => {
            validate_prompt(&args.id)?;
            CronCommand::Enable(CronEnableOptions {
                id: args.id,
                json: args.json,
            })
        }
        CronCommands::Disable(args) => {
            validate_prompt(&args.id)?;
            CronCommand::Disable(CronDisableOptions {
                id: args.id,
                json: args.json,
            })
        }
        CronCommands::Run(args) => {
            validate_prompt(&args.id)?;
            if !args.now && !args.preflight {
                bail!(
                    "agl cron run requires --now or --preflight until daemon scheduling is enabled"
                );
            }
            CronCommand::Run(CronRunOptions {
                id: args.id,
                now: args.now,
                preflight: args.preflight,
                mock_skill_execution: args.mock_skill_execution,
                json: args.json,
            })
        }
        CronCommands::Tick(args) => CronCommand::Tick(CronTickOptions {
            at: args.at,
            mock_skill_execution: args.mock_skill_execution,
            json: args.json,
        }),
        CronCommands::History(args) => {
            validate_prompt(&args.id)?;
            CronCommand::History(CronHistoryOptions {
                id: args.id,
                json: args.json,
            })
        }
        CronCommands::Delete(args) => {
            validate_prompt(&args.id)?;
            CronCommand::Delete(CronDeleteOptions {
                id: args.id,
                json: args.json,
            })
        }
    })
}

fn cron_target(skill: Option<String>, builtin: Option<String>) -> Result<CronTargetArg> {
    match (skill, builtin) {
        (Some(skill), None) => {
            if let Err(err) = SkillId::new(skill.clone()) {
                bail!("--skill is invalid: {err}");
            }
            Ok(CronTargetArg {
                kind: CronTargetKindArg::Skill,
                target_ref: skill,
            })
        }
        (None, Some(builtin)) => {
            validate_prompt(&builtin)?;
            Ok(CronTargetArg {
                kind: CronTargetKindArg::Builtin,
                target_ref: builtin,
            })
        }
        (None, None) => bail!("exactly one of --skill or --builtin is required"),
        (Some(_), Some(_)) => bail!("--skill and --builtin cannot be used together"),
    }
}

fn memory_command(command: MemoryCommands) -> Result<MemoryCommand> {
    Ok(match command {
        MemoryCommands::Add(args) => MemoryCommand::Add(MemoryAddOptions {
            scope: args.scope.scope,
            scope_key: args.scope.scope_key,
            kind: args.kind,
            title: args.title,
            body: args.body,
            source_ref: args.source_ref,
            confidence: validate_confidence(args.confidence)?,
            json: args.json,
        }),
        MemoryCommands::List(args) => MemoryCommand::List(MemoryListOptions {
            scope: args.scope.scope,
            scope_key: args.scope.scope_key,
            include_deleted: args.include_deleted,
            limit: validate_limit(args.limit, "--limit")?,
            json: args.json,
        }),
        MemoryCommands::Search(args) => {
            validate_prompt(&args.query)?;
            MemoryCommand::Search(MemorySearchOptions {
                query: args.query,
                scope: args.scope.scope,
                scope_key: args.scope.scope_key,
                include_deleted: args.include_deleted,
                limit: validate_limit(args.limit, "--limit")?,
                json: args.json,
            })
        }
        MemoryCommands::Show(args) => {
            validate_prompt(&args.id)?;
            MemoryCommand::Show(MemoryShowOptions {
                id: args.id,
                json: args.json,
            })
        }
        MemoryCommands::Delete(args) => {
            validate_prompt(&args.id)?;
            MemoryCommand::Delete(MemoryDeleteOptions {
                id: args.id,
                json: args.json,
            })
        }
        MemoryCommands::Suggest(args) => {
            validate_prompt(&args.source_ref)?;
            MemoryCommand::Suggest(MemorySuggestOptions {
                scope: args.scope.scope,
                scope_key: args.scope.scope_key,
                kind: args.kind,
                title: args.title,
                body: args.body,
                source_ref: args.source_ref,
                confidence: validate_confidence(args.confidence)?,
                json: args.json,
            })
        }
        MemoryCommands::ListSuggestions(args) => {
            MemoryCommand::ListSuggestions(MemoryListSuggestionsOptions {
                scope: args.scope.scope,
                scope_key: args.scope.scope_key,
                status: args.status,
                all_scopes: args.all_scopes,
                limit: validate_limit(args.limit, "--limit")?,
                json: args.json,
            })
        }
        MemoryCommands::Approve(args) => {
            validate_prompt(&args.id)?;
            MemoryCommand::Approve(MemoryApproveOptions {
                id: args.id,
                json: args.json,
            })
        }
        MemoryCommands::Reject(args) => {
            validate_prompt(&args.id)?;
            if let Some(reason) = &args.reason {
                validate_prompt(reason)?;
            }
            MemoryCommand::Reject(MemoryRejectOptions {
                id: args.id,
                reason: args.reason,
                json: args.json,
            })
        }
    })
}

fn notes_command(command: NotesCommands) -> Result<NotesCommand> {
    Ok(match command {
        NotesCommands::Add(args) => NotesCommand::Add(NotesAddOptions {
            title: args.title,
            body: args.body,
            json: args.json,
        }),
        NotesCommands::List(args) => NotesCommand::List(NotesListOptions {
            include_deleted: args.include_deleted,
            limit: validate_limit(args.limit, "--limit")?,
            json: args.json,
        }),
        NotesCommands::Search(args) => {
            validate_prompt(&args.query)?;
            NotesCommand::Search(NotesSearchOptions {
                query: args.query,
                include_deleted: args.include_deleted,
                limit: validate_limit(args.limit, "--limit")?,
                json: args.json,
            })
        }
        NotesCommands::Show(args) => {
            validate_prompt(&args.id)?;
            NotesCommand::Show(NotesShowOptions {
                id: args.id,
                json: args.json,
            })
        }
        NotesCommands::Update(args) => {
            validate_prompt(&args.id)?;
            NotesCommand::Update(NotesUpdateOptions {
                id: args.id,
                title: args.title,
                body: args.body,
                json: args.json,
            })
        }
        NotesCommands::Delete(args) => {
            validate_prompt(&args.id)?;
            NotesCommand::Delete(NotesDeleteOptions {
                id: args.id,
                json: args.json,
            })
        }
        NotesCommands::Link(args) => {
            validate_prompt(&args.id)?;
            validate_prompt(&args.target_ref)?;
            NotesCommand::Link(NotesLinkOptions {
                id: args.id,
                target_ref: args.target_ref,
                label: args.label,
                json: args.json,
            })
        }
        NotesCommands::Remember(args) => {
            validate_prompt(&args.id)?;
            NotesCommand::Remember(NotesRememberOptions {
                id: args.id,
                scope: args.scope.scope,
                scope_key: args.scope.scope_key,
                kind: args.kind,
                json: args.json,
            })
        }
    })
}

fn skill_command(command: SkillCommands) -> SkillCommand {
    match command {
        SkillCommands::Init(args) => SkillCommand::Init(SkillInitOptions {
            dry_run: args.dry_run,
            json: args.json,
        }),
        SkillCommands::List(args) => SkillCommand::List(SkillListOptions {
            json: args.json,
            source: args.source,
            trusted_only: args.trusted_only,
            limit: args.limit,
        }),
        SkillCommands::Inspect(args) => SkillCommand::Inspect(SkillInspectOptions {
            name: args.name,
            json: args.json,
            runtime: args.runtime,
        }),
        SkillCommands::Status(args) => SkillCommand::Status(SkillStatusOptions {
            json: args.json,
            strict: args.strict,
        }),
        SkillCommands::Verify(args) => SkillCommand::Verify(SkillVerifyOptions { json: args.json }),
        SkillCommands::SyncFolders(args) => SkillCommand::SyncFolders(SkillFolderSyncOptions {
            json: args.json,
            dry_run: args.dry_run,
            when: args.when,
        }),
        SkillCommands::Lock(args) => SkillCommand::Lock(SkillLockOptions {
            json: args.json,
            dry_run: args.dry_run,
        }),
        SkillCommands::Trust(args) => SkillCommand::Trust(SkillTrustOptions {
            name: args.name,
            json: args.json,
            yes: args.yes,
        }),
        SkillCommands::Revoke(args) => SkillCommand::Revoke(SkillRevokeOptions {
            name: args.name,
            json: args.json,
        }),
    }
}

fn run_options_from_args(args: RunArgs) -> Result<RunOptions> {
    let prompt = args.prompt_option.or_else(|| {
        if args.prompt.is_empty() {
            None
        } else {
            Some(join_prompt(args.prompt))
        }
    });
    if let Some(prompt) = &prompt {
        validate_prompt(prompt)?;
    }

    Ok(RunOptions {
        prompt,
        ..run_options_from_common(args.common)?
    })
}

fn run_options_from_prompt(prompt: String) -> Result<RunOptions> {
    validate_prompt(&prompt)?;
    Ok(RunOptions {
        prompt: Some(prompt),
        ..RunOptions::default()
    })
}

fn chat_options_from_args(args: ChatArgs) -> Result<RunOptions> {
    if args.new_session && args.session_id.is_some() {
        bail!("--new-session cannot be used with --session-id");
    }

    Ok(RunOptions {
        session_id: args.session_id,
        no_history: args.no_history,
        new_session: args.new_session,
        ..run_options_from_common(args.common)?
    })
}

fn serve_options_from_args(args: ServeArgs) -> Result<ServeOptions> {
    Ok(ServeOptions {
        socket_path: args.socket,
        config: args.common.config,
        function_ref: args.common.function_ref,
        artifact_root: args.common.artifact_root,
        workspace_root: args.common.workspace_root,
        max_output_tokens: args
            .common
            .max_output_tokens
            .map(validate_max_output_tokens)
            .transpose()?,
        tool_mode: args.common.tool_mode,
        skills: validate_skill_ids(args.common.skills)?,
        memory: args.common.memory,
    })
}

fn inference_run_options_from_args(args: InferenceRunArgs) -> Result<RunOptions> {
    let prompt = args.prompt_option.or_else(|| {
        if args.prompt.is_empty() {
            None
        } else {
            Some(join_prompt(args.prompt))
        }
    });
    if let Some(prompt) = &prompt {
        validate_prompt(prompt)?;
    }

    Ok(RunOptions {
        prompt,
        ..run_options_from_inference_common(args.common)?
    })
}

fn inference_chat_options_from_args(args: InferenceChatArgs) -> Result<RunOptions> {
    if args.new_session && args.session_id.is_some() {
        bail!("--new-session cannot be used with --session-id");
    }

    Ok(RunOptions {
        session_id: args.session_id,
        no_history: args.no_history,
        new_session: args.new_session,
        ..run_options_from_inference_common(args.common)?
    })
}

fn inference_serve_options_from_args(args: InferenceServeArgs) -> Result<ServeOptions> {
    Ok(ServeOptions {
        socket_path: args.socket,
        config: args.common.config,
        function_ref: None,
        artifact_root: args.common.artifact_root,
        workspace_root: args.common.workspace_root,
        max_output_tokens: args
            .common
            .max_output_tokens
            .map(validate_max_output_tokens)
            .transpose()?,
        tool_mode: args.common.tool_mode,
        skills: validate_skill_ids(args.common.skills)?,
        memory: args.common.memory,
    })
}

fn run_options_from_common(common: CommonRunArgs) -> Result<RunOptions> {
    Ok(RunOptions {
        config: common.config,
        function_ref: common.function_ref,
        artifact_root: common.artifact_root,
        workspace_root: common.workspace_root,
        session_id: None,
        no_history: false,
        new_session: false,
        max_output_tokens: common
            .max_output_tokens
            .map(validate_max_output_tokens)
            .transpose()?,
        tool_mode: common.tool_mode,
        skills: validate_skill_ids(common.skills)?,
        memory: common.memory,
        prompt: None,
    })
}

fn run_options_from_inference_common(common: CommonInferenceArgs) -> Result<RunOptions> {
    Ok(RunOptions {
        config: common.config,
        function_ref: None,
        artifact_root: common.artifact_root,
        workspace_root: common.workspace_root,
        session_id: None,
        no_history: false,
        new_session: false,
        max_output_tokens: common
            .max_output_tokens
            .map(validate_max_output_tokens)
            .transpose()?,
        tool_mode: common.tool_mode,
        skills: validate_skill_ids(common.skills)?,
        memory: common.memory,
        prompt: None,
    })
}

fn validate_prompt(prompt: &str) -> Result<()> {
    if prompt.trim().is_empty() {
        bail!("prompt cannot be empty");
    }
    Ok(())
}

fn validate_max_output_tokens(value: u32) -> Result<u32> {
    if value == 0 {
        bail!("--max-output-tokens must be greater than zero");
    }
    Ok(value)
}

fn validate_limit(value: usize, flag: &str) -> Result<usize> {
    if value == 0 {
        bail!("{flag} must be greater than zero");
    }
    Ok(value)
}

fn validate_confidence(value: u8) -> Result<u8> {
    if value > 100 {
        bail!("--confidence must be between 0 and 100");
    }
    Ok(value)
}

fn validate_skill_ids(values: Vec<String>) -> Result<Vec<String>> {
    let mut seen = std::collections::BTreeSet::new();
    for value in &values {
        if let Err(err) = SkillId::new(value.clone()) {
            bail!("--skill is invalid: {err}");
        }
        if !seen.insert(value) {
            bail!("--skill is duplicated: {value}");
        }
    }
    Ok(values)
}

fn top_level_prompt_command(parts: Vec<String>) -> Result<CliCommand> {
    let first = parts.first().map(String::as_str);
    if matches!(
        first,
        Some("infer" | "generate" | "setup" | "doctor" | "model")
    ) {
        let name = first.expect("checked by matches");
        bail!("unknown command `{name}`. Use `agl run --prompt TEXT` for a one-shot prompt.");
    }
    Ok(CliCommand::Run(run_options_from_prompt(join_prompt(
        parts,
    ))?))
}

fn join_prompt(parts: Vec<String>) -> String {
    parts.join(" ")
}

pub(crate) fn print_usage(bin_name: &'static str) -> Result<()> {
    print!("{}", welcome_text(should_color_welcome()));
    println!();

    let mut command = Cli::command().name(bin_name).bin_name(bin_name);
    command.print_help().context("failed to print CLI help")?;
    println!();
    Ok(())
}

fn should_color_welcome() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

fn welcome_text(color: bool) -> String {
    if color {
        format!("\x1b[35m{}\x1b[0m", help::WELCOME)
    } else {
        help::WELCOME.to_string()
    }
}

pub(crate) fn print_completion(shell: Shell) {
    let mut command = PublicCompletionCli::command().name("agl").bin_name("agl");
    generate(shell, &mut command, "agl", &mut std::io::stdout());
}

fn cli_display_name() -> &'static str {
    CLI_DISPLAY_NAME
}

#[derive(Debug, Parser)]
#[command(
    name = "agl",
    bin_name = "agl",
    version,
    about = "agentLIBRE CLI - local-first agentic system"
)]
struct PublicCompletionCli {
    /// Override AGL_HOME for this invocation.
    #[arg(long, global = true, value_name = "DIR")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<PublicCompletionCommands>,

    /// Prompt text for a one-shot run.
    #[arg(value_name = "PROMPT", num_args = 1.., trailing_var_arg = true)]
    prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum PublicCompletionCommands {
    /// Write shell completion scripts to stdout.
    Completion {
        /// Shell to generate completions for.
        #[arg(value_enum, default_value_t = Shell::Bash)]
        shell: Shell,
    },
    /// Runtime configuration commands.
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Inspect and export the local AgentLIBRE store.
    Store {
        #[command(subcommand)]
        command: StoreCommands,
    },
    /// Inspect and create agentFUNCTION artifacts.
    Function {
        #[command(subcommand)]
        command: FunctionCommands,
    },
    /// Low-level direct local inference commands.
    Inference {
        #[command(subcommand)]
        command: InferenceCommands,
    },
    /// Manage local scheduled AgentLIBRE jobs.
    Cron {
        #[command(subcommand)]
        command: CronCommands,
    },
    /// Manage local AgentLIBRE memory.
    Memory {
        #[command(subcommand)]
        command: MemoryCommands,
    },
    /// Manage local AgentLIBRE notes.
    Notes {
        #[command(subcommand)]
        command: NotesCommands,
    },
    /// Initialize the repo-local AgentLIBRE workspace.
    Init(RepoInitArgs),
    /// Run one prompt and print the final answer.
    Run(RunArgs),
    /// Start an interactive chat session.
    Chat(ChatArgs),
    /// Run the local agent runtime daemon in the foreground.
    Serve(ServeArgs),
    /// Report repo-local AgentLIBRE workspace status.
    Status(RepoStatusArgs),
    /// Inspect and verify AgentLIBRE skills.
    Skill {
        #[command(subcommand)]
        command: SkillCommands,
    },
    /// Install AgentLIBRE git hooks for this repository.
    InstallHooks(RepoHooksArgs),
}

#[cfg(test)]
mod tests;
