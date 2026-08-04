#![cfg(all(target_os = "linux", feature = "native-test-fixtures"))]

// End-to-end contract for the separately packaged private launcher.

use std::collections::BTreeMap;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agl_exec::{
    CallerNamespace, CallerOwner, CallerOwnerKind, CallerRole, ExecutionCorrelation, OpaqueOwnerId,
};
use agl_process::{
    EnvironmentOverride, ExecutionAuthorization, ExecutionChannel, ExecutionCursor,
    ExecutionGrantLease, ExecutionId, ExecutionIo, ExecutionKind, ExecutionLimits, ExecutionOwner,
    ExecutionProfile, ExecutionRequest, ExecutionRequestId, ExecutionState, FileOutputSpool,
    InMemoryExecutionRepository, InputLease, ProcessBytes, ProcessErrorCode, ProcessHandle,
    ProcessSupervisor, ProcessSupervisorOptions, TerminalSize, process_platform_diagnostics,
};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

type RunId = ExecutionRequestId;
type StepId = ExecutionRequestId;

const LAUNCHER: &str = env!("CARGO_BIN_EXE_agl-process-launcher");
const HELPER: &str = env!("CARGO_BIN_EXE_agl-process-test-helper");

#[test]
#[ignore = "requires the designated Linux namespace/Landlock/seccomp/pidfd/PTY runner"]
fn native_linux_sandbox_process_and_pty_smoke() {
    let diagnostics = process_platform_diagnostics(LAUNCHER);
    eprintln!(
        "process_platform_diagnostics={}",
        serde_json::to_string(&diagnostics).unwrap()
    );
    assert!(
        diagnostics.supported,
        "designated Linux native smoke cannot skip unsupported isolation: {:?}",
        diagnostics.error_code
    );

    let harness = Harness::new();
    sandbox_contract(&harness);
    argv_pipe_and_pty_contract(&harness);
    supervisor_concurrency_and_backpressure_contract(&harness);
    executable_and_host_contract(&harness);
    termination_and_quota_contract(&harness);
}

fn sandbox_contract(harness: &Harness) {
    let sibling = harness.root.join("sibling-private");
    fs::create_dir_all(&sibling).unwrap();
    let sibling_file = sibling.join("secret");
    let runtime_file = harness.runtime_root.join("runtime-data");
    let workspace_file = harness.workspace.join("workspace-write");
    fs::write(&sibling_file, b"private").unwrap();
    fs::write(&runtime_file, b"runtime").unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();

    let mut request = harness.request(
        vec![
            "sandbox-probe".to_owned(),
            workspace_file.display().to_string(),
            sibling_file.display().to_string(),
            runtime_file.display().to_string(),
            port.to_string(),
        ],
        ExecutionIo::Pipes,
    );
    request.read_only_roots.push(harness.runtime_root.clone());
    let (status, output) = harness.run(request);
    assert_eq!(status.state, ExecutionState::Exited);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    for field in [
        "workspace_write",
        "home_write",
        "tmp_write",
        "sibling_read_denied",
        "sibling_write_denied",
        "runtime_read",
        "runtime_write_denied",
        "dev_null_write",
        "thread_spawn",
        "clone3_unavailable",
        "network_denied",
    ] {
        assert_eq!(report[field], true, "sandbox probe failed field {field}");
    }
    let visible_pids = report["visible_pids"].as_array().unwrap();
    assert!(
        visible_pids.len() <= 8,
        "private /proc exposed unexpected processes: {visible_pids:?}"
    );
    assert!(
        !visible_pids
            .iter()
            .any(|pid| pid.as_u64() == Some(u64::from(std::process::id()))),
        "private /proc exposed the host test process"
    );
    assert_eq!(fs::read(workspace_file).unwrap(), b"workspace-ok");
}

