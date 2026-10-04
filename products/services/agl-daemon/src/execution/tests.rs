use super::*;
use agl_core::agent::AgentOperationKey;
use std::num::NonZeroU32;

struct Harness {
    backend: ExecutionBackend,
    root: PathBuf,
    owner: String,
}
impl Harness {
    fn new() -> Self {
        let root = PathBuf::from("/tmp").join(format!("agl-systemd-test-{}", uuid::Uuid::now_v7()));
        private_dir(&root).unwrap();
        let store = StoreHandle::open_at(root.join("store")).unwrap();
        let owner = format!("agl-test-owner-{}.service", uuid::Uuid::now_v7());
        checked(
            Command::new(program("systemd-run").unwrap())
                .args(["--user", "--quiet", "--collect", "--service-type=exec"])
                .arg(format!("--unit={owner}"))
                .arg("--")
                .arg(program("sleep").unwrap())
                .arg("infinity"),
        )
        .unwrap();
        let mut backend = ExecutionBackend::new(
            &root.join("data"),
            &root.join("state"),
            store,
            owner.clone(),
        )
        .unwrap();
        Arc::get_mut(&mut backend.0).unwrap().helper = PathBuf::from(
            std::env::var_os("AGL_TEST_HELPER")
                .expect("AGL_TEST_HELPER must name the built agl binary"),
        );
        Self {
            backend,
            root,
            owner,
        }
    }
    fn request(&self, script: &str) -> ExecutionStartRequest {
        ExecutionStartRequest {
            owner: ExecutionOwner::Agent {
                conversation_id: agl_core::ConversationId::generate(),
                run_id: agl_core::AgentRunId::generate(),
            },
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            cwd: self.root.to_string_lossy().into_owned(),
            environment: std::collections::BTreeMap::from([(
                "PATH".into(),
                std::env::var("PATH").unwrap(),
            )]),
            clear_environment: true,
            io: ExecutionIo::Pipes,
            timeout_ms: 0,
            max_output_bytes: 64,
            terminal_size: None,
            isolation: ExecutionIsolation::Standard,
        }
    }
    fn wait(&self, id: ExecutionId) -> ExecutionStatus {
        let until = Instant::now() + Duration::from_secs(15);
        loop {
            let status = self.backend.inspect(id).unwrap();
            if status.state != ExecutionState::Running {
                return status;
            }
            assert!(Instant::now() < until, "execution did not complete");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        if let Ok(entries) = fs::read_dir(&self.backend.0.root) {
            for entry in entries.flatten() {
                if let Ok(id) = ExecutionId::parse(&entry.file_name().to_string_lossy()) {
                    let _ = Command::new(&self.backend.0.systemctl)
                        .args(["--user", "stop", &unit(id)])
                        .output();
                }
            }
        }
        let _ = Command::new(&self.backend.0.systemctl)
            .args(["--user", "stop", &self.owner, &self.backend.0.tmux_unit])
            .output();
        if !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.root);
        } else {
            eprintln!("Evidence retained at {}", self.root.display());
        }
    }
}

#[test]
fn wait_for_execution_without_a_successful_start_is_recoverable() {
    use agl_core::agent::{
        AbsolutePath, AuthorityGrantSet, RelativePath, ToolFailureEffect, WorkspaceScope,
    };
    use agl_tool_api::{ToolCancellation, ToolContext};

    let root =
        std::env::temp_dir().join(format!("agl-execution-missing-id-{}", uuid::Uuid::now_v7()));
    private_dir(&root).unwrap();
    let store = StoreHandle::open_at(root.join("store")).unwrap();
    let backend = ExecutionBackend::new(
        &root.join("data"),
        &root.join("state"),
        store,
        "agl-execution-missing-id-test.service".into(),
    )
    .unwrap();
    let conversation_id = agl_core::ConversationId::generate();
    let context = ToolContext::new(
        AgentOperationKey {
            run_id: agl_core::AgentRunId::generate(),
            ordinal: NonZeroU32::MIN,
        },
        Some(conversation_id),
        WorkspaceScope {
            root: AbsolutePath::try_from(root.to_string_lossy().into_owned()).unwrap(),
            working_directory: RelativePath::try_from(".".to_owned()).unwrap(),
        },
        AuthorityGrantSet(Vec::new()),
        i64::MAX,
        4096,
        ToolCancellation::new(),
    );

    let failure = backend
        .tool_action(
            context,
            ExecutionAction::Wait {
                execution_id: ExecutionId::generate(),
                wait_timeout_ms: 100,
            },
        )
        .unwrap_err();
    assert_eq!(failure.kind, ToolFailureKind::Unavailable);
    match failure.effect {
        ToolFailureEffect::None {
            field,
            next_actions,
            ..
        } => {
            assert_eq!(field.as_deref(), Some("execution_id"));
            assert_eq!(next_actions.len(), 1);
            assert!(next_actions[0].contains("successful start"));
        }
        ToolFailureEffect::Unknown => panic!("missing Execution must prove no effect"),
    }

    drop(backend);
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires a live systemd user manager and AGL_TEST_HELPER"]
fn systemd_ordinary_execution_inherits_graphical_user_manager_environment() {
    let h = Harness::new();
    let manager =
        checked(Command::new(&h.backend.0.systemctl).args(["--user", "show-environment"])).unwrap();
    let variables = ["WAYLAND_DISPLAY", "DISPLAY", "XDG_RUNTIME_DIR"];
    let environment = String::from_utf8(manager.stdout).unwrap();
    let mut expected = Vec::new();
    for name in variables {
        let value = environment
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}=")))
            .unwrap_or("");
        expected.extend_from_slice(value.as_bytes());
        expected.push(0);
    }
    let mut request = h.request(
        "printf '%s\\000%s\\000%s\\000' \"$WAYLAND_DISPLAY\" \"$DISPLAY\" \"$XDG_RUNTIME_DIR\"",
    );
    request.clear_environment = false;
    request.max_output_bytes = 4096;
    let started = h.backend.launch(None, request, None, None).unwrap();
    let done = h.wait(started.execution_id);
    assert_eq!(done.outcome, Some(ExecutionOutcome::Exit { code: 0 }));
    assert_eq!(
        h.backend
            .read(started.execution_id, ExecutionOutputStream::Stdout, 0, 4096)
            .unwrap()
            .data,
        expected
    );
}

