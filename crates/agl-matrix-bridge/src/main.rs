use std::path::PathBuf;
#[cfg(unix)]
use std::{io::Write, time::Duration};

#[cfg(unix)]
use agl_matrix_bridge::{
    AgentClient, BridgeApp, BridgeInboundEvent, EncryptionState, LazyDaemonClient,
    MatrixDeviceVerificationRequest, MatrixOutboxDeliveryReport, MatrixPasswordLogin,
    MatrixRuntime, MatrixSasPresentation, MatrixUserDevice,
};
use agl_matrix_bridge::{BridgeConfig, BridgeOutboundAction, BridgeState};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

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
    /// Deliver queued agentLIBRE Matrix notification outbox rows.
    DeliverOutbox {
        /// Matrix bridge config TOML path.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,

        /// agentLIBRE store root containing agentlibre.sqlite3.
        #[arg(long, value_name = "PATH")]
        store_root: PathBuf,

        /// Maximum queued notifications to deliver.
        #[arg(long, default_value_t = 10, value_name = "N")]
        limit: usize,
    },
    /// Login to Matrix with password credentials from environment and save session.
    LoginPassword {
        /// Matrix bridge config TOML path.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
        /// Matrix username. Defaults to AGL_MATRIX_USERNAME.
        #[arg(long, value_name = "USERNAME")]
        username: Option<String>,
        /// Read Matrix password from stdin instead of AGL_MATRIX_PASSWORD.
        #[arg(long)]
        password_stdin: bool,
        /// Matrix device display name. Defaults to AGL_MATRIX_DEVICE_DISPLAY_NAME or agl-matrix-bridge.
        #[arg(long, value_name = "NAME")]
        device_display_name: Option<String>,
        /// Replace an existing session file. The configured crypto store must still be empty.
        #[arg(long)]
        replace_session: bool,
    },
    /// Run interactive Matrix SAS device verification.
    VerifyDevice {
        /// Matrix bridge config TOML path.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
        /// Matrix user id that owns the trusted device. Must match matrix.user_id.
        #[arg(long, value_name = "USER_ID")]
        user_id: Option<String>,
        /// Trusted Matrix device id for matrix.user_id. Overrides [verification].device_id.
        #[arg(long, value_name = "DEVICE_ID")]
        device_id: Option<String>,
        /// Seconds to wait for each Matrix verification phase.
        #[arg(long, default_value_t = 300, value_name = "SECONDS")]
        timeout_seconds: u64,
        /// Wait for the trusted device to start verification instead of initiating it here.
        #[arg(long)]
        accept_incoming: bool,
    },
    /// List Matrix devices visible for a user.
    ListDevices {
        /// Matrix bridge config TOML path.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
        /// Matrix user id to inspect. Defaults to matrix.user_id.
        #[arg(long, value_name = "USER_ID")]
        user_id: Option<String>,
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
    init_tracing();
    if let Err(err) = run(Cli::parse()).await {
        eprintln!("error: {err:#}");
        std::process::exit(1);
    }
}

