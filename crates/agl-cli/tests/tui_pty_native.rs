#![cfg(target_os = "linux")]

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use agl_chat::{ChatInferenceJob, InferenceClient, InferenceClientHandle, InferenceOptions};
use agl_config::ResolvedInferenceConfig;
use agl_ids::SessionId;
use agl_inference::{InferenceResponse, ModelManagerStatus};
use agl_runtime::{AgentLibrePaths, AgentLibreRuntimeConfig};

const READY_TEXT: &[u8] = b"agentLIBRE";
const ENABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004h";
const DISABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";
const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";
const CURSOR_POSITION_REPLY: &[u8] = b"\x1b[1;1R";
// Ratatui's inline viewport redraws from the cursor down instead of clearing
// the full physical screen.
const INLINE_REDRAW_CLEAR: &[u8] = b"\x1b[J";

struct NoInference;

impl InferenceClient for NoInference {
    fn generate(&self, _job: ChatInferenceJob) -> anyhow::Result<InferenceResponse> {
        anyhow::bail!("native TUI restoration test never invokes inference")
    }

    fn clear_context(
        &self,
        _config: &ResolvedInferenceConfig,
        _session_id: &SessionId,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn release_context(
        &self,
        _config: &ResolvedInferenceConfig,
        _session_id: &SessionId,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn status(&self) -> anyhow::Result<ModelManagerStatus> {
        Ok(ModelManagerStatus::default())
    }
}

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
    paths: AgentLibrePaths,
    socket: PathBuf,
    server: Option<DaemonFixture>,
}

impl NativeTuiEnvironment {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "agl-tui-pty-{}-{}",
            std::process::id(),
            agl_ids::RequestId::generate()
        ));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let paths = AgentLibrePaths::from_agl_home(root.join("agl-home"));
        write_runtime_config(&paths, &workspace);
        let inference_config = write_inference_config(&root);
        let runtime = AgentLibreRuntimeConfig::from_paths(paths.clone()).unwrap();
        let state = agl_daemon::SharedDaemonState::open(
            runtime,
            InferenceOptions {
                config: Some(inference_config),
                ..InferenceOptions::default()
            },
            InferenceClientHandle::new(NoInference),
        )
        .unwrap();
        let socket = root.join("daemon.sock");
        let server = DaemonFixture::start(&socket, state);
        Self {
            root,
            workspace,
            paths,
            socket,
            server: Some(server),
        }
    }

    fn spawn_tui(&self) -> ParentTerminalFixture {
        ParentTerminalFixture::spawn(&self.paths, &self.socket, &self.workspace)
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
    }
}

impl Drop for NativeTuiEnvironment {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            drop(server);
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct DaemonFixture {
    stop: Arc<AtomicBool>,
    disconnect: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl DaemonFixture {
    fn start(socket: &Path, state: agl_daemon::SharedDaemonState) -> Self {
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (disconnect, disconnected) = tokio::sync::oneshot::channel();
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
                    tokio::select! {
                        result = agl_daemon::serve_connection(stream, &state) => {
                            result.map_err(|error| format!("daemon connection failed: {error:#}"))
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
    fn spawn(paths: &AgentLibrePaths, socket: &Path, workspace: &Path) -> Self {
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
        let child = Command::new(env!("CARGO_BIN_EXE_agl"))
            .arg("--home")
            .arg(paths.config_dir.parent().unwrap())
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
        let deadline = Instant::now() + Duration::from_secs(5);
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
                "timed out waiting for TUI output"
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

fn write_runtime_config(paths: &AgentLibrePaths, workspace: &Path) {
    let path = paths.runtime_config_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let workspace = serde_json::to_string(&workspace.to_string_lossy()).unwrap();
    std::fs::write(path, format!("[workspace]\nroot = {workspace}\n")).unwrap();
}

fn write_inference_config(root: &Path) -> PathBuf {
    let path = root.join("inference.toml");
    let model = serde_json::to_string(&root.join("unused.gguf").to_string_lossy()).unwrap();
    std::fs::write(
        &path,
        format!(
            "[backend]\nkind = \"llama_cpp\"\nmodel = {model}\n\n[runtime]\ngpu_layers = 0\ncontext_tokens = 128\nthreads = 1\nbatch_size = 16\nubatch_size = 16\n\n[model]\ndialect = \"qwen3\"\ntool_call_format = \"hermes_json\"\n"
        ),
    )
    .unwrap();
    path
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
