use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agl_execd::{ExecutionServer, ExecutionService, ListenerSource};
use agl_execution_api::{
    ExecutionClient, ExecutionIo, ExecutionIsolation, ExecutionOutcome, ExecutionOutputStream,
    ExecutionOwner, ExecutionSignal, ExecutionStartRequest, ExecutionState, TerminalSize,
};

fn root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("agl-execd-{name}-{}", uuid::Uuid::now_v7()))
}

fn program(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join(name))
        .find(|path| path.is_absolute() && path.is_file())
        .unwrap_or_else(|| panic!("missing test executable: {name}"))
}

async fn start_server(root: &Path) -> (ExecutionClient, tokio::task::JoinHandle<()>) {
    let socket = root.join("execd.sock");
    let launcher = PathBuf::from(env!("CARGO_BIN_EXE_agl-execd-launcher"));
    let server = ExecutionServer::start(&root.join("data"), launcher).unwrap();
    let task = tokio::spawn({
        let socket = socket.clone();
        async move {
            let _ = server.serve(ListenerSource::Bind(socket)).await;
        }
    });
    for _ in 0..100 {
        if socket.exists() {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            let socket_metadata = std::fs::symlink_metadata(&socket).unwrap();
            let parent_metadata = std::fs::metadata(socket.parent().unwrap()).unwrap();
            assert_eq!(socket_metadata.permissions().mode() & 0o777, 0o600);
            assert_eq!(parent_metadata.permissions().mode() & 0o777, 0o700);
            assert_eq!(socket_metadata.uid(), unsafe { libc::geteuid() });
            return (ExecutionClient::new(socket), task);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("execd socket was not created");
}

#[test]
fn launcher_identity_rejects_symlinks_and_writable_files() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let root = root("launcher-identity");
    std::fs::create_dir_all(&root).unwrap();
    let real = PathBuf::from(env!("CARGO_BIN_EXE_agl-execd-launcher"));
    let linked = root.join("linked-launcher");
    symlink(&real, &linked).unwrap();
    assert!(ExecutionService::start(&root.join("linked-data"), linked).is_err());

    let writable = root.join("writable-launcher");
    std::fs::copy(real, &writable).unwrap();
    std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(ExecutionService::start(&root.join("writable-data"), writable).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

fn request(argv: Vec<String>, io: ExecutionIo) -> ExecutionStartRequest {
    ExecutionStartRequest {
        owner: ExecutionOwner::Runtime {
            component: "integration-test".to_owned(),
        },
        argv,
        cwd: "/tmp".to_owned(),
        environment: BTreeMap::new(),
        clear_environment: false,
        io,
        timeout_ms: 5_000,
        max_output_bytes: 64 * 1024,
        terminal_size: (io == ExecutionIo::Pty).then_some(TerminalSize {
            rows: 24,
            columns: 80,
        }),
        isolation: agl_execution_api::ExecutionIsolation::Standard,
    }
}

async fn wait(client: &ExecutionClient, id: agl_execution_api::ExecutionId) {
    for _ in 0..500 {
        if client.inspect(id).await.unwrap().state != ExecutionState::Running {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("execution did not finish");
}

#[tokio::test]
async fn exact_argv_output_survives_client_connections() {
    let root = root("pipes");
    let (client, server) = start_server(&root).await;
    let status = client
        .start(request(
            vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "printf EXECD_OK".to_owned(),
            ],
            ExecutionIo::Pipes,
        ))
        .await
        .unwrap();
    wait(&client, status.execution_id).await;
    let output = client.read(status.execution_id, 0, 1024).await.unwrap();
    assert_eq!(output.chunks.len(), 1);
    assert_eq!(output.chunks[0].data, b"EXECD_OK");
    assert!(output.eof);
    server.abort();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn pty_accepts_input_and_resize() {
    let root = root("pty");
    let (client, server) = start_server(&root).await;
    let status = client
        .start(request(
            vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "test -t 0 && test -t 1 && test -t 2 || exit 9; IFS= read -r line; stty size; printf 'got:%s\\n' \"$line\"".to_owned(),
            ],
            ExecutionIo::Pty,
        ))
        .await
        .unwrap();
    let terminal = status.terminal_id.unwrap();
    client
        .resize(
            terminal,
            TerminalSize {
                rows: 30,
                columns: 100,
            },
        )
        .await
        .unwrap();
    client.write(terminal, b"hello\n".to_vec()).await.unwrap();
    wait(&client, status.execution_id).await;
    let output = client
        .read(status.execution_id, 0, 64 * 1024)
        .await
        .unwrap();
    let bytes = output
        .chunks
        .iter()
        .flat_map(|chunk| chunk.data.iter().copied())
        .collect::<Vec<_>>();
    let output = String::from_utf8_lossy(&bytes);
    assert!(output.contains("30 100"), "{output:?}");
    assert!(output.contains("got:hello"), "{output:?}");

    let eof = client
        .start(request(
            vec![
                program("sh").to_string_lossy().into_owned(),
                "-c".to_owned(),
                "cat; printf eof".to_owned(),
            ],
            ExecutionIo::Pty,
        ))
        .await
        .unwrap();
    let eof_terminal = eof.terminal_id.unwrap();
    client
        .write(eof_terminal, b"hi\x04\x04".to_vec())
        .await
        .unwrap();
    wait(&client, eof.execution_id).await;
    let output = client.read(eof.execution_id, 0, 4096).await.unwrap();
    let bytes = output
        .chunks
        .iter()
        .flat_map(|chunk| chunk.data.iter().copied())
        .collect::<Vec<_>>();
    assert!(String::from_utf8_lossy(&bytes).contains("eof"));
    server.abort();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn private_runtime_service_is_supervised_until_termination() {
    let root = root("runtime-service");
    std::fs::create_dir_all(&root).unwrap();
    let artifact = root.join("model.bin");
    std::fs::write(&artifact, b"model").unwrap();
    let socket = root.join("private.sock");
    let (client, server) = start_server(&root).await;
    let sleep = program("sleep");
    let status = client
        .start(ExecutionStartRequest {
            owner: ExecutionOwner::Runtime {
                component: "inference-test".to_owned(),
            },
            argv: vec![sleep.to_string_lossy().into_owned(), "30".to_owned()],
            cwd: root.to_string_lossy().into_owned(),
            environment: BTreeMap::new(),
            clear_environment: false,
            io: ExecutionIo::Pipes,
            timeout_ms: 0,
            max_output_bytes: 64 * 1024,
            terminal_size: None,
            isolation: ExecutionIsolation::PrivateInference {
                artifacts: vec![artifact.to_string_lossy().into_owned()],
                listen_socket: socket.to_string_lossy().into_owned(),
                device_paths: vec![],
                address_space_limit_bytes: 512 * 1024 * 1024,
            },
        })
        .await
        .unwrap();
    assert_eq!(status.state, ExecutionState::Running);
    assert!(socket.exists());
    client
        .signal(status.execution_id, ExecutionSignal::Terminate)
        .await
        .unwrap();
    wait(&client, status.execution_id).await;
    server.abort();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn pipes_preserve_cwd_exact_arguments_and_output_streams() {
    let root = root("argv-cwd-streams");
    std::fs::create_dir_all(&root).unwrap();
    let (client, server) = start_server(&root).await;
    let argument = "space ; $HOME *";
    let mut start = request(
        vec![
            program("sh").to_string_lossy().into_owned(),
            "-c".to_owned(),
            "printf '%s|%s' \"$PWD\" \"$1\"; printf ERR >&2".to_owned(),
            "sh".to_owned(),
            argument.to_owned(),
        ],
        ExecutionIo::Pipes,
    );
    start.cwd = root.to_string_lossy().into_owned();
    let status = client.start(start).await.unwrap();
    wait(&client, status.execution_id).await;
    let output = client.read(status.execution_id, 0, 4096).await.unwrap();
    let stdout = output
        .chunks
        .iter()
        .filter(|chunk| chunk.stream == ExecutionOutputStream::Stdout)
        .flat_map(|chunk| chunk.data.iter().copied())
        .collect::<Vec<_>>();
    let stderr = output
        .chunks
        .iter()
        .filter(|chunk| chunk.stream == ExecutionOutputStream::Stderr)
        .flat_map(|chunk| chunk.data.iter().copied())
        .collect::<Vec<_>>();
    assert_eq!(
        String::from_utf8(stdout).unwrap(),
        format!("{}|{argument}", root.display())
    );
    assert_eq!(stderr, b"ERR");
    server.abort();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn output_limit_and_timeout_have_explicit_outcomes() {
    let root = root("bounds-timeout");
    let (client, server) = start_server(&root).await;
    let mut bounded = request(
        vec![
            program("sh").to_string_lossy().into_owned(),
            "-c".to_owned(),
            "printf 123456789".to_owned(),
        ],
        ExecutionIo::Pipes,
    );
    bounded.max_output_bytes = 5;
    let bounded = client.start(bounded).await.unwrap();
    wait(&client, bounded.execution_id).await;
    let status = client.inspect(bounded.execution_id).await.unwrap();
    let output = client.read(bounded.execution_id, 0, 64).await.unwrap();
    assert!(status.output_truncated);
    assert_eq!(output.chunks[0].data, b"12345");

    let mut timed = request(
        vec![
            program("sleep").to_string_lossy().into_owned(),
            "30".to_owned(),
        ],
        ExecutionIo::Pipes,
    );
    timed.timeout_ms = 30;
    let timed = client.start(timed).await.unwrap();
    wait(&client, timed.execution_id).await;
    let timed_output = client.read(timed.execution_id, 0, 4096).await.unwrap();
    assert_eq!(
        client.inspect(timed.execution_id).await.unwrap().outcome,
        Some(ExecutionOutcome::TimedOut),
        "output={timed_output:?}"
    );
    server.abort();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn timeout_cleans_up_launcher_owned_descendants() {
    let root = root("timeout-descendant");
    std::fs::create_dir_all(&root).unwrap();
    let marker = root.join("escaped-timeout-child");
    let (client, server) = start_server(&root).await;
    let mut timed = request(
        vec![
            program("sh").to_string_lossy().into_owned(),
            "-c".to_owned(),
            "(sleep 0.4; printf escaped > escaped-timeout-child) & wait".to_owned(),
        ],
        ExecutionIo::Pipes,
    );
    timed.cwd = root.to_string_lossy().into_owned();
    timed.timeout_ms = 30;
    let timed = client.start(timed).await.unwrap();
    wait(&client, timed.execution_id).await;
    assert_eq!(
        client.inspect(timed.execution_id).await.unwrap().outcome,
        Some(ExecutionOutcome::TimedOut)
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!marker.exists(), "timeout left a descendant running");
    server.abort();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn dropping_service_owner_cleans_up_active_descendants() {
    let root = root("owner-drop");
    std::fs::create_dir_all(&root).unwrap();
    let marker = root.join("escaped-owner-child");
    let launcher = PathBuf::from(env!("CARGO_BIN_EXE_agl-execd-launcher"));
    let service = ExecutionService::start(&root.join("data"), launcher).unwrap();
    let mut active = request(
        vec![
            program("sh").to_string_lossy().into_owned(),
            "-c".to_owned(),
            "(sleep 0.4; printf escaped > escaped-owner-child) & wait".to_owned(),
        ],
        ExecutionIo::Pipes,
    );
    active.cwd = root.to_string_lossy().into_owned();
    active.timeout_ms = 5_000;
    service.launch(active).unwrap();
    std::thread::sleep(Duration::from_millis(40));
    drop(service);
    std::thread::sleep(Duration::from_millis(500));
    assert!(!marker.exists(), "service owner left a descendant running");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn exit_interrupt_and_descendant_cleanup_are_process_group_aware() {
    let root = root("outcomes-group");
    std::fs::create_dir_all(&root).unwrap();
    let (client, server) = start_server(&root).await;
    let exited = client
        .start(request(
            vec![
                program("sh").to_string_lossy().into_owned(),
                "-c".to_owned(),
                "exit 7".to_owned(),
            ],
            ExecutionIo::Pipes,
        ))
        .await
        .unwrap();
    wait(&client, exited.execution_id).await;
    assert_eq!(
        client.inspect(exited.execution_id).await.unwrap().outcome,
        Some(ExecutionOutcome::Exit { code: 7 })
    );

    let interrupted = client
        .start(request(
            vec![
                program("sleep").to_string_lossy().into_owned(),
                "30".to_owned(),
            ],
            ExecutionIo::Pipes,
        ))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    client
        .signal(interrupted.execution_id, ExecutionSignal::Interrupt)
        .await
        .unwrap();
    wait(&client, interrupted.execution_id).await;
    assert_eq!(
        client
            .inspect(interrupted.execution_id)
            .await
            .unwrap()
            .outcome,
        Some(ExecutionOutcome::Signal {
            signal: libc::SIGINT
        })
    );

    let marker = root.join("descendant-finished");
    let mut grouped = request(
        vec![
            program("sh").to_string_lossy().into_owned(),
            "-c".to_owned(),
            "(sleep 1; printf bad > descendant-finished) & wait".to_owned(),
        ],
        ExecutionIo::Pipes,
    );
    grouped.cwd = root.to_string_lossy().into_owned();
    let grouped = client.start(grouped).await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    client
        .signal(grouped.execution_id, ExecutionSignal::Terminate)
        .await
        .unwrap();
    wait(&client, grouped.execution_id).await;
    assert_eq!(
        client.inspect(grouped.execution_id).await.unwrap().outcome,
        Some(ExecutionOutcome::Terminated)
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        !marker.exists(),
        "descendant escaped the execution process group"
    );
    server.abort();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn execd_death_kills_the_process_tree_and_restart_records_unknown() {
    let root = root("service-death");
    std::fs::create_dir_all(&root).unwrap();
    let socket = root.join("state/execd/execd.sock");
    let marker = root.join("escaped-child");
    let mut execd = std::process::Command::new(env!("CARGO_BIN_EXE_agl-execd"))
        .env("AGL_HOME", &root)
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let client = ExecutionClient::new(&socket);
    let mut start = request(
        vec![
            program("sh").to_string_lossy().into_owned(),
            "-c".to_owned(),
            "(sleep 1; printf escaped > escaped-child) & wait".to_owned(),
        ],
        ExecutionIo::Pipes,
    );
    start.cwd = root.to_string_lossy().into_owned();
    let status = client.start(start).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    execd.kill().unwrap();
    execd.wait().unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        !marker.exists(),
        "launcher-owned descendant survived execd death"
    );

    let mut restarted = std::process::Command::new(env!("CARGO_BIN_EXE_agl-execd"))
        .env("AGL_HOME", &root)
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if ExecutionClient::new(&socket)
            .inspect(status.execution_id)
            .await
            .is_ok()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let recovered = ExecutionClient::new(&socket)
        .inspect(status.execution_id)
        .await
        .unwrap();
    assert_eq!(recovered.state, ExecutionState::OutcomeUnknown);
    assert_eq!(
        recovered.outcome,
        Some(ExecutionOutcome::UnknownAfterServiceRestart)
    );
    restarted.kill().unwrap();
    restarted.wait().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn socket_activation_and_private_launcher_identity_are_enforced() {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    use std::os::unix::process::CommandExt as _;

    let root = root("socket-activation");
    let socket = root.join("state/execd/execd.sock");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let listener_fd = listener.as_raw_fd();
    let launcher = PathBuf::from(env!("CARGO_BIN_EXE_agl-execd-launcher"));
    let launcher_metadata = std::fs::metadata(&launcher).unwrap();
    assert_eq!(launcher_metadata.uid(), unsafe { libc::geteuid() });
    assert_eq!(launcher_metadata.permissions().mode() & 0o022, 0);

    let mut command = std::process::Command::new(program("sh"));
    command
        .args([
            "-c",
            "LISTEN_PID=$$ LISTEN_FDS=1 exec \"$1\"",
            "sh",
            env!("CARGO_BIN_EXE_agl-execd"),
        ])
        .env("AGL_HOME", &root);
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(listener_fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let flags = libc::fcntl(3, libc::F_GETFD);
            if flags < 0 || libc::fcntl(3, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut execd = command.spawn().unwrap();
    drop(listener);
    let client = ExecutionClient::new(&socket);
    let mut ready = false;
    for _ in 0..200 {
        let probe = client
            .start(request(
                vec![
                    program("sh").to_string_lossy().into_owned(),
                    "-c".to_owned(),
                    "printf activated".to_owned(),
                ],
                ExecutionIo::Pipes,
            ))
            .await;
        if let Ok(status) = probe {
            wait(&client, status.execution_id).await;
            let output = client.read(status.execution_id, 0, 1024).await.unwrap();
            assert_eq!(output.chunks[0].data, b"activated");
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(ready, "socket-activated execd did not claim descriptor 3");
    execd.kill().unwrap();
    execd.wait().unwrap();
    let _ = std::fs::remove_dir_all(root);
}