fn init_tracing() {
    let filter = if std::env::var_os("AGL_MATRIX_LOG").is_some() {
        EnvFilter::from_env("AGL_MATRIX_LOG")
    } else if std::env::var_os("RUST_LOG").is_some() {
        EnvFilter::from_default_env()
    } else {
        EnvFilter::new("agl_matrix_bridge=info,matrix_sdk=warn,warn")
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_writer(std::io::stderr)
        .try_init();
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::CheckConfig { config } => check_config(config),
        Command::Status { config, socket } => status(config, socket),
        Command::Sync { config, socket } => sync(config, socket).await,
        Command::DeliverOutbox {
            config,
            store_root,
            limit,
        } => deliver_outbox(config, store_root, limit).await,
        Command::LoginPassword {
            config,
            username,
            password_stdin,
            device_display_name,
            replace_session,
        } => {
            login_password(
                config,
                username,
                password_stdin,
                device_display_name,
                replace_session,
            )
            .await
        }
        Command::VerifyDevice {
            config,
            user_id,
            device_id,
            timeout_seconds,
            accept_incoming,
        } => verify_device(config, user_id, device_id, timeout_seconds, accept_incoming).await,
        Command::ListDevices { config, user_id } => list_devices(config, user_id).await,
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

fn trim_trailing_line_endings(mut value: String) -> String {
    while value.ends_with('\n') || value.ends_with('\r') {
        value.pop();
    }
    value
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
async fn deliver_outbox(path: PathBuf, store_root: PathBuf, limit: usize) -> Result<()> {
    let config = BridgeConfig::load(&path)?;
    let report = MatrixRuntime::deliver_outbox(config, store_root, limit).await?;
    print_outbox_delivery_report(&report);
    Ok(())
}

#[cfg(not(unix))]
async fn deliver_outbox(_path: PathBuf, _store_root: PathBuf, _limit: usize) -> Result<()> {
    anyhow::bail!(
        "agl-matrix-bridge deliver-outbox is only available on Unix platforms in this alpha"
    )
}

#[cfg(unix)]
fn print_outbox_delivery_report(report: &MatrixOutboxDeliveryReport) {
    println!("tool=matrix.outbox.deliver");
    println!("queued={}", report.queued);
    println!("sent={}", report.sent);
    println!("failed={}", report.failed);
    println!("truncated={}", report.truncated);
    println!("---");
    for item in &report.items {
        println!(
            "notification id={} notify_ref={} action={} error={}",
            item.id,
            item.notify_ref,
            item.action.as_str(),
            item.error.as_deref().unwrap_or("")
        );
    }
}

#[cfg(unix)]
async fn login_password(
    path: PathBuf,
    username: Option<String>,
    password_stdin: bool,
    device_display_name: Option<String>,
    replace_session: bool,
) -> Result<()> {
    let config = BridgeConfig::load(&path)?;
    let login = if username.is_none() && !password_stdin && device_display_name.is_none() {
        MatrixPasswordLogin::from_env()?
    } else {
        MatrixPasswordLogin {
            username: credential_value(username, "AGL_MATRIX_USERNAME", "password login")?,
            password: if password_stdin {
                read_password_stdin()?
            } else {
                credential_value(None, "AGL_MATRIX_PASSWORD", "password login")?
            },
            device_display_name: credential_value_optional(
                device_display_name,
                "AGL_MATRIX_DEVICE_DISPLAY_NAME",
            )
            .unwrap_or_else(|| "agl-matrix-bridge".to_string()),
        }
    };
    let result = MatrixRuntime::login_with_password(config, login, replace_session).await?;
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
async fn login_password(
    _path: PathBuf,
    _username: Option<String>,
    _password_stdin: bool,
    _device_display_name: Option<String>,
    _replace_session: bool,
) -> Result<()> {
    anyhow::bail!(
        "agl-matrix-bridge login-password is only available on Unix platforms in this alpha"
    )
}

fn credential_value(cli_value: Option<String>, env_name: &str, purpose: &str) -> Result<String> {
    credential_value_optional(cli_value, env_name)
        .with_context(|| format!("{env_name} is required for {purpose}"))
}

fn credential_value_optional(cli_value: Option<String>, env_name: &str) -> Option<String> {
    cli_value
        .or_else(|| std::env::var(env_name).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn read_password_stdin() -> Result<String> {
    use std::io::Read;

    let mut password = String::new();
    std::io::stdin()
        .read_to_string(&mut password)
        .context("failed to read Matrix password from stdin")?;
    let password = trim_trailing_line_endings(password);
    if password.is_empty() {
        anyhow::bail!("Matrix password from stdin is empty");
    }
    Ok(password)
}

#[cfg(unix)]
async fn verify_device(
    path: PathBuf,
    user_id: Option<String>,
    device_id: Option<String>,
    timeout_seconds: u64,
    accept_incoming: bool,
) -> Result<()> {
    let config = BridgeConfig::load(&path)?;
    let user_id = verification_user_id(&config, user_id)?;
    let device_id = required_cli_or_config_value(
        device_id,
        &config.verification.device_id,
        "--device-id",
        "[verification].device_id",
        "Matrix device verification",
    )?;
    if timeout_seconds == 0 {
        anyhow::bail!("--timeout-seconds must be greater than zero");
    }
    let result = MatrixRuntime::verify_device(
        config,
        MatrixDeviceVerificationRequest {
            user_id,
            device_id,
            timeout: Duration::from_secs(timeout_seconds),
            accept_incoming,
        },
        prompt_sas_confirmation,
    )
    .await?;
    println!("verification={}", result.status.as_str());
    println!("user_id={}", result.user_id);
    println!("device_id={}", result.device_id);
    if let Some(flow_id) = result.flow_id {
        println!("flow_id={flow_id}");
    }
    Ok(())
}

fn verification_user_id(config: &BridgeConfig, cli_user_id: Option<String>) -> Result<String> {
    let user_id = cli_user_id
        .or_else(|| config.verification.user_id.clone())
        .unwrap_or_else(|| config.matrix.user_id.clone());
    let user_id = user_id.trim().to_owned();
    if user_id.is_empty() {
        anyhow::bail!("matrix.user_id is required for Matrix device verification");
    }
    if user_id != config.matrix.user_id {
        anyhow::bail!(
            "Matrix device verification only supports self-verification for the bridge account: target user {user_id} must match matrix.user_id {}",
            config.matrix.user_id
        );
    }
    Ok(user_id)
}

#[cfg(not(unix))]
async fn verify_device(
    _path: PathBuf,
    _user_id: Option<String>,
    _device_id: Option<String>,
    _timeout_seconds: u64,
    _accept_incoming: bool,
) -> Result<()> {
    anyhow::bail!(
        "agl-matrix-bridge verify-device is only available on Unix platforms in this alpha"
    )
}

fn required_cli_or_config_value(
    cli_value: Option<String>,
    config_value: &Option<String>,
    flag: &str,
    config_key: &str,
    purpose: &str,
) -> Result<String> {
    let Some(value) = cli_value.or_else(|| config_value.clone()) else {
        anyhow::bail!("{flag} is required for {purpose}: pass {flag} or set {config_key}");
    };
    let value = value.trim().to_owned();
    if value.is_empty() {
        anyhow::bail!("{flag} is required for {purpose}: pass {flag} or set {config_key}");
    }
    Ok(value)
}

#[cfg(unix)]
async fn list_devices(path: PathBuf, user_id: Option<String>) -> Result<()> {
    let config = BridgeConfig::load(&path)?;
    let user_id = user_id.unwrap_or_else(|| config.matrix.user_id.clone());
    let user_id = user_id.trim().to_owned();
    if user_id.is_empty() {
        anyhow::bail!("matrix.user_id is required for Matrix device listing");
    }
    let devices = MatrixRuntime::list_user_devices(config, user_id).await?;
    print_devices(&devices);
    Ok(())
}

#[cfg(not(unix))]
async fn list_devices(_path: PathBuf, _user_id: Option<String>) -> Result<()> {
    anyhow::bail!(
        "agl-matrix-bridge list-devices is only available on Unix platforms in this alpha"
    )
}

fn print_devices(devices: &[MatrixUserDevice]) {
    println!("devices={}", devices.len());
    for device in devices {
        println!(
            "device_id={} user_id={} verified={} display_name={}",
            device.device_id,
            device.user_id,
            device.verified,
            device.display_name.as_deref().unwrap_or("")
        );
    }
}

#[cfg(unix)]
fn prompt_sas_confirmation(presentation: &MatrixSasPresentation) -> Result<bool> {
    println!("flow_id={}", presentation.flow_id);
    println!("sas_user_id={}", presentation.user_id);
    println!("sas_device_id={}", presentation.device_id);
    if !presentation.emojis.is_empty() {
        println!("sas_emojis:");
        for emoji in &presentation.emojis {
            println!("  {} {}", emoji.symbol, emoji.description);
        }
    }
    if let Some((first, second, third)) = presentation.decimals {
        println!("sas_decimals={first}-{second}-{third}");
    }
    print!("Type yes if the SAS matches on the other Matrix device: ");
    std::io::stdout().flush()?;

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim().eq_ignore_ascii_case("yes"))
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