fn argv_pipe_and_pty_contract(harness: &Harness) {
    let side_effect = harness.workspace.join("argv-side-effect");
    let exact = vec![
        "space value".to_owned(),
        "\"quoted\"".to_owned(),
        ";".to_owned(),
        "|".to_owned(),
        "*.rs".to_owned(),
        ">redirect".to_owned(),
        "$value".to_owned(),
        format!("$(touch {})", side_effect.display()),
    ];
    let mut args = vec!["argv-echo".to_owned()];
    args.extend(exact.clone());
    let (_, output) = harness.run(harness.request(args, ExecutionIo::Pipes));
    assert_eq!(
        serde_json::from_slice::<Vec<String>>(&output.stdout).unwrap(),
        exact
    );
    assert!(!side_effect.exists());

    let mut initial = harness.request(vec!["stdin-echo".to_owned()], ExecutionIo::Pipes);
    initial.stdin = Some(ProcessBytes::from_bytes(b"initial stdin\0bytes"));
    let (_, output) = harness.run(initial);
    assert_eq!(output.stdout, b"initial stdin\0bytes");

    let mut echo = harness.request(vec!["stdin-echo".to_owned()], ExecutionIo::Pipes);
    echo.close_stdin_after_initial = false;
    let started = harness.handle.start(echo).unwrap();
    let writer_id = ExecutionRequestId::generate();
    let writer = harness
        .handle
        .attach(
            &started.execution_id,
            &harness.owner,
            writer_id.clone(),
            true,
        )
        .unwrap();
    assert_eq!(writer.attachment_id, writer_id);
    let reader = harness
        .handle
        .attach(
            &started.execution_id,
            &harness.owner,
            ExecutionRequestId::generate(),
            false,
        )
        .unwrap();
    let second_reader = harness
        .handle
        .attach(
            &started.execution_id,
            &harness.owner,
            ExecutionRequestId::generate(),
            false,
        )
        .unwrap();
    assert_eq!(
        harness
            .handle
            .attach(
                &started.execution_id,
                &harness.owner,
                ExecutionRequestId::generate(),
                true,
            )
            .unwrap_err()
            .code(),
        ProcessErrorCode::InputLeaseBusy
    );
    harness
        .handle
        .detach(&started.execution_id, &harness.owner, reader)
        .unwrap();
    harness
        .handle
        .detach(&started.execution_id, &harness.owner, second_reader)
        .unwrap();
    harness
        .handle
        .write(
            &started.execution_id,
            &harness.owner,
            writer.clone(),
            ProcessBytes::from_bytes(b"stdin\0bytes"),
            true,
        )
        .unwrap();
    harness
        .handle
        .detach(&started.execution_id, &harness.owner, writer)
        .unwrap();
    let status = harness.wait(&started.execution_id);
    let output = harness.output(&started.execution_id);
    assert_eq!(status.state, ExecutionState::Exited);
    assert_eq!(output.stdout, b"stdin\0bytes");

    let (binary_status, binary) =
        harness.run(harness.request(vec!["binary-stdio".to_owned()], ExecutionIo::Pipes));
    assert_eq!(binary.stdout, [b'o', 0, 0xff, b'\n']);
    assert_eq!(binary.stderr, [b'e', 0xfe, 0, b'\n']);
    let retained_reader = harness
        .handle
        .attach(
            &binary_status.execution_id,
            &harness.owner,
            ExecutionRequestId::generate(),
            false,
        )
        .unwrap();
    harness
        .handle
        .detach(&binary_status.execution_id, &harness.owner, retained_reader)
        .unwrap();

    let (_, closed) =
        harness.run(harness.request(vec!["close-stdout".to_owned()], ExecutionIo::Pipes));
    assert_eq!(closed.stderr, b"stderr-after-stdout-close\n");

    let mut pty = harness.request(vec!["tty-info".to_owned()], ExecutionIo::Pty);
    pty.terminal_size = Some(TerminalSize {
        columns: 80,
        rows: 24,
    });
    let (_, output) = harness.run(pty);
    let tty: Value = serde_json::from_slice(trim_ascii(&output.terminal)).unwrap();
    for field in [
        "stdin_tty",
        "stdout_tty",
        "stderr_tty",
        "session_leader",
        "controlling_terminal",
    ] {
        assert_eq!(tty[field], true, "PTY assertion failed field {field}");
    }
    assert_eq!(tty["columns"], 80);
    assert_eq!(tty["rows"], 24);

    let mut resize = harness.request(vec!["resize-wait".to_owned()], ExecutionIo::Pty);
    resize.terminal_size = Some(TerminalSize::default());
    let started = harness.handle.start(resize).unwrap();
    let lease = wait_for_output(harness, &started.execution_id, b"initial=80x24", Some(true));
    harness
        .handle
        .detach(&started.execution_id, &harness.owner, lease.unwrap())
        .unwrap();
    assert!(
        harness
            .handle
            .status(&started.execution_id, &harness.owner)
            .unwrap()
            .state
            .is_live(),
        "detach unexpectedly terminated the PTY target"
    );
    harness
        .handle
        .resize(
            &started.execution_id,
            &harness.owner,
            TerminalSize {
                columns: 100,
                rows: 40,
            },
        )
        .unwrap();
    harness.wait(&started.execution_id);
    let output = harness.output(&started.execution_id);
    assert!(contains(&output.terminal, b"resized=100x40"));

    let mut redraw = harness.request(vec!["resize-wait".to_owned()], ExecutionIo::Pty);
    redraw.terminal_size = Some(TerminalSize::default());
    let started = harness.handle.start(redraw).unwrap();
    wait_for_output(harness, &started.execution_id, b"initial=80x24", Some(true));
    harness
        .handle
        .resize(
            &started.execution_id,
            &harness.owner,
            TerminalSize::default(),
        )
        .unwrap();
    harness.wait(&started.execution_id);
    let output = harness.output(&started.execution_id);
    assert!(contains(&output.terminal, b"resized=80x24"));

    let mut interactive = harness.request(vec!["signal-eof".to_owned()], ExecutionIo::Pty);
    interactive.close_stdin_after_initial = false;
    let started = harness.handle.start(interactive).unwrap();
    wait_for_output(harness, &started.execution_id, b"ready", None);
    let writer = harness
        .handle
        .attach(
            &started.execution_id,
            &harness.owner,
            ExecutionRequestId::generate(),
            true,
        )
        .unwrap();
    harness
        .handle
        .write(
            &started.execution_id,
            &harness.owner,
            writer,
            ProcessBytes::from_bytes(b"interactive request\n"),
            true,
        )
        .unwrap();
    let status = harness.wait(&started.execution_id);
    assert_eq!(status.state, ExecutionState::Exited);
    let output = harness.output(&started.execution_id);
    assert!(contains(&output.terminal, b"interactive request\r\n"));
    let json_start = output
        .terminal
        .windows(2)
        .position(|bytes| bytes == b"{\"")
        .expect("PTY helper must emit its EOF report");
    let report_line = output.terminal[json_start..]
        .split(|byte| *byte == b'\r' || *byte == b'\n')
        .next()
        .unwrap();
    let report: Value = serde_json::from_slice(report_line).unwrap();
    assert_eq!(report["eof"], true);
    assert_eq!(
        report["input_bytes"],
        b"interactive request\n".len(),
        "unexpected PTY EOF report: {report}"
    );
}

