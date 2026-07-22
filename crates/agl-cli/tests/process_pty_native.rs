#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use agl_chat::{ChatInferenceJob, InferenceClient, InferenceClientHandle, InferenceOptions};
use agl_config::ResolvedInferenceConfig;
use agl_ids::{ExecutionId, RunId, SessionId, StepId};
use agl_inference::{InferenceResponse, ModelManagerStatus, WorkerRuntimeStatusHandle};
use agl_process::{
    EnvironmentOverride, ExecutionAuthorization, ExecutionIo, ExecutionKind, ExecutionLimits,
    ExecutionOwner, ExecutionProfile, ExecutionRequest, ExecutionState, TerminalSize,
    WRITABLE_INPUT_LEASE_TTL,
};
use agl_runtime::{AgentLibrePaths, AgentLibreRuntimeConfig};

const DETACH: u8 = 0x1d;

struct NoInference;

impl InferenceClient for NoInference {
    fn device_inventory(&self) -> anyhow::Result<Vec<agl_inference::InferenceDeviceInfo>> {
        Ok(Vec::new())
    }

    fn generate(&self, _job: ChatInferenceJob) -> anyhow::Result<InferenceResponse> {
        anyhow::bail!("native process attach test never invokes inference")
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
#[ignore = "requires the designated Linux namespace/Landlock/seccomp/pidfd/PTY runner"]
fn cli_attach_detach_reattach_and_kill_real_daemon_owned_pty() {
    let target_dir = test_target_dir();
    let launcher = target_dir.join("agl-process-launcher");
    let helper = target_dir.join("agl-process-test-helper");
    assert!(
        launcher.is_file(),
        "missing native launcher at {}",
        launcher.display()
    );
    assert!(
        helper.is_file(),
        "missing native helper at {}",
        helper.display()
    );

    let root = std::env::temp_dir().join(format!(
        "ap-{}-{}",
        std::process::id(),
        ExecutionId::generate()
    ));
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let paths = AgentLibrePaths::from_agl_home(root.join("agl-home"));
    write_runtime_config(&paths, &workspace, &target_dir);
    let runtime = AgentLibreRuntimeConfig::from_paths(paths.clone()).unwrap();

    let state = agl_daemon::SharedDaemonState::open(
        runtime.clone(),
        InferenceOptions::default(),
        InferenceClientHandle::new(NoInference),
        WorkerRuntimeStatusHandle::default(),
    )
    .unwrap();
    let process = state.process_handle().unwrap();
    let run_id = RunId::generate();
    let owner = ExecutionOwner::Run {
        run_id: run_id.clone(),
        root_run_id: run_id.clone(),
    };
    let started = process
        .start(ExecutionRequest {
            owner,
            creating_run_id: run_id,
            creating_step_id: StepId::generate(),
            kind: ExecutionKind::Argv,
            program: helper.canonicalize().unwrap(),
            program_digest: None,
            args: vec!["interactive-lines".to_owned()],
            workspace_root: workspace.clone(),
            cwd: workspace.clone(),
            read_only_roots: vec![target_dir.canonicalize().unwrap()],
            environment: EnvironmentOverride {
                values: BTreeMap::new(),
            },
            stdin: None,
            close_stdin_after_initial: false,
            io: ExecutionIo::Pty,
            terminal_size: Some(TerminalSize::default()),
            profile: ExecutionProfile::Workspace,
            authorization: ExecutionAuthorization::default(),
            grant_lease: None,
            limits: ExecutionLimits {
                timeout_ms: Some(90_000),
                max_input_bytes: 65_536,
                max_output_bytes: 1_048_576,
            },
        })
        .unwrap();

    let socket = agl_daemon::default_socket_path(&runtime.paths);
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let listener = UnixListener::bind(&socket).unwrap();
    let server_state = state.clone();
    let server = std::thread::spawn(move || {
        let async_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for _ in 0..3 {
            let (stream, _) = listener.accept().unwrap();
            stream.set_nonblocking(true).unwrap();
            async_runtime
                .block_on(async {
                    let stream = tokio::net::UnixStream::from_std(stream).unwrap();
                    agl_daemon::serve_connection(stream, &server_state).await
                })
                .unwrap();
        }
    });

    let mut first = PtyChild::spawn_attach(&runtime.paths, &workspace, &started.execution_id, 0);
    first.wait_for(b"ready\r\n");
    std::thread::sleep(WRITABLE_INPUT_LEASE_TTL + Duration::from_secs(2));
    assert!(
        first.child.try_wait().unwrap().is_none(),
        "idle writable CLI attachment did not survive one lease TTL"
    );
    first.resize(100, 40);
    first.wait_for(b"resized=100x40\r\n");
    first.write(b"hello\n");
    first.wait_for(b"reply:hello\r\n");
    first.write(b"emit-terminal-effects\n");
    first.wait_for(b"filter-beforefilter-after\r\n");
    for private_payload in [
        b"PRIVATE_CLIPBOARD".as_slice(),
        b"PRIVATE_TITLE".as_slice(),
        b"PRIVATE_DCS".as_slice(),
        b"PRIVATE_APC".as_slice(),
        b"PRIVATE_PM".as_slice(),
    ] {
        assert!(
            !contains(&first.output, private_payload),
            "standalone process attach exposed a blocked terminal payload: {}",
            String::from_utf8_lossy(private_payload)
        );
    }
    first.write(&[DETACH]);
    let first_output = first.finish();
    first.assert_termios_restored();
    let first_cursor = attachment_cursor(&first_output);
    assert!(
        process
            .operator_status(&started.execution_id)
            .unwrap()
            .state
            .is_live(),
        "Ctrl-] unexpectedly terminated the daemon-owned target"
    );

    let mut second = PtyChild::spawn_attach(
        &runtime.paths,
        &workspace,
        &started.execution_id,
        first_cursor,
    );
    second.write(b"again\n");
    second.wait_for(b"reply:again\r\n");
    second.write(&[DETACH]);
    let second_output = second.finish();
    second.assert_termios_restored();
    assert!(
        !contains(&second_output, b"reply:hello\r\n"),
        "reattach replayed output at or before the supplied cursor"
    );
    assert!(attachment_cursor(&second_output) > first_cursor);

    let kill = Command::new(env!("CARGO_BIN_EXE_agl"))
        .args([
            "process",
            "kill",
            started.execution_id.as_str(),
            "--yes",
            "--immediate",
        ])
        .env("AGL_HOME", runtime.paths.config_dir.parent().unwrap())
        .current_dir(&workspace)
        .output()
        .unwrap();
    assert!(
        kill.status.success(),
        "kill failed: stdout={} stderr={}",
        String::from_utf8_lossy(&kill.stdout),
        String::from_utf8_lossy(&kill.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = process.operator_status(&started.execution_id).unwrap();
        if status.state.is_terminal() {
            assert_eq!(status.state, ExecutionState::Cancelled);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "killed PTY did not become terminal"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    server.join().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

struct PtyChild {
    master: File,
    child: Child,
    output: Vec<u8>,
    original: libc::termios,
}

impl PtyChild {
    fn spawn_attach(
        paths: &AgentLibrePaths,
        workspace: &Path,
        execution_id: &ExecutionId,
        after: u64,
    ) -> Self {
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
            .args([
                "process",
                "attach",
                execution_id.as_str(),
                "--after",
                &after.to_string(),
            ])
            .env("AGL_HOME", paths.config_dir.parent().unwrap())
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
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
        self.master.flush().unwrap();
    }

    fn resize(&mut self, columns: u16, rows: u16) {
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
        assert_eq!(
            unsafe { libc::kill(self.child.id().try_into().unwrap(), libc::SIGWINCH) },
            0
        );
    }

    fn wait_for(&mut self, needle: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !contains(&self.output, needle) {
            self.read_available();
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "attach exited before {:?}: {status}; output={}",
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(&self.output)
                );
            }
            assert!(Instant::now() < deadline, "timed out waiting for PTY bytes");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn finish(&mut self) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.read_available();
            if let Some(status) = self.child.try_wait().unwrap() {
                self.read_available();
                assert!(
                    status.success(),
                    "attach failed: {status}; output={}",
                    String::from_utf8_lossy(&self.output)
                );
                return self.output.clone();
            }
            assert!(
                Instant::now() < deadline,
                "attach did not finish after detach"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn read_available(&mut self) {
        let mut bytes = [0_u8; 4096];
        loop {
            match self.master.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => self.output.extend_from_slice(&bytes[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(error) => panic!("failed to read pseudo-terminal: {error}"),
            }
        }
    }

    fn assert_termios_restored(&self) {
        let mut current = unsafe { std::mem::zeroed::<libc::termios>() };
        assert_eq!(
            unsafe { libc::tcgetattr(self.master.as_raw_fd(), &mut current) },
            0
        );
        assert_eq!(current.c_iflag, self.original.c_iflag);
        assert_eq!(current.c_oflag, self.original.c_oflag);
        assert_eq!(current.c_cflag, self.original.c_cflag);
        assert_eq!(current.c_lflag, self.original.c_lflag);
        assert_eq!(current.c_cc, self.original.c_cc);
    }
}

impl Drop for PtyChild {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn write_runtime_config(paths: &AgentLibrePaths, workspace: &Path, runtime_root: &Path) {
    let path = paths.runtime_config_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let workspace = serde_json::to_string(&workspace.to_string_lossy()).unwrap();
    let runtime_root = serde_json::to_string(&runtime_root.to_string_lossy()).unwrap();
    std::fs::write(
        path,
        format!(
            "[workspace]\nroot = {workspace}\n\n[execution]\nruntime_read_only_roots = [{runtime_root}]\n"
        ),
    )
    .unwrap();
}

fn test_target_dir() -> PathBuf {
    let current = std::env::current_exe().unwrap();
    let parent = current.parent().unwrap();
    if parent.file_name().is_some_and(|name| name == "deps") {
        parent.parent().unwrap().to_path_buf()
    } else {
        parent.to_path_buf()
    }
}

fn attachment_cursor(output: &[u8]) -> u64 {
    let text = String::from_utf8_lossy(output);
    let marker = text.rfind("cursor=").expect("missing attachment cursor");
    text[marker + "cursor=".len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|candidate| candidate == needle)
}