#[test]
#[ignore = "requires a live systemd user manager and AGL_TEST_HELPER"]
fn systemd_execution_lifecycle() {
    let h = Harness::new();
    let request = h.request("printf 'A\\000B'; printf '\\377E' >&2; exit 7");
    let operation = AgentOperationKey {
        run_id: match request.owner {
            ExecutionOwner::Agent { run_id, .. } => run_id,
            _ => unreachable!(),
        },
        ordinal: NonZeroU32::MIN,
    };
    let status = h
        .backend
        .launch(Some(operation.clone()), request.clone(), None, None)
        .unwrap();
    let done = h.wait(status.execution_id);
    assert_eq!(done.state, ExecutionState::Exited);
    assert_eq!(done.outcome, Some(ExecutionOutcome::Exit { code: 7 }));
    assert_eq!(done.stopped_by, None);
    assert_eq!(
        h.backend
            .read(status.execution_id, ExecutionOutputStream::Stdout, 0, 100)
            .unwrap()
            .data,
        b"A\0B"
    );
    assert_eq!(
        h.backend
            .read(status.execution_id, ExecutionOutputStream::Stderr, 0, 100)
            .unwrap()
            .data,
        b"\xffE"
    );
    let replay = h
        .backend
        .launch(Some(operation.clone()), request, None, None)
        .unwrap();
    assert_eq!(replay.execution_id, status.execution_id);
    h.backend.cleanup(status.execution_id).unwrap();
    assert!(
        h.backend
            .inspect(status.execution_id)
            .unwrap()
            .outputs_removed
    );
    let replay = h
        .backend
        .launch(
            Some(operation),
            h.request("touch replay-must-not-run"),
            None,
            None,
        )
        .unwrap();
    assert_eq!(replay.execution_id, status.execution_id);
    assert!(!h.root.join("replay-must-not-run").exists());

    let cap = h
        .backend
        .launch(
            None,
            h.request("i=0; while [ $i -lt 4096 ]; do printf x; printf y >&2; i=$((i+1)); done"),
            None,
            None,
        )
        .unwrap();
    let done = h.wait(cap.execution_id);
    assert_eq!(done.outcome, Some(ExecutionOutcome::Exit { code: 0 }));
    assert_eq!(
        done.stdout.unwrap(),
        ExecutionStreamStatus {
            bytes: 64,
            truncated: true
        }
    );
    assert_eq!(
        done.stderr.unwrap(),
        ExecutionStreamStatus {
            bytes: 64,
            truncated: true
        }
    );

    let main = h
        .backend
        .launch(
            None,
            h.request("(trap '' TERM; exec sleep 60) & printf '%s' $!; exit 7"),
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        h.wait(main.execution_id).outcome,
        Some(ExecutionOutcome::Exit { code: 7 })
    );
    let child = String::from_utf8(
        h.backend
            .read(main.execution_id, ExecutionOutputStream::Stdout, 0, 100)
            .unwrap()
            .data,
    )
    .unwrap();
    assert!(!Path::new(&format!("/proc/{child}")).exists());

    let mut request = h.request("trap '' TERM; while :; do sleep 1; done");
    request.timeout_ms = 100;
    let timeout = h.backend.launch(None, request, None, None).unwrap();
    let done = h.wait(timeout.execution_id);
    assert_eq!(done.stopped_by, Some(ExecutionStopCause::Timeout));
    assert!(matches!(
        done.outcome,
        Some(ExecutionOutcome::Signal { .. })
    ));

    let stopped = h
        .backend
        .launch(
            None,
            h.request("trap '' TERM; while :; do sleep 1; done"),
            None,
            None,
        )
        .unwrap();
    h.backend
        .stop(stopped.execution_id, ExecutionStopCause::Stop)
        .unwrap();
    let done = h.wait(stopped.execution_id);
    assert_eq!(done.stopped_by, Some(ExecutionStopCause::Stop));

    let ordinary = h
        .backend
        .launch(None, h.request("while :; do sleep 1; done"), None, None)
        .unwrap();
    let mut protected = h.request("while :; do sleep 1; done");
    protected.owner = ExecutionOwner::Runtime {
        component: "test-protected".into(),
    };
    let protected = h.backend.launch(None, protected, None, None).unwrap();
    checked(Command::new(&h.backend.0.systemctl).args([
        "--user",
        "kill",
        "--kill-whom=main",
        "--signal=SIGKILL",
        &h.owner,
    ]))
    .unwrap();
    assert_eq!(h.wait(protected.execution_id).state, ExecutionState::Exited);
    assert_eq!(
        h.backend.inspect(ordinary.execution_id).unwrap().state,
        ExecutionState::Running
    );
    let mut reconnected = ExecutionBackend::new(
        &h.root.join("data"),
        &h.root.join("state"),
        h.backend.0.store.clone(),
        h.owner.clone(),
    )
    .unwrap();
    Arc::get_mut(&mut reconnected.0).unwrap().helper = h.backend.0.helper.clone();
    assert_eq!(
        reconnected.inspect(ordinary.execution_id).unwrap().state,
        ExecutionState::Running
    );
    checked(Command::new(&h.backend.0.systemctl).args([
        "--user",
        "kill",
        "--kill-whom=main",
        "--signal=SIGKILL",
        &h.backend.0.tmux_unit,
    ]))
    .unwrap();
    let done = h.wait(ordinary.execution_id);
    if done.state == ExecutionState::OutcomeUnknown {
        assert_eq!(done.outcome, None);
        assert_eq!(done.stopped_by, None);
    } else {
        assert_eq!(done.stopped_by, Some(ExecutionStopCause::TmuxLost));
    }
}
#[test]
#[ignore = "requires a live systemd user manager and AGL_TEST_HELPER"]
fn systemd_terminal_literal_input_and_capture() {
    let h = Harness::new();
    let mut request =
        h.request("stty raw -echo; printf EARLY; dd bs=1 count=5 2>/dev/null; printf END; sleep 1");
    request.io = ExecutionIo::Pty;
    request.max_output_bytes = 4096;
    request.terminal_size = Some(TerminalSize::default());
    let started = h.backend.launch(None, request, None, None).unwrap();
    let ready = Instant::now() + Duration::from_secs(5);
    loop {
        let output = h
            .backend
            .read(started.execution_id, ExecutionOutputStream::Pty, 0, 4096)
            .unwrap();
        if output.data.windows(5).any(|w| w == b"EARLY") {
            break;
        }
        assert!(Instant::now() < ready);
        std::thread::sleep(Duration::from_millis(5));
    }
    h.backend
        .terminal_write(started.execution_id, b"a\n\x03bc")
        .unwrap();
    let done = h.wait(started.execution_id);
    assert_eq!(done.outcome, Some(ExecutionOutcome::Exit { code: 0 }));
    let output = h
        .backend
        .read(started.execution_id, ExecutionOutputStream::Pty, 0, 1024)
        .unwrap();
    assert!(output.data.windows(5).any(|window| window == b"a\n\x03bc"));
    assert!(output.data.windows(5).any(|window| window == b"EARLY"));
}
#[test]
fn reservation_survives_restart_and_cancel_blocks_new_launch() {
    let root = std::env::temp_dir().join(format!("agl-reservation-{}", uuid::Uuid::now_v7()));
    let store = StoreHandle::open_at(&root).unwrap();
    let run_id = agl_core::AgentRunId::generate();
    let operation = AgentOperationKey {
        run_id,
        ordinal: NonZeroU32::MIN,
    };
    let request = ExecutionStartRequest {
        owner: ExecutionOwner::Agent {
            run_id,
            conversation_id: agl_core::ConversationId::generate(),
        },
        argv: vec!["/bin/true".into()],
        cwd: "/tmp".into(),
        environment: Default::default(),
        clear_environment: true,
        io: ExecutionIo::Pipes,
        timeout_ms: 0,
        max_output_bytes: 64,
        terminal_size: None,
        isolation: Default::default(),
    };
    let status = ExecutionStatus {
        execution_id: ExecutionId::generate(),
        state: ExecutionState::Running,
        outcome: None,
        stopped_by: None,
        stdout: Some(Default::default()),
        stderr: Some(Default::default()),
        terminal: None,
        outputs_removed: false,
    };
    assert_eq!(
        store
            .reserve_execution(Some(operation.clone()), &request, &status)
            .unwrap(),
        (status.execution_id, true)
    );
    drop(store);
    let store = StoreHandle::open_at(&root).unwrap();
    assert_eq!(
        store
            .reserve_execution(Some(operation), &request, &status)
            .unwrap(),
        (status.execution_id, false)
    );
    store.cancel_execution_run(run_id).unwrap();
    assert!(
        store
            .reserve_execution(
                Some(AgentOperationKey {
                    run_id,
                    ordinal: NonZeroU32::new(2).unwrap()
                }),
                &request,
                &status
            )
            .is_err()
    );
    drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn final_execution_status_and_cleanup_cannot_regress() {
    let root = std::env::temp_dir().join(format!("agl-monotonic-{}", uuid::Uuid::now_v7()));
    let store = StoreHandle::open_at(&root).unwrap();
    let request = ExecutionStartRequest {
        owner: ExecutionOwner::Runtime {
            component: "test".into(),
        },
        argv: vec!["/bin/true".into()],
        cwd: "/tmp".into(),
        environment: Default::default(),
        clear_environment: true,
        io: ExecutionIo::Pipes,
        timeout_ms: 0,
        max_output_bytes: 64,
        terminal_size: None,
        isolation: Default::default(),
    };
    let running = ExecutionStatus {
        execution_id: ExecutionId::generate(),
        state: ExecutionState::Running,
        outcome: None,
        stopped_by: None,
        stdout: Some(Default::default()),
        stderr: Some(Default::default()),
        terminal: None,
        outputs_removed: false,
    };
    store.reserve_execution(None, &request, &running).unwrap();
    let mut done = running.clone();
    done.state = ExecutionState::Exited;
    done.outcome = Some(ExecutionOutcome::Exit { code: 7 });
    let observe = |status: &ExecutionStatus| ExecutionInput::Observe {
        unit_live: true,
        completion: Some(status.clone()),
        streams: Streams::from(status),
        stop_cause: None,
    };
    store
        .apply_execution_input(done.execution_id, observe(&done))
        .unwrap();
    assert_eq!(
        store
            .apply_execution_input(running.execution_id, observe(&running))
            .unwrap(),
        done
    );
    store
        .apply_execution_input(done.execution_id, ExecutionInput::OutputsRemoved)
        .unwrap();
    done.outputs_removed = true;
    assert_eq!(
        store
            .apply_execution_input(running.execution_id, observe(&running))
            .unwrap(),
        done
    );
    drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires a live systemd user manager and AGL_TEST_HELPER"]
fn systemd_failed_payload_exec_is_unknown() {
    let h = Harness::new();
    let mut request = h.request("touch must-not-run");
    request.argv[0] = h
        .root
        .join("missing-program")
        .to_string_lossy()
        .into_owned();
    let launched = h.backend.launch(None, request, None, None).unwrap();
    let done = h.wait(launched.execution_id);
    assert_eq!(done.state, ExecutionState::OutcomeUnknown);
    assert_eq!(done.outcome, None);
    assert!(!h.root.join("must-not-run").exists());
}

fn git_request(h: &Harness, script: &str) -> agl_tool_api::ServiceGitRequest {
    agl_tool_api::ServiceGitRequest {
        args: vec![
            "-c".into(),
            format!("alias.probe=!{script}").into(),
            "probe".into(),
        ],
        cwd: h.root.clone(),
        env: vec![("PATH".into(), std::env::var_os("PATH").unwrap())],
        stdin: Vec::new(),
        timeout: Duration::from_secs(10),
        deadline_at_ms: now_ms() + 10000,
        cancellation: agl_tool_api::ToolCancellation::new(),
    }
}
#[test]
#[ignore = "requires a live systemd user manager and AGL_TEST_HELPER"]
fn systemd_service_git_deadline_cancel_and_cgroup_limits() {
    let h = Harness::new();
    let config = ServiceGitConfig {
        memory_max_bytes: 128 * 1024 * 1024,
        max_output_bytes: 7,
    };
    let request = git_request(&h, "printf 123456789; printf abcdefghi >&2; exit 3");
    let output = h.backend.run_git(request, config).unwrap();
    assert_eq!(output.exit_code, Some(3));
    assert_eq!(output.stdout, b"1234567");
    assert!(output.stdout_truncated);
    assert_eq!(output.stderr, b"abcdefg");
    assert!(output.stderr_truncated);

    let mut expired = git_request(&h, "touch expired-must-not-run");
    expired.deadline_at_ms = now_ms() - 1;
    assert!(h.backend.run_git(expired, config).is_err());
    assert!(!h.root.join("expired-must-not-run").exists());

    let mut deadline = git_request(
        &h,
        "trap '' TERM; echo ready > deadline-ready; while :; do sleep 1; done",
    );
    deadline.deadline_at_ms = now_ms() + 1000;
    let before = Instant::now();
    let output = h.backend.run_git(deadline, config).unwrap();
    assert!(h.root.join("deadline-ready").exists());
    assert!(output.timed_out);
    assert_eq!(output.exit_code, None);
    assert!(before.elapsed() >= Duration::from_secs(2));
    assert!(before.elapsed() < Duration::from_secs(5));

    let request = git_request(
        &h,
        "trap '' TERM; cat /proc/$$/cgroup > limit-cgroup; echo ready > cancel-ready; while :; do sleep 1; done",
    );
    let cancel = request.cancellation.clone();
    let marker = h.root.join("cancel-ready");
    let cgroup = h.root.join("limit-cgroup");
    let systemctl = h.backend.0.systemctl.clone();
    let observer = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(5);
        while !marker.exists() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(5));
        }
        let group = fs::read_to_string(cgroup).unwrap();
        let unit = group.trim().rsplit('/').next().unwrap();
        let actual = checked(Command::new(systemctl).args([
            "--user",
            "show",
            unit,
            "-p",
            "MemoryMax",
            "--value",
        ]))
        .unwrap();
        assert_eq!(String::from_utf8_lossy(&actual.stdout).trim(), "134217728");
        cancel.cancel();
    });
    let output = h.backend.run_git(request, config).unwrap();
    observer.join().unwrap();
    assert!(output.cancelled);
    assert_eq!(output.exit_code, None);
}

