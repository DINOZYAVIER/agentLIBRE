use std::path::PathBuf;
#[cfg(unix)]
use std::{io::Write, time::Duration};

use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
#[cfg(unix)]
use agl_matrix_bridge::{
    AgentClient, BridgeApp, BridgeInboundEvent, EncryptionState, LazyDaemonClient,
    MatrixDeviceVerificationRequest, MatrixPasswordLogin, MatrixRuntime, MatrixSasPresentation,
};
use agl_matrix_bridge::{BridgeConfig, BridgeOutboundAction, BridgeState};
use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::{Parser, Subcommand};
use matrix_sdk::authentication::matrix::MatrixSession;
use pbkdf2::pbkdf2_hmac;
use serde::Deserialize;
use sha2::Sha256;
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
    /// Login to Matrix with password credentials from environment and save session.
    LoginPassword {
        /// Matrix bridge config TOML path.
        #[arg(long, value_name = "PATH")]
        config: PathBuf,
    },
    /// Run interactive Matrix SAS device verification.
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
        /// Seconds to wait for each Matrix verification phase.
        #[arg(long, default_value_t = 300, value_name = "SECONDS")]
        timeout_seconds: u64,
    },
    /// Convert a legacy encrypted Matrix session into the current session file shape.
    MigrateLegacySession {
        /// Legacy encrypted session.json path.
        #[arg(long, value_name = "PATH")]
        input: PathBuf,
        /// File containing the legacy Matrix store passphrase.
        #[arg(long, value_name = "PATH")]
        passphrase_file: PathBuf,
        /// Output Matrix session path for the current bridge.
        #[arg(long, value_name = "PATH")]
        output: PathBuf,
        /// Expected Matrix bot user id.
        #[arg(long, value_name = "USER_ID")]
        user_id: String,
        /// Allow overwriting an existing output file.
        #[arg(long)]
        force: bool,
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
        Command::LoginPassword { config } => login_password(config).await,
        Command::VerifyDevice {
            config,
            user_id,
            device_id,
            timeout_seconds,
        } => verify_device(config, user_id, device_id, timeout_seconds).await,
        Command::MigrateLegacySession {
            input,
            passphrase_file,
            output,
            user_id,
            force,
        } => migrate_legacy_session(input, passphrase_file, output, user_id, force),
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

#[derive(Deserialize)]
#[serde(tag = "format", rename_all = "snake_case")]
enum LegacyStoredSession {
    Plain {
        session: MatrixSession,
    },
    Aes256GcmPbkdf2Sha256 {
        iterations: u32,
        salt_b64: String,
        nonce_b64: String,
        ciphertext_b64: String,
    },
}

fn migrate_legacy_session(
    input: PathBuf,
    passphrase_file: PathBuf,
    output: PathBuf,
    user_id: String,
    force: bool,
) -> Result<()> {
    if output.exists() && !force {
        anyhow::bail!(
            "output already exists: {}; pass --force to overwrite",
            output.display()
        );
    }

    let input_bytes = std::fs::read(&input)
        .with_context(|| format!("failed to read legacy Matrix session {}", input.display()))?;
    let stored: LegacyStoredSession = serde_json::from_slice(&input_bytes)
        .with_context(|| format!("failed to parse legacy Matrix session {}", input.display()))?;
    let session = decode_legacy_session(stored, &passphrase_file)?;
    if session.meta.user_id.as_str() != user_id {
        anyhow::bail!(
            "legacy Matrix session user {} does not match expected user {}",
            session.meta.user_id,
            user_id
        );
    }

    let bytes =
        serde_json::to_vec_pretty(&session).context("failed to serialize Matrix session")?;
    write_private_file(&output, &bytes)
        .with_context(|| format!("failed to write Matrix session {}", output.display()))?;
    println!("migration=ok");
    println!("user_id={}", session.meta.user_id);
    println!("device_id={}", session.meta.device_id);
    println!("output={}", output.display());
    Ok(())
}

fn decode_legacy_session(
    stored: LegacyStoredSession,
    passphrase_file: &std::path::Path,
) -> Result<MatrixSession> {
    match stored {
        LegacyStoredSession::Plain { session } => Ok(session),
        LegacyStoredSession::Aes256GcmPbkdf2Sha256 {
            iterations,
            salt_b64,
            nonce_b64,
            ciphertext_b64,
        } => {
            let passphrase = std::fs::read_to_string(passphrase_file).with_context(|| {
                format!(
                    "failed to read legacy Matrix session passphrase {}",
                    passphrase_file.display()
                )
            })?;
            let passphrase = trim_trailing_line_endings(passphrase);
            if passphrase.is_empty() {
                anyhow::bail!(
                    "legacy Matrix session passphrase file is empty: {}",
                    passphrase_file.display()
                );
            }
            let salt = BASE64
                .decode(salt_b64)
                .context("invalid legacy session salt")?;
            let nonce = BASE64
                .decode(nonce_b64)
                .context("invalid legacy session nonce")?;
            let ciphertext = BASE64
                .decode(ciphertext_b64)
                .context("invalid legacy session ciphertext")?;
            let mut key = [0u8; 32];
            pbkdf2_hmac::<Sha256>(passphrase.as_bytes(), &salt, iterations, &mut key);
            let cipher = Aes256Gcm::new_from_slice(&key).expect("AES-256 key length is fixed");
            let plaintext = cipher
                .decrypt(Nonce::from_slice(&nonce), ciphertext.as_slice())
                .map_err(|_| anyhow::anyhow!("failed to decrypt legacy Matrix session"))?;
            serde_json::from_slice(&plaintext)
                .context("failed to parse decrypted legacy Matrix session")
        }
    }
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

#[cfg(unix)]
async fn verify_device(
    path: PathBuf,
    user_id: String,
    device_id: String,
    timeout_seconds: u64,
) -> Result<()> {
    let config = BridgeConfig::load(&path)?;
    if user_id.trim().is_empty() {
        anyhow::bail!("--user-id is required for Matrix device verification");
    }
    if device_id.trim().is_empty() {
        anyhow::bail!("--device-id is required for Matrix device verification");
    }
    if timeout_seconds == 0 {
        anyhow::bail!("--timeout-seconds must be greater than zero");
    }
    let result = MatrixRuntime::verify_device(
        config,
        MatrixDeviceVerificationRequest {
            user_id,
            device_id,
            timeout: Duration::from_secs(timeout_seconds),
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

#[cfg(not(unix))]
async fn verify_device(
    _path: PathBuf,
    _user_id: String,
    _device_id: String,
    _timeout_seconds: u64,
) -> Result<()> {
    anyhow::bail!(
        "agl-matrix-bridge verify-device is only available on Unix platforms in this alpha"
    )
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
fn write_private_file(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create Matrix session dir {}", parent.display()))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_file(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create Matrix session dir {}", parent.display()))?;
    }
    std::fs::write(path, bytes)?;
    Ok(())
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
