#![cfg(target_os = "linux")]

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use agl_ids::{DaemonInstanceId, RequestId, SessionId};
use agl_protocol::{
    ApplicationToolResult, ApplicationToolResultEvent, CommandCatalogEvent, DaemonEvent,
    DaemonEventKind, DaemonRequest, DaemonRequestKind, PROTOCOL_VERSION, PresentationCursor,
    ProtocolToolMode, RuntimeGenerationIdentity, RuntimeGenerationKind, SanitizedDisplayPath,
    SessionHeader, SessionPresentationSnapshot, SessionPresentationSnapshotTransfer,
    SessionPresentationSnapshotTransferPurpose, SessionPresentationStatus,
};
use agl_terminal_protocol::{
    ServiceIdentity, TERMINAL_RESPONSE_SCHEMA, TerminalGenerationIdentity,
    TerminalGenerationManifest, TerminalRequest, TerminalRequestKind, TerminalResponse,
    TerminalResponseKind,
};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio_util::codec::{Framed, LinesCodec};

const READY_TEXT: &[u8] = b"agentLIBRE";
const ENABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004h";
const DISABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";
const CURSOR_POSITION_REPLY: &[u8] = b"\x1b[1;1R";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
static FIXTURE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
// Ratatui's inline viewport redraws from the cursor down instead of clearing
// the full physical screen.
const INLINE_REDRAW_CLEAR: &[u8] = b"\x1b[J";

#[test]
fn interactive_disconnect_restores_parent_terminal() {
    let mut environment = NativeTuiEnvironment::new();
    let mut terminal = environment.spawn_tui();
    terminal.wait_until_ready();
    terminal.write(&[0x04]);
    let status = terminal.finish();

    assert!(status.success(), "Ctrl+D disconnect failed: {status}");
    terminal.assert_parent_terminal_restored();
    terminal.assert_control_modes_restored();
    environment.join_server();
}

#[test]
fn interactive_daemon_connection_failure_restores_parent_terminal() {
    let mut environment = NativeTuiEnvironment::new();
    let mut terminal = environment.spawn_tui();
    terminal.wait_until_ready();
    environment.disconnect_server();
    let status = terminal.finish();

    assert!(
        !status.success(),
        "daemon connection loss unexpectedly succeeded"
    );
    terminal.assert_parent_terminal_restored();
    terminal.assert_control_modes_restored();
    environment.join_server();
}

#[test]
fn interactive_sigint_restores_parent_terminal() {
    let mut environment = NativeTuiEnvironment::new();
    let mut terminal = environment.spawn_tui();
    terminal.wait_until_ready();
    terminal.send_signal(libc::SIGINT);
    let status = terminal.finish();

    assert!(
        status.success(),
        "SIGINT shutdown failed: {status}; output={}",
        escaped(&terminal.output)
    );
    terminal.assert_parent_terminal_restored();
    terminal.assert_control_modes_restored();
    environment.join_server();
}

#[test]
fn interactive_suspend_continue_resize_and_terminate_restore_parent_terminal() {
    let mut environment = NativeTuiEnvironment::new();
    let mut terminal = environment.spawn_tui();
    terminal.wait_until_ready();

    for (columns, rows) in [(91, 31), (104, 42), (79, 23)] {
        terminal.resize(columns, rows);
        terminal.send_signal(libc::SIGWINCH);
    }
    terminal.assert_child_is_running();
    terminal.assert_child_terminal_raw();

    terminal.send_signal(libc::SIGTSTP);
    terminal.wait_until_stopped();
    terminal.assert_parent_terminal_restored();
    let clear_count_before_continue = count_occurrences(&terminal.output, INLINE_REDRAW_CLEAR);

    terminal.send_signal(libc::SIGCONT);
    terminal.wait_until_raw_after_continue(clear_count_before_continue);
    terminal.resize(120, 45);
    terminal.send_signal(libc::SIGWINCH);
    terminal.assert_child_is_running();

    terminal.send_signal(libc::SIGTERM);
    let status = terminal.finish();
    assert!(
        status.success(),
        "SIGTERM shutdown failed: {status}; output={}",
        escaped(&terminal.output)
    );
    terminal.assert_parent_terminal_restored();
    terminal.assert_control_modes_restored();
    assert!(
        count_occurrences(&terminal.output, ENABLE_BRACKETED_PASTE) >= 2,
        "SIGCONT did not re-enable bracketed-paste mode: {}",
        escaped(&terminal.output)
    );
    assert!(
        count_occurrences(&terminal.output, DISABLE_BRACKETED_PASTE) >= 2,
        "suspend/final cleanup did not disable bracketed-paste mode twice: {}",
        escaped(&terminal.output)
    );
    environment.join_server();
}