#[test]
#[ignore = "requires a live systemd user manager and AGL_TEST_HELPER"]
fn systemd_service_git_cancels_during_helper_startup() {
    let mut h = Harness::new();
    let helper = h.root.join("delayed-helper");
    let marker = h.root.join("startup-entered");
    fs::write(
        &helper,
        format!(
            "#!/bin/sh\nif [ \"$2\" = launch ]; then\n: > {}\n{} 30\nfi\nexec {} \"$@\"\n",
            shell_quote(&marker.to_string_lossy()),
            shell_quote(&program("sleep").unwrap().to_string_lossy()),
            shell_quote(&h.backend.0.helper.to_string_lossy()),
        ),
    )
    .unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    Arc::get_mut(&mut h.backend.0).unwrap().helper = helper;
    let request = git_request(&h, "touch cancelled-payload-must-not-run");
    let cancel = request.cancellation.clone();
    let observer = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(5);
        while !marker.exists() {
            assert!(Instant::now() < until, "helper did not start");
            std::thread::sleep(Duration::from_millis(5));
        }
        cancel.cancel();
    });
    let before = Instant::now();
    let output = h
        .backend
        .run_git(request, ServiceGitConfig::default())
        .unwrap();
    observer.join().unwrap();
    assert!(output.cancelled);
    assert_eq!(output.exit_code, None);
    assert!(before.elapsed() < Duration::from_secs(5));
    assert!(!h.root.join("cancelled-payload-must-not-run").exists());

    let mut request = git_request(&h, "touch deadline-payload-must-not-run");
    request.deadline_at_ms = now_ms() + 500;
    let before = Instant::now();
    let output = h
        .backend
        .run_git(request, ServiceGitConfig::default())
        .unwrap();
    assert!(output.timed_out);
    assert_eq!(output.exit_code, None);
    assert!(before.elapsed() < Duration::from_secs(5));
    assert!(!h.root.join("deadline-payload-must-not-run").exists());
}