fn supervisor_concurrency_and_backpressure_contract(harness: &Harness) {
    let mut concurrent = Vec::new();
    for _ in 0..4 {
        let request = harness.request(
            vec!["sleep-ms".to_owned(), "50".to_owned()],
            ExecutionIo::Pipes,
        );
        concurrent.push(harness.handle.start(request).unwrap().execution_id);
    }
    for execution_id in concurrent {
        assert_eq!(harness.wait(&execution_id).state, ExecutionState::Exited);
    }

    let mut blocked = harness.request(
        vec!["sleep-ms".to_owned(), "5000".to_owned()],
        ExecutionIo::Pipes,
    );
    blocked.close_stdin_after_initial = false;
    let started = harness.handle.start(blocked).unwrap();
    let writer = harness
        .handle
        .attach(
            &started.execution_id,
            &harness.owner,
            ExecutionRequestId::generate(),
            true,
        )
        .unwrap();
    harness
        .handle
        .write(
            &started.execution_id,
            &harness.owner,
            writer.clone(),
            ProcessBytes::from_bytes(&vec![b'x'; 65_536]),
            false,
        )
        .unwrap();
    assert_eq!(
        harness
            .handle
            .write(
                &started.execution_id,
                &harness.owner,
                writer,
                ProcessBytes::from_bytes(b"overflow"),
                false,
            )
            .unwrap_err()
            .code(),
        ProcessErrorCode::InputBackpressure
    );
    harness
        .handle
        .kill(
            &started.execution_id,
            &harness.owner,
            agl_process::KillMode::Immediate,
        )
        .unwrap();
    assert_eq!(
        harness.wait(&started.execution_id).state,
        ExecutionState::Cancelled
    );
}

