use std::path::PathBuf;

#[cfg(unix)]
use agl_matrix_bridge::{
    AgentClient, BridgeApp, BridgeInboundEvent, EncryptionState, LazyDaemonClient,
    MatrixPasswordLogin, MatrixRuntime,
};
use agl_matrix_bridge::{BridgeConfig, BridgeOutboundAction, BridgeState};
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
    /// Run foreground Matrix sync loop.
    Sync {
        /// Matrix bridge config TOML path.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,

        /// Override daemon Unix socket path.
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
    },
    /// Login to Matrix with password credentials from environment and save session.
    LoginPassword {
        /// Matrix bridge config TOML path.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
    /// Fail closed until interactive Matrix device verification is implemented.
    VerifyDevice {
        /// Matrix bridge config TOML path.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
        /// Matrix user id that owns the device.
        #[arg(long, value_name = "USER_ID")]
        user_id: String,
        /// Matrix device id to verify.
        #[arg(long, value_name = "DEVICE_ID")]
        device_id: String,
    },
    /// Run handler/state logic against one synthetic Matrix text event.
    HandleTestEvent {
        /// Matrix bridge config TOML path.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,

        /// Matrix room id.
        #[arg(long, value_name = "ID")]
        room: String,

        /// Matrix sender user id.
        #[arg(long, value_name = "ID")]
        sender: String,

        /// Matrix event id.
        #[arg(long, value_name = "ID")]
        event: String,

        /// Matrix thread root event id.
        #[arg(long, value_name = "ID")]
        thread: Option<String>,

        /// Plaintext message body.
        #[arg(long, value_name = "TEXT")]
        body: String,

        /// Override daemon Unix socket path.
        #[arg(long, value_name = "PATH")]
        socket: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() {
    if let Err(err) = run(Cli::parse()).await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::CheckConfig { config } => check_config(config),
        Command::Status { config, socket } => status(config, socket),
        Command::Sync { config, socket } => sync(config, socket).await,
        Command::LoginPassword { config } => login_password(config).await,
        Command::VerifyDevice {
            config,
            user_id,
            device_id,
        } => verify_device(config, user_id, device_id),
        Command::HandleTestEvent {
            config,
            room,
            sender,
            event,
            thread,
            body,
            socket,
        } => handle_test_event(config, room, sender, event, thread, body, socket),
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
    println!(
        "store_path_configured={}",
        has_config_value(&config.matrix.store_path)
    );
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
    let mut client = LazyDaemonClient::new(socket_path);
    println!("{}", client.daemon_status()?);
    Ok(())
}

#[cfg(not(unix))]
fn status(_path: PathBuf, _socket: Option<PathBuf>) -> Result<()> {
    anyhow::bail!("agl-matrix-bridge status is only available on Unix platforms in this alpha")
}

#[cfg(unix)]
async fn sync(path: PathBuf, socket: Option<PathBuf>) -> Result<()> {
    let mut config = BridgeConfig::load(&path)?;
    config
        .validate()
        .map_err(|err| anyhow::anyhow!("bridge config is invalid: {err:?}"))?;
    if let Some(socket) = socket {
        config.agl.socket_path = Some(socket.display().to_string());
    }
    let socket_path = config
        .agl
        .socket_path
        .clone()
        .map(PathBuf::from)
        .context("daemon socket path is required: set [agl].socket_path or pass --socket")?;
    let runtime = MatrixRuntime::from_config(config, socket_path).await?;
    runtime.sync_forever().await
}

#[cfg(not(unix))]
async fn sync(_path: PathBuf, _socket: Option<PathBuf>) -> Result<()> {
    anyhow::bail!("agl-matrix-bridge sync is only available on Unix platforms in this alpha")
}

#[cfg(unix)]
async fn login_password(path: PathBuf) -> Result<()> {
    let config = BridgeConfig::load(&path)?;
    let login = MatrixPasswordLogin::from_env()?;
    let result = MatrixRuntime::login_with_password(config, login).await?;
    println!("login=ok");
    println!("user_id={}", result.user_id);
    println!("device_id={}", result.device_id);
    println!("session_path={}", result.session_path.display());
    if let Some(store_path) = result.store_path {
        println!("store_path={}", store_path.display());
    }
    Ok(())
}

#[cfg(not(unix))]
async fn login_password(_path: PathBuf) -> Result<()> {
    anyhow::bail!(
        "agl-matrix-bridge login-password is only available on Unix platforms in this alpha"
    )
}

fn verify_device(path: PathBuf, user_id: String, device_id: String) -> Result<()> {
    let config = BridgeConfig::load(&path)?;
    config
        .validate()
        .map_err(|err| anyhow::anyhow!("bridge config is invalid: {err:?}"))?;
    if user_id.trim().is_empty() {
        anyhow::bail!("--user-id is required for Matrix device verification");
    }
    if device_id.trim().is_empty() {
        anyhow::bail!("--device-id is required for Matrix device verification");
    }
    if !has_config_value(&config.matrix.store_path) {
        anyhow::bail!("matrix.store_path is required for Matrix device verification");
    }
    anyhow::bail!(
        "Matrix device verification is not implemented in this alpha; it requires a persistent crypto store and interactive SAS verification loop"
    )
}

fn has_config_value(value: &Option<String>) -> bool {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some()
}

#[cfg(unix)]
fn handle_test_event(
    path: PathBuf,
    room: String,
    sender: String,
    event: String,
    thread: Option<String>,
    body: String,
    socket: Option<PathBuf>,
) -> Result<()> {
    let mut config = BridgeConfig::load(&path)?;
    config
        .validate()
        .map_err(|err| anyhow::anyhow!("bridge config is invalid: {err:?}"))?;
    if let Some(socket) = socket {
        config.agl.socket_path = Some(socket.display().to_string());
    }
    let socket_path = config
        .agl
        .socket_path
        .clone()
        .map(PathBuf::from)
        .context("daemon socket path is required: set [agl].socket_path or pass --socket")?;
    let mut app = BridgeApp::from_config(config)?;
    let mut client = LazyDaemonClient::new(socket_path);
    let actions = app.handle_event(
        BridgeInboundEvent {
            event_id: event,
            room_id: room,
            sender_user_id: sender,
            thread_root_event_id: thread,
            body,
            encryption: EncryptionState::Plaintext,
        },
        &mut client,
    )?;
    print_actions(&actions);
    Ok(())
}

#[cfg(not(unix))]
fn handle_test_event(
    _path: PathBuf,
    _room: String,
    _sender: String,
    _event: String,
    _thread: Option<String>,
    _body: String,
    _socket: Option<PathBuf>,
) -> Result<()> {
    anyhow::bail!(
        "agl-matrix-bridge handle-test-event is only available on Unix platforms in this alpha"
    )
}

fn print_actions(actions: &[BridgeOutboundAction]) {
    for action in actions {
        match action {
            BridgeOutboundAction::Ignore { reason } => println!("action=ignore reason={reason}"),
            BridgeOutboundAction::ReplyInThread { body } => {
                println!("action=reply bytes={}", body.len())
            }
            BridgeOutboundAction::NoticeInThread { body } => {
                println!("action=notice bytes={}", body.len())
            }
            BridgeOutboundAction::MarkProcessed { event_id } => {
                println!("action=mark_processed event_id={event_id}")
            }
            BridgeOutboundAction::PersistBinding { key, session_id } => {
                println!(
                    "action=persist_binding room_id={} thread_root_event_id={} session_id={}",
                    key.room_id,
                    key.thread_root_event_id.as_deref().unwrap_or(""),
                    session_id
                )
            }
            BridgeOutboundAction::RemoveBinding { key } => {
                println!(
                    "action=remove_binding room_id={} thread_root_event_id={}",
                    key.room_id,
                    key.thread_root_event_id.as_deref().unwrap_or("")
                )
            }
        }
    }
}