#[test]
#[ignore = "requires a live systemd user manager and AGL_TEST_HELPER"]
fn systemd_session_loss_stops_payload_without_daemon_observation() {
    let h = Harness::new();
    let mut request =
        h.request("trap '' HUP; echo $$ > session-loss-pid; while :; do sleep 1; done");
    request.timeout_ms = 10000;
    let start = h.backend.launch(None, request, None, None).unwrap();
    let marker = h.root.join("session-loss-pid");
    let until = Instant::now() + Duration::from_secs(3);
    while !marker.exists() {
        assert!(Instant::now() < until);
        std::thread::sleep(Duration::from_millis(5));
    }
    let pid: i32 = fs::read_to_string(marker).unwrap().trim().parse().unwrap();
    h.backend
        .tmux(&["kill-session", "-t", &session(start.execution_id)])
        .unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(Instant::now() < until, "payload survived its lost session");
        std::thread::sleep(Duration::from_millis(5));
    }
    let done = h.wait(start.execution_id);
    assert_eq!(done.state, ExecutionState::OutcomeUnknown);
    assert_eq!(done.outcome, None);
}

#[test]
#[ignore = "requires a live systemd user manager and AGL_TEST_HELPER"]
fn systemd_tmux_loss_stops_protected_payload_with_daemon_still_alive() {
    let h = Harness::new();
    let mut request = h.request("echo $$ > protected-tmux-loss-pid; while :; do sleep 1; done");
    request.owner = ExecutionOwner::Runtime {
        component: "service-test".into(),
    };
    let start = h.backend.launch(None, request, None, None).unwrap();
    checked(Command::new(&h.backend.0.systemctl).args([
        "--user",
        "kill",
        "--kill-whom=main",
        "--signal=SIGKILL",
        &h.backend.0.tmux_unit,
    ]))
    .unwrap();
    assert_ne!(h.wait(start.execution_id).state, ExecutionState::Running);
    assert!(h.backend.unit_live(&h.owner).unwrap());
    assert!(!h.backend.unit_live(&unit(start.execution_id)).unwrap());
}