fn executable_and_host_contract(harness: &Harness) {
    let frozen_marker = harness.workspace.join("frozen-shell-target-ran");
    let mut frozen = harness.request(
        vec!["touch".to_owned(), frozen_marker.display().to_string()],
        ExecutionIo::Pty,
    );
    frozen.kind = ExecutionKind::Shell;
    frozen.program_digest = Some(format!("sha256:{}", "0".repeat(64)));
    assert_eq!(
        harness.handle.start(frozen.clone()).unwrap_err().code(),
        ProcessErrorCode::SandboxExecutableUnavailable
    );
    assert!(!frozen_marker.exists(), "digest-mismatched shell executed");
    frozen.program_digest = Some(sha256_file(&harness.helper));
    let (status, _) = harness.run(frozen);
    assert_eq!(status.state, ExecutionState::Exited);
    assert!(
        frozen_marker.exists(),
        "frozen shell handle did not execute"
    );

    let unadmitted_dir = harness.root.join("unadmitted-home-bin");
    fs::create_dir_all(&unadmitted_dir).unwrap();
    let unadmitted = unadmitted_dir.join("helper");
    fs::copy(&harness.helper, &unadmitted).unwrap();
    let unadmitted = unadmitted.canonicalize().unwrap();
    let marker = harness.workspace.join("unadmitted-target-ran");
    let mut request = harness.request(
        vec!["touch".to_owned(), marker.display().to_string()],
        ExecutionIo::Pipes,
    );
    request.program = unadmitted.clone();
    let error = harness.handle.start(request.clone()).unwrap_err();
    assert_eq!(error.code(), ProcessErrorCode::SandboxExecutableUnavailable);
    assert!(!marker.exists(), "unadmitted workspace target executed");

    request.profile = ExecutionProfile::Host;
    assert_eq!(
        harness.handle.start(request.clone()).unwrap_err().code(),
        ProcessErrorCode::HostAuthorityRequired
    );
    request.authorization.host_process_execution = true;
    request.grant_lease = Some(ExecutionGrantLease {
        origin: agl_process::ExecutionLeaseOrigin::ToolGrant,
        grant_id: "native-smoke-host-grant".to_owned(),
        duration: "one_turn".to_owned(),
        scope_digest: "sha256:native-smoke".to_owned(),
    });
    let (status, _) = harness.run(request.clone());
    assert_eq!(status.state, ExecutionState::Exited);
    assert!(marker.exists(), "authorized host target did not execute");

    request.authorization.shell_login_startup = true;
    let (status, _) = harness.run(request);
    assert_eq!(status.state, ExecutionState::Exited);

    let mut workspace_login = harness.request(
        vec!["argv-echo".to_owned(), "login".to_owned()],
        ExecutionIo::Pipes,
    );
    workspace_login.authorization.shell_login_startup = true;
    assert_eq!(
        harness.handle.start(workspace_login).unwrap_err().code(),
        ProcessErrorCode::LoginAuthorityRequired
    );
}

