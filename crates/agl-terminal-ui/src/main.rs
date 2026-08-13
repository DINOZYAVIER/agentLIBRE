use std::path::PathBuf;

use agl_terminal_ui::{InteractiveOptions, ToolAccessMode, UiRuntimeConfig};
use anyhow::Result;
use clap::{Parser, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "agl-terminal",
    version,
    about = "Interactive agentLIBRE terminal"
)]
struct Args {
    #[arg(long)]
    home: Option<PathBuf>,
    #[arg(long, value_name = "latest|SESSION_ID")]
    resume: Option<String>,
    #[arg(long)]
    no_input_history: bool,
    #[arg(long)]
    socket: Option<PathBuf>,
    #[arg(long = "workspace-root")]
    workspace: Option<PathBuf>,
    #[arg(long)]
    function: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long, value_enum)]
    mode: Option<Mode>,
    #[arg(long = "skill")]
    skills: Vec<String>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Mode {
    ReadOnly,
    Write,
    Execute,
    Approve,
    Admin,
}

impl From<Mode> for ToolAccessMode {
    fn from(value: Mode) -> Self {
        match value {
            Mode::ReadOnly => Self::ReadOnly,
            Mode::Write => Self::Write,
            Mode::Execute => Self::Execute,
            Mode::Approve => Self::Approve,
            Mode::Admin => Self::Admin,
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let runtime = match args.home {
        Some(home) => UiRuntimeConfig::from_home(home)?,
        None => UiRuntimeConfig::from_env()?,
    };
    agl_terminal_ui::run_interactive(
        InteractiveOptions {
            resume: args.resume,
            input_history: !args.no_input_history,
            socket_path: args.socket,
            workspace_root: args.workspace,
            function_ref: args.function,
            model_id: args.model,
            operation_mode: args.mode.map(Into::into),
            skills: args.skills,
        },
        &runtime,
    )
}