#[test]
#[ignore = "requires a live systemd user manager and AGL_TEST_HELPER"]
fn systemd_terminal_transfers_a_full_raw_write() {
    let h = Harness::new();
    let script = r#"
import hashlib, os, tty
tty.setraw(0)
os.write(1, b'READY\n')
data = bytearray()
while len(data) < 65536:
    part = os.read(0, 65536 - len(data))
    assert part
    data.extend(part)
os.write(1, hashlib.sha256(data).hexdigest().encode() + b'\n')
"#;
    let mut request = h.request("");
    request.argv = vec![
        program("python3").unwrap().to_string_lossy().into_owned(),
        "-B".into(),
        "-c".into(),
        script.into(),
    ];
    request.io = ExecutionIo::Pty;
    request.timeout_ms = 10000;
    request.max_output_bytes = 128;
    request.terminal_size = Some(TerminalSize::default());
    let start = h.backend.launch(None, request, None, None).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let output = h
            .backend
            .read(start.execution_id, ExecutionOutputStream::Pty, 0, 128)
            .unwrap();
        if output.data.starts_with(b"READY\n") {
            break;
        }
        assert!(Instant::now() < until, "terminal did not enter raw mode");
        std::thread::sleep(Duration::from_millis(5));
    }
    let data: Vec<u8> = (0..65536).map(|index| index as u8).collect();
    h.backend.terminal_write(start.execution_id, &data).unwrap();
    assert_eq!(
        h.wait(start.execution_id).outcome,
        Some(ExecutionOutcome::Exit { code: 0 })
    );
    let output = h
        .backend
        .read(start.execution_id, ExecutionOutputStream::Pty, 0, 128)
        .unwrap();
    let digest = Sha256::digest(&data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert_eq!(output.data, format!("READY\n{digest}\n").as_bytes());
    assert!(!output.truncated);
}