fn termination_and_quota_contract(harness: &Harness) {
    let mut timeout = harness.request(
        vec!["sleep-ms".to_owned(), "5000".to_owned()],
        ExecutionIo::Pipes,
    );
    timeout.limits.timeout_ms = Some(50);
    let (status, _) = harness.run(timeout);
    assert_eq!(status.state, ExecutionState::TimedOut);

    let mut quota = harness.request(
        vec!["long-output".to_owned(), "1048576".to_owned()],
        ExecutionIo::Pipes,
    );
    quota.limits.max_output_bytes = 4_096;
    let (status, output) = harness.run(quota);
    assert_eq!(
        status.error_code.as_deref(),
        Some(ProcessErrorCode::OutputLimitExceeded.as_str())
    );
    assert!(status.output_truncated);
    assert!(status.discarded_output_bytes > 0);
    assert_eq!(output.stdout.len(), 4_096);

    let mut ignored = harness.request(
        vec!["ignore-term".to_owned(), "5000".to_owned()],
        ExecutionIo::Pipes,
    );
    ignored.limits.timeout_ms = Some(50);
    let (status, _) = harness.run(ignored);
    assert_eq!(status.state, ExecutionState::TimedOut);

    let mut exit = harness.request(
        vec!["exit-code".to_owned(), "17".to_owned()],
        ExecutionIo::Pipes,
    );
    exit.limits.timeout_ms = Some(1_000);
    let (status, _) = harness.run(exit);
    assert!(matches!(
        status.exit,
        Some(agl_process::ExecutionExit::Code { code: 17 })
    ));
}

struct Harness {
    root: PathBuf,
    workspace: PathBuf,
    runtime_root: PathBuf,
    helper: PathBuf,
    owner: ExecutionOwner,
    handle: ProcessHandle,
    supervisor: ProcessSupervisor,
}

impl Harness {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("agl-process-linux-native-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let runtime_root = root.join("runtime-root");
        fs::create_dir_all(&runtime_root).unwrap();
        let runtime_root = runtime_root.canonicalize().unwrap();
        let helper = Path::new(HELPER).canonicalize().unwrap();
        let launcher = Path::new(LAUNCHER).canonicalize().unwrap();
        let options = ProcessSupervisorOptions {
            launcher_path: launcher,
            data_root: root.join("data"),
            state_root: root.join("state"),
            max_active: 8,
            command_capacity: 128,
            poll_interval: Duration::from_millis(2),
            setup_timeout: Duration::from_secs(5),
            termination_grace: Duration::from_millis(50),
            max_input_bytes: 65_536,
            max_result_bytes: 65_536,
            max_spool_bytes: 2 * 1024 * 1024,
            termination_output_headroom_bytes: 4_096,
            finished_retention: Duration::from_secs(60),
            runtime_read_only_roots: vec![
                helper.parent().unwrap().to_path_buf(),
                runtime_root.clone(),
            ],
        };
        let repository = Arc::new(InMemoryExecutionRepository::new());
        let spool = Arc::new(FileOutputSpool::new(root.join("spool")).unwrap());
        let supervisor = ProcessSupervisor::start(options, repository, spool).unwrap();
        let handle = supervisor.handle();
        let root_run_id = RunId::generate();
        Self {
            root,
            workspace,
            runtime_root,
            helper,
            owner: ExecutionOwner::new(
                CallerOwner::new(
                    CallerNamespace::new("agentlibre", 1).unwrap(),
                    OpaqueOwnerId::new(root_run_id.as_str()).unwrap(),
                    CallerOwnerKind::Ephemeral,
                    CallerRole::Agent,
                ),
                OpaqueOwnerId::new(root_run_id.as_str()).unwrap(),
            ),
            handle,
            supervisor,
        }
    }

