use std::path::PathBuf;

use agl_matrix_bridge::{AgentClient, BridgeConfig, BridgeState};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "agl-matrix-bridge",
    version,
    about = "agentLIBRE Matrix bridge tooling"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate bridge config without connecting to Matrix.
    CheckConfig {
        /// Matrix bridge config TOML path.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
    /// Report daemon status through the bridge client boundary.
    Status {
        /// Matrix bridge config TOML path.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,

        /// Override daemon Unix socket path.
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
    },
}

fn main() {
    if let Err(err) = run(Cli::parse()) {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::CheckConfig { config } => check_config(config),
        Command::Status { config, socket } => status(config, socket),
    }
}

fn check_config(path: PathBuf) -> Result<()> {
    let config = BridgeConfig::load(&path)?;
    config
        .validate()
        .map_err(|err| anyhow::anyhow!("bridge config is invalid: {err:?}"))?;
    let state = if let Some(path) = &config.bindings.path {
        BridgeState::load(path).with_context(|| format!("failed to load binding state {}", path))?
    } else {
        BridgeState::default()
    };

    println!("config=ok");
    println!("command_prefix={}", config.matrix.command_prefix());
    println!("normal_chat={}", config.matrix.normal_chat);
    println!("encrypted_rooms={:?}", config.matrix.encrypted_rooms);
    println!("allowed_rooms={}", config.access.allowed_rooms.len());
    println!("allowed_users={}", config.access.allowed_users.len());
    println!("bindings={}", state.bindings.len());
    println!("processed_events={}", state.processed_event_ids.len());
    Ok(())
}

#[cfg(unix)]
fn status(path: PathBuf, socket: Option<PathBuf>) -> Result<()> {
    let config = BridgeConfig::load(&path)?;
    config
        .validate()
        .map_err(|err| anyhow::anyhow!("bridge config is invalid: {err:?}"))?;
    let socket_path = socket
        .or_else(|| config.agl.socket_path.map(PathBuf::from))
        .context("daemon socket path is required: set [agl].socket_path or pass --socket")?;
    let mut client =
        agl_matrix_bridge::AgentLibreClient::connect(&socket_path).with_context(|| {
            format!(
                "failed to connect to daemon socket {}",
                socket_path.display()
            )
        })?;
    println!("{}", client.daemon_status()?);
    Ok(())
}

#[cfg(not(unix))]
fn status(_path: PathBuf, _socket: Option<PathBuf>) -> Result<()> {
    anyhow::bail!("agl-matrix-bridge status is only available on Unix platforms in this alpha")
}