#[test]
#[ignore = "requires a live systemd user manager and AGL_TEST_HELPER"]
fn systemd_terminal_has_controlling_tty_and_resize() {
    let h = Harness::new();
    let mut request = h.request("test -t 0 && test -t 1 && test -t 2 || exit 9; test -r /dev/tty || exit 10; IFS= read -r line; stty size; printf 'got:%s\\n' \"$line\"");
    request.io = ExecutionIo::Pty;
    request.terminal_size = Some(TerminalSize::default());
    request.max_output_bytes = 4096;
    let started = h.backend.launch(None, request, None, None).unwrap();
    h.backend
        .tmux(&[
            "resize-window",
            "-t",
            &session(started.execution_id),
            "-x",
            "100",
            "-y",
            "30",
        ])
        .unwrap();
    h.backend
        .terminal_write(started.execution_id, "привіт\n".as_bytes())
        .unwrap();
    assert_eq!(
        h.wait(started.execution_id).outcome,
        Some(ExecutionOutcome::Exit { code: 0 })
    );
    let output = h
        .backend
        .read(started.execution_id, ExecutionOutputStream::Pty, 0, 4096)
        .unwrap();
    let text = String::from_utf8_lossy(&output.data);
    assert!(text.contains("30 100"), "{text:?}");
    assert!(text.contains("got:привіт"), "{text:?}");
}
#[test]
#[ignore = "requires a live systemd user manager and AGL_TEST_HELPER"]
fn systemd_private_inference_preserves_fd_and_sandbox_boundary() {
    let h = Harness::new();
    let bin = h.root.join("bin");
    let private = h.root.join("private");
    fs::create_dir(&bin).unwrap();
    fs::create_dir(&private).unwrap();
    fs::write(h.root.join("model"), b"model").unwrap();
    fs::write(h.root.join("secret"), b"secret").unwrap();
    let source = bin.join("probe.c");
    let exe = bin.join("probe");
    fs::write(
        &source,
        r#"
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <unistd.h>
int main(int argc, char **argv) {
    char model[5]; assert(pread(200, model, 5, 0) == 5);
    assert(model[0] == 'm'); assert(atoi(getenv("AGL_LLAMA_SERVER_LISTEN_FD")) == 190);
    int accepting = 0; socklen_t len = sizeof(accepting);
    assert(getsockopt(190, SOL_SOCKET, SO_ACCEPTCONN, &accepting, &len) == 0 && accepting);
    assert(socket(AF_INET, SOCK_STREAM, 0) == -1 && errno == EPERM);
    assert(socket(AF_UNIX, SOCK_STREAM, 0) == -1 && errno == EPERM);
    assert(open(argv[1], O_RDONLY) == -1 && errno == EACCES);
    assert(open(argv[2], O_WRONLY) == -1 && errno == EACCES);
    int out = open("allowed", O_CREAT | O_WRONLY, 0600); assert(out >= 0); close(out);
    return 0;
}
"#,
    )
    .unwrap();
    checked(
        Command::new(program("cc").unwrap())
            .arg(&source)
            .arg("-o")
            .arg(&exe),
    )
    .unwrap();
    let mut request = h.request("");
    request.owner = ExecutionOwner::Runtime {
        component: "inference-test".into(),
    };
    request.cwd = private.to_string_lossy().into_owned();
    request.argv = vec![
        exe.to_string_lossy().into_owned(),
        h.root.join("secret").to_string_lossy().into_owned(),
        h.root.join("model").to_string_lossy().into_owned(),
    ];
    let model = File::open(h.root.join("model")).unwrap();
    use std::os::fd::AsRawFd;
    let pinned = format!("/proc/{}/fd/{}", std::process::id(), model.as_raw_fd());
    let isolation = PrivateInference {
        artifacts: vec![pinned],
        listen_socket: private.join("private.sock").to_string_lossy().into_owned(),
        device_paths: Vec::new(),
        address_space_limit_bytes: 512 * 1024 * 1024,
    };
    let started = h
        .backend
        .launch(None, request, Some(isolation), None)
        .unwrap();
    let done = h.wait(started.execution_id);
    let stderr = h
        .backend
        .read(started.execution_id, ExecutionOutputStream::Stderr, 0, 4096)
        .unwrap();
    assert_eq!(
        done.outcome,
        Some(ExecutionOutcome::Exit { code: 0 }),
        "{stderr:?}"
    );
    assert!(private.join("allowed").exists());
}