struct NativeTuiEnvironment {
    root: PathBuf,
    workspace: PathBuf,
    home: PathBuf,
    socket: PathBuf,
    ui_executable: PathBuf,
    server: Option<DaemonFixture>,
    terminal_server: Option<TerminalFixture>,
}

impl NativeTuiEnvironment {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "atui-{}-{}",
            std::process::id(),
            FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let home = root.join("agl-home");
        let (ui_executable, terminal_generation) = install_terminal_generation(&root);
        let terminal_identity = write_terminal_identity(&home, terminal_generation);
        let terminal_server = TerminalFixture::start(
            &home.join("runtime/agl-terminal/terminal.sock"),
            terminal_identity,
        );
        let socket = root.join("daemon.sock");
        let server = DaemonFixture::start(&socket, &workspace);
        Self {
            root,
            workspace,
            home,
            socket,
            ui_executable,
            server: Some(server),
            terminal_server: Some(terminal_server),
        }
    }

    fn spawn_tui(&self) -> ParentTerminalFixture {
        ParentTerminalFixture::spawn(
            &self.ui_executable,
            &self.home,
            &self.socket,
            &self.workspace,
        )
    }

    fn disconnect_server(&mut self) {
        self.server
            .as_mut()
            .expect("daemon fixture is present")
            .disconnect();
    }

    fn join_server(&mut self) {
        self.server
            .take()
            .expect("daemon fixture is present")
            .join();
        self.terminal_server
            .take()
            .expect("terminal fixture is present")
            .join();
    }
}

fn install_terminal_generation(root: &Path) -> (PathBuf, TerminalGenerationIdentity) {
    let terminal_root = root.join("agl-terminal");
    let generations = terminal_root.join("generations");
    std::fs::create_dir_all(&generations).unwrap();
    let generation = generations.join("terminal-generation");
    std::fs::create_dir_all(&generation).unwrap();
    std::fs::copy(
        env!("CARGO_BIN_EXE_agl-terminal"),
        generation.join("agl-terminal"),
    )
    .unwrap();
    std::fs::write(generation.join("agl-terminald"), b"terminald fixture").unwrap();
    std::fs::write(generation.join("agl-process-launcher"), b"launcher fixture").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&terminal_root, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&generations, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&generation, std::fs::Permissions::from_mode(0o755)).unwrap();
        for name in ["agl-terminald", "agl-process-launcher", "agl-terminal"] {
            std::fs::set_permissions(
                generation.join(name),
                std::fs::Permissions::from_mode(0o555),
            )
            .unwrap();
        }
    }
    let verified = TerminalGenerationManifest::seal(&generation, &"a".repeat(40)).unwrap();
    let installed_generation = generations.join(verified.generation_directory_name());
    std::fs::rename(&generation, &installed_generation).unwrap();
    (
        installed_generation.join("agl-terminal"),
        verified.identity().clone(),
    )
}