    fn request(&self, args: Vec<String>, io: ExecutionIo) -> ExecutionRequest {
        let creating_run_id = self.owner.caller().owner_id().as_str();
        let creating_step_id = StepId::generate();
        ExecutionRequest {
            owner: self.owner.clone(),
            correlation: ExecutionCorrelation::new(
                CallerNamespace::new("agentlibre", 1).unwrap(),
                OpaqueOwnerId::new(creating_run_id).unwrap(),
                OpaqueOwnerId::new(creating_step_id.as_str()).unwrap(),
            ),
            kind: ExecutionKind::Argv,
            program: self.helper.clone(),
            argv0: self.helper.display().to_string(),
            program_digest: None,
            args,
            workspace_root: self.workspace.clone(),
            cwd: self.workspace.clone(),
            read_only_roots: vec![self.helper.parent().unwrap().to_path_buf()],
            environment: EnvironmentOverride {
                values: BTreeMap::new(),
            },
            stdin: None,
            close_stdin_after_initial: true,
            io,
            terminal_size: (io == ExecutionIo::Pty).then_some(TerminalSize::default()),
            profile: ExecutionProfile::Workspace,
            authorization: ExecutionAuthorization::default(),
            grant_lease: None,
            limits: ExecutionLimits {
                timeout_ms: Some(5_000),
                max_input_bytes: 65_536,
                max_output_bytes: 1024 * 1024,
            },
        }
    }

    fn run(&self, request: ExecutionRequest) -> (agl_process::ExecutionStatus, CapturedOutput) {
        let started = self.handle.start(request).unwrap();
        let status = self.wait(&started.execution_id);
        let output = self.output(&started.execution_id);
        (status, output)
    }

    fn wait(&self, execution_id: &ExecutionId) -> agl_process::ExecutionStatus {
        self.handle
            .wait(
                execution_id,
                &self.owner,
                Some(Instant::now() + Duration::from_secs(10)),
                || false,
            )
            .unwrap()
    }

    fn output(&self, execution_id: &ExecutionId) -> CapturedOutput {
        let mut output = CapturedOutput::default();
        let mut cursor = 0;
        for _ in 0..128 {
            let read = self
                .handle
                .read(
                    execution_id,
                    &self.owner,
                    ExecutionCursor {
                        after_sequence: cursor,
                    },
                    65_536,
                )
                .unwrap();
            for chunk in read.chunks {
                let bytes = chunk.bytes.decode(65_536).unwrap();
                match chunk.channel {
                    ExecutionChannel::Stdout => output.stdout.extend(bytes),
                    ExecutionChannel::Stderr => output.stderr.extend(bytes),
                    ExecutionChannel::Terminal => output.terminal.extend(bytes),
                    ExecutionChannel::Lifecycle => {}
                }
                cursor = cursor.max(chunk.sequence);
            }
            if read.state.is_terminal() && read.next_sequence <= cursor {
                return output;
            }
            cursor = cursor.max(read.next_sequence);
        }
        panic!("process output replay did not converge")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.supervisor.shutdown();
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[derive(Default)]
struct CapturedOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    terminal: Vec<u8>,
}

fn wait_for_output(
    harness: &Harness,
    execution_id: &ExecutionId,
    needle: &[u8],
    writable: Option<bool>,
) -> Option<InputLease> {
    let lease = writable.map(|writable| {
        harness
            .handle
            .attach(
                execution_id,
                &harness.owner,
                ExecutionRequestId::generate(),
                writable,
            )
            .unwrap()
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut cursor = 0;
    let mut terminal = Vec::new();
    loop {
        let read = harness
            .handle
            .read(
                execution_id,
                &harness.owner,
                ExecutionCursor {
                    after_sequence: cursor,
                },
                65_536,
            )
            .unwrap();
        for chunk in read.chunks {
            if chunk.channel == ExecutionChannel::Terminal {
                terminal.extend(chunk.bytes.decode(65_536).unwrap());
            }
            cursor = cursor.max(chunk.sequence);
        }
        cursor = cursor.max(read.next_sequence);
        if contains(&terminal, needle) {
            return lease;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for PTY output"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|candidate| candidate == needle)
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &bytes[start..end]
}

fn sha256_file(path: &Path) -> String {
    use std::fmt::Write as _;

    let mut rendered = String::from("sha256:");
    for byte in Sha256::digest(fs::read(path).unwrap()) {
        write!(&mut rendered, "{byte:02x}").unwrap();
    }
    rendered
}