#[test]
#[ignore = "requires a live systemd user manager and AGL_TEST_HELPER; run in scripts/agl-nix-vulkan.sh"]
fn systemd_planner_keeps_workspace_readonly_and_host_ipc_hidden() {
    let h = Harness::new();
    let workspace = h.root.join("workspace");
    fs::create_dir_all(workspace.join("target")).unwrap();
    fs::write(workspace.join("input"), "original").unwrap();
    fs::write(workspace.join("agentLIBRE.toml"), "manifest").unwrap();
    fs::write(workspace.join("target/cache"), "host-cache").unwrap();
    fs::write(h.root.join("secret"), "secret").unwrap();
    let script = r#"
import errno, os, pathlib, socket, sys
root = pathlib.Path.cwd()
assert (root / 'input').read_text() == 'original'
assert (root / 'agentLIBRE.toml').read_text() == 'manifest'
assert not (root / 'target').exists()
assert not pathlib.Path(sys.argv[1]).exists()
assert not pathlib.Path('/run/user').exists()
for operation in [lambda: (root / 'input').write_text('bad'), lambda: (root / 'new').write_text('bad'),
                  lambda: os.chmod(root / 'input', 0o777), lambda: os.utime(root / 'input', None)]:
    try: operation()
    except OSError: pass
    else: raise AssertionError('workspace write allowed')
for family in [socket.AF_INET, socket.AF_INET6, socket.AF_UNIX]:
    try: socket.socket(family)
    except PermissionError: pass
    else: raise AssertionError('socket allowed')
scratch = pathlib.Path(os.environ['TMPDIR'])
(scratch / 'allowed').write_text('allowed')
cache = pathlib.Path(os.environ['XDG_CACHE_HOME'])
marker = cache / 'persistent-proof'
assert os.environ['CARGO_NET_OFFLINE'] == 'true'
for descriptor in [187, 188]:
    try: os.fstat(descriptor)
    except OSError as error: assert error.errno == errno.EBADF
    else: raise AssertionError('host preparation descriptor leaked')
cargo = pathlib.Path(os.environ['CARGO_HOME'])
assert cargo == cache / 'cargo'
assert not (cargo / 'credentials').exists() and not (cargo / 'credentials.toml').exists()
assert not (cargo / 'config').exists() and not (cargo / 'config.toml').exists()
for name in sys.argv[2:]:
    try: (cargo / name / 'agl-readonly-proof').write_text('bad')
    except OSError as error: assert error.errno == errno.EROFS
    else: raise AssertionError('Cargo dependency source is writable')
for operation in [lambda: os.mkfifo(cache / 'fifo'), lambda: os.mknod(cache / 'node', 0o10600)]:
    try: operation()
    except PermissionError: pass
    else: raise AssertionError('special cache entry allowed')
if marker.exists(): assert marker.read_text() == 'persistent'
else: marker.write_text('persistent')
print(scratch)
"#;
    let python = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join("python3"))
        .find_map(|p| {
            p.canonicalize()
                .ok()
                .filter(|p| p.starts_with("/nix/store") && p.is_file())
        })
        .expect("Nix python3 required");
    let mut request = h.request("");
    request.cwd = workspace.to_string_lossy().into_owned();
    request.argv = vec![
        python.to_string_lossy().into_owned(),
        "-B".into(),
        "-c".into(),
        script.into(),
        h.root.join("secret").to_string_lossy().into_owned(),
    ];
    let cache =
        planner_cache::PlannerCache::prepare(h.backend.0.root.parent().unwrap(), &workspace)
            .unwrap();
    for (source, name) in [(&cache.registry, "registry"), (&cache.git, "git")] {
        if source.is_some() {
            request.argv.push(name.into());
        }
    }
    request.timeout_ms = 10000;
    request.max_output_bytes = 8192;
    request.isolation = ExecutionIsolation::ReadOnlyWorkspace {
        workspace_root: request.cwd.clone(),
        output_directories: vec!["target".into()],
    };
    let replay_request = request.clone();
    let start = h.backend.launch(None, request, None, None).unwrap();
    let done = h.wait(start.execution_id);
    let stderr = h
        .backend
        .read(start.execution_id, ExecutionOutputStream::Stderr, 0, 8192)
        .unwrap();
    assert_eq!(
        done.outcome,
        Some(ExecutionOutcome::Exit { code: 0 }),
        "{stderr:?}"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("input")).unwrap(),
        "original"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("target/cache")).unwrap(),
        "host-cache"
    );
    let output = h
        .backend
        .read(start.execution_id, ExecutionOutputStream::Stdout, 0, 8192)
        .unwrap();
    let scratch = PathBuf::from(String::from_utf8(output.data).unwrap().trim());
    assert!(scratch.starts_with(h.backend.0.root.join("planner-scratch")));
    assert!(!scratch.exists());
    let cache =
        planner_cache::PlannerCache::prepare(h.backend.0.root.parent().unwrap(), &workspace)
            .unwrap();
    assert_eq!(
        fs::read_to_string(cache.path.join("persistent-proof")).unwrap(),
        "persistent"
    );
    let next = h.backend.launch(None, replay_request, None, None).unwrap();
    assert_eq!(
        h.wait(next.execution_id).outcome,
        Some(ExecutionOutcome::Exit { code: 0 })
    );
    assert_eq!(
        fs::read_to_string(cache.path.join("persistent-proof")).unwrap(),
        "persistent"
    );
}