fn write_terminal_identity(
    home: &Path,
    terminal_generation: TerminalGenerationIdentity,
) -> ServiceIdentity {
    let root = home.join("runtime/agl-terminal");
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("service-identity.json");
    let identity = ServiceIdentity::new(
        terminal_generation,
        agl_exec::ServiceGenerationId::generate(),
    )
    .unwrap();
    std::fs::write(&path, serde_json::to_vec(&identity).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    identity
}

impl Drop for NativeTuiEnvironment {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            drop(server);
        }
        if let Some(server) = self.terminal_server.take() {
            drop(server);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if let Ok(entries) = std::fs::read_dir(self.root.join("agl-terminal/generations")) {
                for entry in entries.flatten() {
                    if entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("generation-")
                    {
                        let _ = std::fs::set_permissions(
                            entry.path(),
                            std::fs::Permissions::from_mode(0o755),
                        );
                        if let Ok(files) = std::fs::read_dir(entry.path()) {
                            for file in files.flatten() {
                                let _ = std::fs::set_permissions(
                                    file.path(),
                                    std::fs::Permissions::from_mode(0o755),
                                );
                            }
                        }
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct TerminalFixture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl TerminalFixture {
    fn start(socket: &Path, identity: ServiceIdentity) -> Self {
        std::fs::create_dir_all(socket.parent().expect("terminal socket has a parent")).unwrap();
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => return Err(format!("terminal accept failed: {error}")),
                };
                let mut length = [0_u8; 4];
                stream
                    .read_exact(&mut length)
                    .map_err(|error| format!("terminal request length failed: {error}"))?;
                let mut frame = vec![0_u8; u32::from_be_bytes(length) as usize];
                stream
                    .read_exact(&mut frame)
                    .map_err(|error| format!("terminal request failed: {error}"))?;
                let request: TerminalRequest = serde_json::from_slice(&frame)
                    .map_err(|error| format!("terminal request decode failed: {error}"))?;
                let response = match request.request {
                    TerminalRequestKind::Hello => TerminalResponseKind::Hello,
                    TerminalRequestKind::ListExecutions { .. } => {
                        TerminalResponseKind::ExecutionList {
                            statuses: Vec::new(),
                        }
                    }
                    TerminalRequestKind::ListTopology { .. } => {
                        TerminalResponseKind::TerminalList {
                            records: Vec::new(),
                        }
                    }
                    other => return Err(format!("unexpected terminal request: {other:?}")),
                };
                let response = TerminalResponse {
                    schema: TERMINAL_RESPONSE_SCHEMA.to_owned(),
                    request_id: request.request_id,
                    service: identity.clone(),
                    response,
                };
                let encoded = serde_json::to_vec(&response)
                    .map_err(|error| format!("terminal response encode failed: {error}"))?;
                stream
                    .write_all(&(encoded.len() as u32).to_be_bytes())
                    .and_then(|()| stream.write_all(&encoded))
                    .map_err(|error| format!("terminal response failed: {error}"))?;
            }
            Ok(())
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }

    fn join(mut self) {
        self.stop.store(true, Ordering::Release);
        let result = self.thread.take().unwrap().join().unwrap();
        if let Err(error) = result {
            panic!("{error}");
        }
    }
}

impl Drop for TerminalFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct DaemonFixture {
    stop: Arc<AtomicBool>,
    disconnect: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl DaemonFixture {
    fn start(socket: &Path, workspace: &Path) -> Self {
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (disconnect, disconnected) = tokio::sync::oneshot::channel();
        let workspace = workspace.to_path_buf();
        let thread = std::thread::spawn(move || {
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break Some(stream),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if thread_stop.load(Ordering::Acquire) {
                            break None;
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => return Err(format!("daemon accept failed: {error}")),
                }
            };
            let Some(stream) = stream else {
                return Ok(());
            };
            stream
                .set_nonblocking(true)
                .map_err(|error| format!("failed to configure daemon stream: {error}"))?;
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("failed to build daemon runtime: {error}"))?
                .block_on(async {
                    let stream = tokio::net::UnixStream::from_std(stream)
                        .map_err(|error| format!("failed to adopt daemon stream: {error}"))?;
                    let mut framed = Framed::new(
                        stream,
                        LinesCodec::new_with_max_length(agl_protocol::MAX_JSONL_FRAME_BYTES),
                    );
                    let session_id = SessionId::generate();
                    let daemon_instance_id = DaemonInstanceId::generate();
                    tokio::select! {
                        result = serve_agent_fixture(
                            &mut framed,
                            &workspace,
                            &session_id,
                            &daemon_instance_id,
                        ) => {
                            result
                        }
                        _ = disconnected => Ok(()),
                    }
                })
        });
        Self {
            stop,
            disconnect: Some(disconnect),
            thread: Some(thread),
        }
    }

    fn disconnect(&mut self) {
        if let Some(disconnect) = self.disconnect.take() {
            let _ = disconnect.send(());
        }
    }

    fn join(mut self) {
        self.stop.store(true, Ordering::Release);
        let result = self
            .thread
            .take()
            .expect("daemon fixture thread is present")
            .join()
            .expect("daemon fixture thread panicked");
        if let Err(error) = result {
            panic!("{error}");
        }
    }
}

async fn serve_agent_fixture(
    framed: &mut Framed<tokio::net::UnixStream, LinesCodec>,
    workspace: &Path,
    session_id: &SessionId,
    daemon_instance_id: &DaemonInstanceId,
) -> Result<(), String> {
    while let Some(line) = framed.next().await {
        let line = line.map_err(|error| format!("daemon request read failed: {error}"))?;
        let request: DaemonRequest = serde_json::from_str(&line)
            .map_err(|error| format!("daemon request decode failed: {error}"))?;
        match request.kind {
            DaemonRequestKind::Hello(_) => {
                send_agent_event(
                    framed,
                    &request.request_id,
                    DaemonEventKind::Hello(agl_protocol::HelloEvent {
                        protocol_version: PROTOCOL_VERSION.to_owned(),
                        product_version: env!("CARGO_PKG_VERSION").to_owned(),
                        daemon_instance_id: daemon_instance_id.clone(),
                        daemon_runtime: fixture_runtime_identity(),
                        engine_protocol_id: fixture_digest('d'),
                        tools: Vec::new(),
                    }),
                )
                .await?;
            }
            DaemonRequestKind::ApplicationAction(_) => {
                send_agent_event(
                    framed,
                    &request.request_id,
                    DaemonEventKind::ApplicationToolResult(ApplicationToolResultEvent {
                        result: ApplicationToolResult::SessionOpened {
                            session_id: session_id.clone(),
                            resumed: false,
                        },
                    }),
                )
                .await?;
            }
            DaemonRequestKind::SessionPresentationSubscribe(_) => {
                let snapshot = fixture_snapshot(workspace, session_id, daemon_instance_id);
                let transfer = SessionPresentationSnapshotTransfer::encode(
                    RequestId::generate(),
                    SessionPresentationSnapshotTransferPurpose::SubscriptionInitial,
                    &snapshot,
                )
                .map_err(|error| format!("snapshot encode failed: {error}"))?;
                send_agent_event(
                    framed,
                    &request.request_id,
                    DaemonEventKind::SessionPresentationSnapshotManifest(transfer.manifest),
                )
                .await?;
                for chunk in transfer.chunks {
                    send_agent_event(
                        framed,
                        &request.request_id,
                        DaemonEventKind::SessionPresentationSnapshotChunk(chunk),
                    )
                    .await?;
                }
                send_agent_event(
                    framed,
                    &request.request_id,
                    DaemonEventKind::SessionPresentationSnapshotFinished(transfer.finished),
                )
                .await?;
            }
            DaemonRequestKind::CommandCatalog(_) => {
                send_agent_event(
                    framed,
                    &request.request_id,
                    DaemonEventKind::CommandCatalog(CommandCatalogEvent {
                        descriptors: Vec::new(),
                    }),
                )
                .await?;
            }
            DaemonRequestKind::SubscriptionCancel(_) => {}
            other => return Err(format!("unexpected daemon request: {other:?}")),
        }
    }
    Ok(())
}

async fn send_agent_event(
    framed: &mut Framed<tokio::net::UnixStream, LinesCodec>,
    request_id: &RequestId,
    kind: DaemonEventKind,
) -> Result<(), String> {
    let event = DaemonEvent::new(Some(request_id.clone()), kind);
    event
        .validate()
        .map_err(|error| format!("invalid daemon fixture event: {error}"))?;
    let line = serde_json::to_string(&event)
        .map_err(|error| format!("daemon response encode failed: {error}"))?;
    framed
        .send(line)
        .await
        .map_err(|error| format!("daemon response failed: {error}"))
}

fn fixture_runtime_identity() -> RuntimeGenerationIdentity {
    RuntimeGenerationIdentity {
        kind: RuntimeGenerationKind::Development,
        generation_id: fixture_digest('a'),
        builtin_catalog_digest: fixture_digest('b'),
        executable_digest: fixture_digest('c'),
    }
}

fn fixture_digest(fill: char) -> String {
    format!("sha256:{}", fill.to_string().repeat(64))
}

fn fixture_snapshot(
    workspace: &Path,
    session_id: &SessionId,
    daemon_instance_id: &DaemonInstanceId,
) -> SessionPresentationSnapshot {
    let workspace = SanitizedDisplayPath {
        text: workspace.to_string_lossy().into_owned(),
        truncated: false,
    };
    SessionPresentationSnapshot {
        session_id: session_id.clone(),
        cursor: PresentationCursor {
            daemon_instance_id: daemon_instance_id.clone(),
            revision: 1,
        },
        older_page_cursor: None,
        header: SessionHeader {
            session_id: session_id.clone(),
            status: SessionPresentationStatus::Active,
            durable: true,
            resumed: false,
            title: None,
            function_name: "coding".to_owned(),
            model_id: Some("local".to_owned()),
            operation_mode: ProtocolToolMode::Execute,
            selected_skills: Vec::new(),
            runtime_context_revision: 1,
            workspace_root: workspace.clone(),
            cwd: workspace,
            workspace_history_scope: fixture_digest('e'),
            execution_context_revision: 1,
            context_used_tokens: None,
            context_limit_tokens: None,
            active_run_count: 0,
            queued_prompt_count: 0,
            active_execution_count: 0,
        },
        items: Vec::new(),
        active_run: None,
        queued_prompts: Vec::new(),
        terminals: Vec::new(),
        executions: Vec::new(),
        activity: None,
        command_context: agl_protocol::CommandContext {
            session_id: Some(session_id.clone()),
            session_active: true,
            active_or_queued_turns: 0,
            active_executions: 0,
            host_shell_available: true,
            operation_mode: ProtocolToolMode::Execute,
        },
    }
}

impl Drop for DaemonFixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.disconnect();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct ParentTerminalFixture {
    master: File,
    child: Child,
    output: Vec<u8>,
    original: libc::termios,
    answered_cursor_queries: usize,
}

impl ParentTerminalFixture {
    fn spawn(executable: &Path, home: &Path, socket: &Path, workspace: &Path) -> Self {
        let mut master = -1;
        let mut slave = -1;
        let size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &size,
                )
            },
            0
        );
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        assert_eq!(unsafe { libc::tcgetattr(slave, &mut original) }, 0);
        let duplicate = |descriptor| {
            let found = unsafe { libc::dup(descriptor) };
            assert!(found >= 0);
            unsafe { File::from_raw_fd(found) }
        };
        let child = Command::new(executable)
            .arg("--home")
            .arg(home)
            .arg("--socket")
            .arg(socket)
            .arg("--workspace-root")
            .arg(workspace)
            .arg("--no-input-history")
            .env("TERM", "xterm-256color")
            .env("RUST_BACKTRACE", "0")
            .current_dir(workspace)
            .stdin(Stdio::from(duplicate(slave)))
            .stdout(Stdio::from(duplicate(slave)))
            .stderr(Stdio::from(duplicate(slave)))
            .spawn()
            .unwrap();
        unsafe { libc::close(slave) };
        let flags = unsafe { libc::fcntl(master, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        Self {
            master: unsafe { File::from_raw_fd(master) },
            child,
            output: Vec::new(),
            original,
            answered_cursor_queries: 0,
        }
    }

    fn wait_until_ready(&mut self) {
        self.wait_for(READY_TEXT);
        assert!(contains(&self.output, ENABLE_BRACKETED_PASTE));
        self.assert_child_terminal_raw();
    }

    fn write(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
        self.master.flush().unwrap();
    }

    fn resize(&self, columns: u16, rows: u16) {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size) },
            0
        );
    }

    fn send_signal(&self, signal: libc::c_int) {
        assert_eq!(
            unsafe { libc::kill(self.child.id().try_into().unwrap(), signal) },
            0
        );
    }

    fn wait_until_stopped(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let child_pid: libc::pid_t = self.child.id().try_into().unwrap();
        loop {
            self.read_available();
            let mut status = 0;
            let found =
                unsafe { libc::waitpid(child_pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
            if found == child_pid {
                assert!(
                    libc::WIFSTOPPED(status),
                    "child changed state without stopping: {status}; output={}",
                    escaped(&self.output)
                );
                return;
            }
            assert!(
                found >= 0,
                "waitpid failed: {}",
                std::io::Error::last_os_error()
            );
            assert!(Instant::now() < deadline, "TUI did not stop after SIGTSTP");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_until_raw_after_continue(&mut self, previous_clear_count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.read_available();
            let raw = terminal_is_raw(&self.current_termios());
            let clear_count = count_occurrences(&self.output, INLINE_REDRAW_CLEAR);
            if raw && clear_count > previous_clear_count {
                return;
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "TUI exited during SIGCONT recovery: {status}; output={}",
                    escaped(&self.output)
                );
            }
            assert!(
                Instant::now() < deadline,
                "TUI did not recover after SIGCONT: raw={raw}, clear_count={clear_count}, previous_clear_count={previous_clear_count}; output={}",
                escaped(&self.output),
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for(&mut self, needle: &[u8]) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while !contains(&self.output, needle) {
            self.read_available();
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "TUI exited before {:?}: {status}; output={}",
                    String::from_utf8_lossy(needle),
                    escaped(&self.output)
                );
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for TUI output {:?}; output={}",
                String::from_utf8_lossy(needle),
                escaped(&self.output)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn finish(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.read_available();
            if let Some(status) = self.child.try_wait().unwrap() {
                self.read_available();
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "TUI did not finish; output={}",
                escaped(&self.output)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn assert_child_is_running(&mut self) {
        self.read_available();
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "TUI unexpectedly exited: {}",
            escaped(&self.output)
        );
    }

    fn assert_child_terminal_raw(&self) {
        let current = self.current_termios();
        assert!(
            terminal_is_raw(&current),
            "child did not put its terminal in raw mode"
        );
    }

    fn assert_parent_terminal_restored(&self) {
        let current = self.current_termios();
        assert_eq!(current.c_iflag, self.original.c_iflag);
        assert_eq!(current.c_oflag, self.original.c_oflag);
        assert_eq!(current.c_cflag, self.original.c_cflag);
        assert_eq!(current.c_lflag, self.original.c_lflag);
        assert_eq!(current.c_cc, self.original.c_cc);
    }

    fn assert_control_modes_restored(&self) {
        let last_enable = rposition(&self.output, ENABLE_BRACKETED_PASTE)
            .expect("TUI never enabled bracketed-paste mode");
        let last_disable = rposition(&self.output, DISABLE_BRACKETED_PASTE)
            .expect("TUI never disabled bracketed-paste mode");
        assert!(
            last_disable > last_enable,
            "final bracketed-paste control was not disable: {}",
            escaped(&self.output)
        );
        assert!(
            self.output[last_disable..]
                .windows(SHOW_CURSOR.len())
                .any(|candidate| candidate == SHOW_CURSOR),
            "final cleanup did not show the cursor: {}",
            escaped(&self.output)
        );
    }

    fn current_termios(&self) -> libc::termios {
        let mut current = unsafe { std::mem::zeroed::<libc::termios>() };
        assert_eq!(
            unsafe { libc::tcgetattr(self.master.as_raw_fd(), &mut current) },
            0
        );
        current
    }

    fn read_available(&mut self) {
        let mut bytes = [0_u8; 4096];
        loop {
            match self.master.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => self.output.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(error) => panic!("failed to read parent pseudo-terminal: {error}"),
            }
        }
        let query_count = count_occurrences(&self.output, CURSOR_POSITION_QUERY);
        while self.answered_cursor_queries < query_count {
            self.master.write_all(CURSOR_POSITION_REPLY).unwrap();
            self.master.flush().unwrap();
            self.answered_cursor_queries += 1;
        }
    }
}

impl Drop for ParentTerminalFixture {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn terminal_is_raw(termios: &libc::termios) -> bool {
    termios.c_lflag & (libc::ICANON | libc::ECHO) == 0
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|candidate| candidate == needle)
}

fn rposition(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .rposition(|candidate| candidate == needle)
}

fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|candidate| *candidate == needle)
        .count()
}

fn escaped(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| std::ascii::escape_default(*byte))
        .map(char::from)
        .collect()
}
