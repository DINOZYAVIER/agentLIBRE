use std::path::PathBuf;

use agl_core::AgentRunId;
use clap::{Args, Parser, Subcommand};

pub(super) const APPLICATION_DESCRIPTION: &str =
    "agentLIBRE — the first agentic system in Milky Way galaxy";

#[derive(Debug, Parser)]
#[command(
    name = "agl",
    version,
    about = APPLICATION_DESCRIPTION
)]
pub(super) struct Cli {
    /// Override the Function reasoning default for new Runs in this invocation.
    #[arg(long, global = true, value_parser = parse_reasoning, value_name = "low|medium|xhigh")]
    pub(super) reasoning: Option<agl_core::agent::ReasoningEffort>,
    /// Function source directory for a new root chat.
    #[arg(long, value_name = "FUNCTION")]
    pub(super) function: Option<String>,
    #[command(subcommand)]
    pub(super) command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub(super) enum Command {
    /// Generate and inspect an implementation plan.
    Plan {
        #[command(subcommand)]
        command: PlanCommand,
    },
    /// Retain a Store explicitly before starting with an empty database.
    Store {
        #[command(subcommand)]
        command: StoreCommand,
    },
    /// Resume a durable Conversation.
    Resume(ResumeArgs),
    /// Start an interactive chat, optionally with an explicit Function.
    Chat(ChatArgs),
    /// Change Conversation display metadata.
    Conversation {
        #[command(subcommand)]
        command: ConversationCommand,
    },
    /// Run the long-lived local Agent service.
    Serve,
    /// Validate or activate the single human configuration file.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Inspect generated configuration, resources and active runs.
    Doctor,
    /// Manage declarative Function source.
    Function {
        #[command(subcommand)]
        command: FunctionCommand,
    },
    /// Manage Forge-owned workspace artifacts.
    Artifact {
        #[command(subcommand)]
        command: ArtifactCommand,
    },
    /// Run one foreground turn with an explicit Function.
    Run {
        function_directory: String,
        prompt: String,
    },
    /// Read the durable public projection of one AgentRun.
    View { run_id: AgentRunId },
    /// Request cancellation of one AgentRun.
    Cancel { run_id: AgentRunId },
}

#[derive(Debug, Subcommand)]
pub(super) enum PlanCommand {
    /// Generate a validated implementation-plan draft.
    Create {
        /// Planner Function directory or typed Function requirement.
        #[arg(long = "function", value_name = "PLANNER")]
        function: Option<String>,
        /// Task to investigate and decompose.
        prompt: String,
    },
    /// View a generated implementation-plan draft.
    View {
        plan_id: agl_core::implementation_plan::PlanId,
    },
    /// Explicitly approve the exact current draft digest.
    Approve {
        plan_id: agl_core::implementation_plan::PlanId,
        #[arg(long, value_name = "sha256:...")]
        digest: agl_core::implementation_plan::PlanDigest,
    },
    /// Execute approved slices sequentially in fresh coder Conversations.
    Implement {
        plan_id: agl_core::implementation_plan::PlanId,
        #[arg(long, value_name = "sha256:...")]
        digest: agl_core::implementation_plan::PlanDigest,
        /// Coder Function directory or typed Forge Function requirement.
        #[arg(long = "function", value_name = "CODER")]
        function: String,
    },
    /// View durable slice execution state.
    Status {
        plan_id: agl_core::implementation_plan::PlanId,
    },
}

#[derive(Debug, Args)]
pub(super) struct ChatArgs {
    /// Function directory or typed Function requirement.
    #[arg(value_name = "FUNCTION", conflicts_with = "function")]
    pub(super) function_directory: Option<String>,
    /// Function directory or typed Function requirement.
    #[arg(
        long = "function",
        value_name = "FUNCTION",
        conflicts_with = "function_directory"
    )]
    pub(super) function: Option<String>,
}

#[derive(Debug, Args)]
pub(super) struct ResumeArgs {
    pub(super) conversation: Option<String>,
    #[arg(long, conflicts_with = "conversation")]
    pub(super) last: bool,
    #[arg(long, conflicts_with = "conversation")]
    pub(super) all: bool,
}

#[derive(Debug, Subcommand)]
pub(super) enum ConversationCommand {
    /// Set the unique human name of one Conversation.
    Rename {
        conversation: String,
        new_name: String,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum FunctionCommand {
    /// Verify the Forge lock and resolve a Function identity.
    Lock { function_directory: PathBuf },
}

#[derive(Debug, Subcommand)]
pub(super) enum ArtifactCommand {
    /// Register an entity in the workspace Forge catalog and refresh its lock.
    Add {
        #[arg(long)]
        id: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        schema: String,
        #[arg(long = "git")]
        git_source: String,
        #[arg(long)]
        revision: String,
        #[arg(long = "path")]
        source_dir: String,
    },
}

#[derive(Debug, Subcommand)]
pub(super) enum StoreCommand {
    /// Preserve the current database and atomically create an empty Store.
    Rotate,
}

#[derive(Debug, Subcommand)]
pub(super) enum ConfigCommand {
    /// Validate source and print the exact proposed Function plans.
    Check {
        #[arg(long = "function", value_name = "FUNCTION")]
        function: Vec<String>,
    },
    /// Atomically write source locks and generated state, then reload the daemon.
    Apply {
        #[arg(long = "function", value_name = "FUNCTION")]
        function: Vec<String>,
    },
}

pub(super) fn parse_reasoning(value: &str) -> Result<agl_core::agent::ReasoningEffort, String> {
    use agl_core::agent::ReasoningEffort;
    match value {
        "low" => Ok(ReasoningEffort::Low),
        "medium" => Ok(ReasoningEffort::Medium),
        "xhigh" => Ok(ReasoningEffort::Xhigh),
        _ => Err("reasoning must be low, medium, or xhigh".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_artifact_add_contract() {
        let cli = Cli::try_parse_from([
            "agl",
            "artifact",
            "add",
            "--id",
            "agentlibre.memory",
            "--kind",
            "memory",
            "--schema",
            "agentlibre.memory/v1",
            "--git",
            "/tmp/source",
            "--revision",
            "HEAD",
            "--path",
            "memory",
        ])
        .unwrap();
        let Some(Command::Artifact {
            command:
                ArtifactCommand::Add {
                    id,
                    kind,
                    schema,
                    git_source,
                    revision,
                    source_dir,
                },
        }) = cli.command
        else {
            panic!("artifact add was not parsed");
        };
        assert_eq!(id, "agentlibre.memory");
        assert_eq!(kind, "memory");
        assert_eq!(schema, "agentlibre.memory/v1");
        assert_eq!(git_source, "/tmp/source");
        assert_eq!(revision, "HEAD");
        assert_eq!(source_dir, "memory");
    }
}