#[test]
#[ignore = "requires a live systemd user manager and AGL_TEST_HELPER"]
fn systemd_conversation_access_and_original_run_cancellation() {
    use agl_core::agent::{AbsolutePath, AuthorityGrantSet, RelativePath, WorkspaceScope};
    use agl_tool_api::{ToolCancellation, ToolContext};
    let h = Harness::new();
    let request = h.request("while :; do sleep 1; done");
    let ExecutionOwner::Agent {
        conversation_id,
        run_id,
    } = request.owner
    else {
        unreachable!()
    };
    let started = h.backend.launch(None, request, None, None).unwrap();
    let context = |conversation, run| {
        ToolContext::new(
            AgentOperationKey {
                run_id: run,
                ordinal: NonZeroU32::MIN,
            },
            Some(conversation),
            WorkspaceScope {
                root: AbsolutePath::try_from(h.root.to_string_lossy().into_owned()).unwrap(),
                working_directory: RelativePath::try_from(".".to_owned()).unwrap(),
            },
            AuthorityGrantSet(Vec::new()),
            now_ms() + 10000,
            65536,
            ToolCancellation::new(),
        )
    };
    let next_run = agl_core::AgentRunId::generate();
    let wrong = h
        .backend
        .tool_action(
            context(agl_core::ConversationId::generate(), next_run),
            ExecutionAction::Stop(started.execution_id),
        )
        .unwrap_err();
    assert_eq!(wrong.kind, ToolFailureKind::Unauthorized);
    let same = h
        .backend
        .tool_action(
            context(conversation_id, next_run),
            ExecutionAction::Inspect(started.execution_id),
        )
        .unwrap();
    assert!(matches!(
        same,
        ExecutionResult::Status(ExecutionStatus {
            state: ExecutionState::Running,
            ..
        })
    ));
    h.backend.cancel_run(next_run).unwrap();
    assert_eq!(
        h.backend.inspect(started.execution_id).unwrap().state,
        ExecutionState::Running
    );
    h.backend.cancel_run(run_id).unwrap();
    assert_eq!(
        h.wait(started.execution_id).stopped_by,
        Some(ExecutionStopCause::RunCancel)
    );
    let (_, saved, _) = h.backend.0.store.execution(started.execution_id).unwrap();
    assert_eq!(saved.state, ExecutionState::Exited);
}

#[test]
fn failed_completion_proof_stays_unknown_even_when_a_late_success_arrives() {
    use agl_core::Fsm;
    let initial = ExecutionFsm::initial(ExecutionId::generate(), ExecutionIo::Pipes);
    let valid = Completion {
        main: MainOutcome {
            outcome: Some(ExecutionOutcome::Exit { code: 0 }),
            result: "success".into(),
        },
        streams: Streams::from(&initial),
        started: true,
        launch_failed: false,
        timeout_configured: false,
        stop_cause: None,
        owner_lost: false,
    };
    let succeeded = ExecutionFsm
        .transition(&initial, ExecutionInput::Complete(valid.clone()))
        .unwrap()
        .state;
    assert_eq!(succeeded.state, ExecutionState::Exited);
    for missing in 0..3 {
        let mut proof = valid.clone();
        match missing {
            0 => proof.started = false,
            1 => proof.launch_failed = true,
            _ => proof.streams.stderr = None,
        }
        let unknown = ExecutionFsm
            .transition(&initial, ExecutionInput::Complete(proof))
            .unwrap()
            .state;
        assert_eq!(unknown.state, ExecutionState::OutcomeUnknown);
        assert_eq!(unknown.outcome, None);
        assert_eq!(
            ExecutionFsm
                .transition(&unknown, ExecutionInput::Complete(valid.clone()))
                .unwrap()
                .state,
            unknown
        );
    }
    let recovered = ExecutionFsm
        .transition(
            &initial,
            ExecutionInput::Observe {
                unit_live: false,
                completion: None,
                streams: Streams::from(&initial),
                stop_cause: None,
            },
        )
        .unwrap()
        .state;
    assert_eq!(recovered.state, ExecutionState::OutcomeUnknown);
    assert!(
        ExecutionFsm
            .transition(&initial, ExecutionInput::OutputsRemoved)
            .is_err()
    );
}

#[test]
#[ignore = "requires a live systemd user manager and AGL_TEST_HELPER"]
fn systemd_forge_git_streams_data_fds_without_log_truncation() {
    use std::io::{Read, Seek, Write};
    let h = Harness::new();
    let input_path = h.root.join("raw-input");
    let output_path = h.root.join("raw-output");
    let bytes = vec![0xa5; 3 * 64 * 1024 + 7];
    let mut input = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&input_path)
        .unwrap();
    input.write_all(&bytes).unwrap();
    input.rewind().unwrap();
    let output = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&output_path)
        .unwrap();
    let result = h
        .backend
        .run_forge_git(
            ayeque_forge_core::GitCommand {
                cwd: h.root.clone(),
                args: ["-c", "alias.probe=!cat; printf noisy-stderr >&2", "probe"]
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                stdin: Some(input),
                stdout: Some(output),
                deadline: Instant::now() + Duration::from_secs(10),
            },
            ServiceGitConfig {
                memory_max_bytes: 1073741824,
                max_output_bytes: 7,
            },
        )
        .unwrap();
    assert!(result.status.success());
    assert!(result.stdout.is_empty());
    assert_eq!(result.stderr, b"noisy-s");
    let mut actual = Vec::new();
    File::open(output_path)
        .unwrap()
        .read_to_end(&mut actual)
        .unwrap();
    assert_eq!(actual, bytes);
}
