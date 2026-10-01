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

fn nix_program(name: &str) -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|directory| directory.join(name))
        .find(|path| {
            path.canonicalize()
                .is_ok_and(|real| real.starts_with("/nix/store") && real.is_file())
        })
        .unwrap_or_else(|| panic!("missing Nix test executable: {name}"))
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

fn readonly_request(workspace: &Path, argv: Vec<String>) -> ExecutionStartRequest {
    let mut request = request(argv, ExecutionIo::Pipes);
    request.owner = ExecutionOwner::Agent {
        conversation_id: agl_core::ConversationId::generate(),
        run_id: agl_core::AgentRunId::generate(),
    };
    request.cwd = workspace.to_string_lossy().into_owned();
    request.isolation = ExecutionIsolation::ReadOnlyWorkspace {
        workspace_root: request.cwd.clone(),
        output_directories: Vec::new(),
    };
    request
}

#[tokio::test]
async fn explicit_output_trees_are_absent_from_the_planner_view() {
    use std::os::unix::fs::symlink;
    let root = root("excluded-outputs");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join("target")).unwrap();
    std::fs::create_dir_all(workspace.join("target-source")).unwrap();
    std::fs::write(workspace.join("source"), "source").unwrap();
    std::fs::write(workspace.join("target-source/input"), "source").unwrap();
    std::fs::write(
        workspace.join("agentLIBRE.toml"),
        "the daemon already parsed this declaration",
    )
    .unwrap();
    std::fs::write(workspace.join("target/cache"), "host build cache").unwrap();
    let fifo = std::ffi::CString::new(workspace.join("target/fifo").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    symlink(workspace.join("target/cache"), workspace.join("cache-link")).unwrap();
    let (client, server) = start_server(&root).await;
    let script = r#"
import os, pathlib
root = pathlib.Path.cwd()
assert (root / 'source').read_text() == 'source'
assert (root / 'target-source/input').read_text() == 'source'
assert (root / 'agentLIBRE.toml').read_text() == 'the daemon already parsed this declaration'
assert not (root / 'target').exists()
assert not (root / 'cache-link').exists()
scratch = pathlib.Path(os.environ['TMPDIR'])
(scratch / 'build-output').write_text('allowed')
"#;
    let mut request = readonly_request(
        &workspace,
        vec![
            nix_program("python3").to_string_lossy().into_owned(),
            "-c".into(),
            script.into(),
        ],
    );
    let ExecutionIsolation::ReadOnlyWorkspace {
        output_directories, ..
    } = &mut request.isolation
    else {
        unreachable!()
    };
    *output_directories = vec!["target".into()];
    let status = client.start(request).await.unwrap();
    wait(&client, status.execution_id).await;
    assert_eq!(
        client.inspect(status.execution_id).await.unwrap().outcome,
        Some(ExecutionOutcome::Exit { code: 0 }),
        "{:?}",
        client.read(status.execution_id, 0, 65536).await.unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("target/cache")).unwrap(),
        "host build cache"
    );
    assert!(
        std::fs::read_dir(root.join("data/planner-scratch"))
            .unwrap()
            .next()
            .is_none()
    );
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn readonly_workspace_blocks_data_and_metadata_writes_and_cleans_scratch() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = root("readonly");
    let workspace = root.join("workspace");
    let git = root.join("external-git");
    let common = root.join("git-common");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&git).unwrap();
    std::fs::create_dir_all(&common).unwrap();
    std::fs::write(workspace.join("input"), "b\na\n").unwrap();
    std::fs::write(
        workspace.join(".git"),
        format!("gitdir: {}\n", git.display()),
    )
    .unwrap();
    std::fs::write(git.join("index"), "unchanged").unwrap();
    std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(git.join("commondir"), "../git-common\n").unwrap();
    std::fs::write(common.join("config"), "common-config").unwrap();
    symlink(git.join("index"), workspace.join("escape")).unwrap();
    let mode = std::fs::metadata(workspace.join("input"))
        .unwrap()
        .permissions()
        .mode();
    let (client, server) = start_server(&root).await;
    let script = r#"
import os, pathlib, subprocess, sys
workspace, git = map(pathlib.Path, sys.argv[1:3])
assert (workspace / 'input').read_text() == 'b\na\n'
assert (git / 'index').read_text() == 'unchanged'
common = (git / (git / 'commondir').read_text().strip()).resolve()
assert (common / 'config').read_text() == 'common-config'
actions = [
    lambda: (workspace / 'created').write_text('bad'),
    lambda: (workspace / 'input').write_text('bad'),
    lambda: (workspace / 'input').unlink(),
    lambda: (workspace / 'input').rename(workspace / 'renamed'),
    lambda: os.chmod(workspace / 'input', 0o777),
    lambda: os.utime(workspace / 'input', None),
    lambda: (git / 'index').write_text('bad'),
    lambda: (common / 'config').write_text('bad'),
    lambda: (workspace / 'escape').write_text('bad'),
    lambda: os.link(workspace / 'input', pathlib.Path(os.environ['TMPDIR']) / 'linked'),
]
for action in actions:
    try: action()
    except OSError: pass
    else: raise AssertionError('workspace write escaped sandbox')
assert subprocess.run([sys.argv[3], '-o', str(workspace / 'sorted'), str(workspace / 'input')]).returncode != 0
assert subprocess.run([sys.argv[4], '-n', 'w ' + str(workspace / 'sed-output'), str(workspace / 'input')]).returncode != 0
scratch = pathlib.Path(os.environ['TMPDIR'])
(scratch / 'output').write_text('allowed')
(scratch / 'locked').mkdir()
os.chmod(scratch / 'locked', 0)
print(scratch)
"#;
    let status = client
        .start(readonly_request(
            &workspace,
            vec![
                nix_program("python3").to_string_lossy().into_owned(),
                "-B".into(),
                "-c".into(),
                script.into(),
                workspace.to_string_lossy().into_owned(),
                git.to_string_lossy().into_owned(),
                program("sort").to_string_lossy().into_owned(),
                program("sed").to_string_lossy().into_owned(),
            ],
        ))
        .await
        .unwrap();
    wait(&client, status.execution_id).await;
    let outcome = client.inspect(status.execution_id).await.unwrap().outcome;
    let output = client
        .read(status.execution_id, 0, 64 * 1024)
        .await
        .unwrap();
    let stdout = output
        .chunks
        .iter()
        .filter(|chunk| chunk.stream == ExecutionOutputStream::Stdout)
        .flat_map(|chunk| chunk.data.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        outcome,
        Some(ExecutionOutcome::Exit { code: 0 }),
        "{output:?}"
    );
    let scratch = PathBuf::from(String::from_utf8(stdout).unwrap().trim());
    assert!(scratch.starts_with(root.join("data/planner-scratch")));
    assert!(!scratch.exists());
    assert_eq!(
        std::fs::read_to_string(workspace.join("input")).unwrap(),
        "b\na\n"
    );
    assert_eq!(
        std::fs::metadata(workspace.join("input"))
            .unwrap()
            .permissions()
            .mode(),
        mode
    );
    assert_eq!(
        std::fs::read_to_string(git.join("index")).unwrap(),
        "unchanged"
    );
    assert!(!workspace.join("created").exists());
    assert!(!workspace.join("sorted").exists());
    assert!(!workspace.join("sed-output").exists());
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn readonly_execution_denies_network_and_host_unix_socket_access() {
    let root = root("readonly-sockets");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(root.join("host.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (client, server) = start_server(&root).await;
    let script = r#"
import ctypes, errno, os, socket
assert os.environ['TEST_SANDBOX_ENV'] == 'fixture'
assert 'PATH' not in os.environ
for family in [socket.AF_INET, socket.AF_INET6, socket.AF_UNIX]:
    try: socket.socket(family)
    except PermissionError: pass
    else: raise AssertionError('socket creation was allowed')
libc = ctypes.CDLL(None, use_errno=True)
params = ctypes.create_string_buffer(256)
assert libc.syscall(425, 8, ctypes.byref(params)) == -1
assert ctypes.get_errno() == errno.EPERM, 'io_uring setup was allowed'
print('isolated')
"#;
    let mut request = readonly_request(
        &workspace,
        vec![
            nix_program("python3").to_string_lossy().into_owned(),
            "-B".into(),
            "-c".into(),
            script.into(),
        ],
    );
    request.clear_environment = true;
    request
        .environment
        .insert("TEST_SANDBOX_ENV".into(), "fixture".into());
    let status = client.start(request).await.unwrap();
    wait(&client, status.execution_id).await;
    assert_eq!(
        client.inspect(status.execution_id).await.unwrap().outcome,
        Some(ExecutionOutcome::Exit { code: 0 }),
        "{:?}",
        client.read(status.execution_id, 0, 65536).await.unwrap()
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn readonly_view_hides_host_fifos_and_other_host_files() {
    use std::os::unix::fs::{OpenOptionsExt, symlink};
    let root = root("readonly-fifo");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let fifo = root.join("host.fifo");
    let path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let _host = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .unwrap();
    symlink(&fifo, workspace.join("fifo-alias")).unwrap();
    std::fs::write(root.join("host-secret"), "hidden").unwrap();
    let (client, server) = start_server(&root).await;
    let script = r#"
import errno, os, pathlib, sys
for path in sys.argv[1:3]:
    for flags in [os.O_RDONLY, os.O_WRONLY, os.O_RDWR]:
        try: os.open(path, flags | os.O_NONBLOCK)
        except OSError as error: assert error.errno == errno.ENOENT
        else: raise AssertionError('host FIFO was accessible')
assert not pathlib.Path(sys.argv[3]).exists()
assert not pathlib.Path('/run/user').exists()
print('fifo-isolated')
"#;
    let status = client
        .start(readonly_request(
            &workspace,
            vec![
                nix_program("python3").to_string_lossy().into_owned(),
                "-B".into(),
                "-c".into(),
                script.into(),
                fifo.to_string_lossy().into_owned(),
                workspace.join("fifo-alias").to_string_lossy().into_owned(),
                root.join("host-secret").to_string_lossy().into_owned(),
            ],
        ))
        .await
        .unwrap();
    wait(&client, status.execution_id).await;
    assert_eq!(
        client.inspect(status.execution_id).await.unwrap().outcome,
        Some(ExecutionOutcome::Exit { code: 0 }),
        "{:?}",
        client.read(status.execution_id, 0, 65536).await.unwrap()
    );
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn readonly_view_rejects_special_files_and_executables_outside_nix() {
    let root = root("readonly-special");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let fifo = workspace.join("fifo");
    let path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let (client, server) = start_server(&root).await;
    let error = client
        .start(readonly_request(
            &workspace,
            vec![nix_program("true").to_string_lossy().into_owned()],
        ))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unsupported special file"));
    std::fs::remove_file(fifo).unwrap();
    let outside = root.join("outside-true");
    std::fs::copy(nix_program("true"), &outside).unwrap();
    let error = client
        .start(readonly_request(
            &workspace,
            vec![outside.to_string_lossy().into_owned()],
        ))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Nix store executable"));
    assert_eq!(
        std::fs::read_dir(root.join("data/planner-scratch"))
            .unwrap()
            .count(),
        0
    );
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn reflink_view_retains_original_data_and_excludes_later_host_fifos() {
    let root = root("readonly-reflink");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("input"), "original-data").unwrap();
    let (client, server) = start_server(&root).await;
    let script = r#"
import errno, os, pathlib, time
print('READY', flush=True)
time.sleep(0.3)
assert pathlib.Path('input').read_text() == 'original-data'
try: os.open('late-fifo', os.O_RDONLY | os.O_NONBLOCK)
except OSError as error: assert error.errno == errno.ENOENT
else: raise AssertionError('late host FIFO entered the view')
print('stable-copy')
"#;
    let status = client
        .start(readonly_request(
            &workspace,
            vec![
                nix_program("python3").to_string_lossy().into_owned(),
                "-B".into(),
                "-c".into(),
                script.into(),
            ],
        ))
        .await
        .unwrap();
    let mut ready = false;
    for _ in 0..200 {
        let output = client.read(status.execution_id, 0, 65536).await.unwrap();
        if output
            .chunks
            .iter()
            .any(|chunk| String::from_utf8_lossy(&chunk.data).contains("READY"))
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert!(ready);
    std::fs::write(workspace.join("input"), "new-host-data").unwrap();
    let fifo = workspace.join("late-fifo");
    let path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    wait(&client, status.execution_id).await;
    assert_eq!(
        client.inspect(status.execution_id).await.unwrap().outcome,
        Some(ExecutionOutcome::Exit { code: 0 }),
        "{:?}",
        client.read(status.execution_id, 0, 65536).await.unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("input")).unwrap(),
        "new-host-data"
    );
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn missing_bubblewrap_fails_without_starting_the_command() {
    let root = root("missing-bwrap");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let socket = root.join("state/execd/execd.sock");
    let mut execd = std::process::Command::new(env!("CARGO_BIN_EXE_agl-execd"))
        .env("AGL_HOME", &root)
        .env("PATH", root.join("missing-bin"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let result = ExecutionClient::new(&socket)
        .start(readonly_request(
            &workspace,
            vec![
                program("sh").to_string_lossy().into_owned(),
                "-c".into(),
                "touch escaped".into(),
            ],
        ))
        .await;
    execd.kill().unwrap();
    execd.wait().unwrap();
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("bubblewrap is required")
    );
    assert!(!workspace.join("escaped").exists());
    assert_eq!(
        std::fs::read_dir(root.join("data/execd/planner-scratch"))
            .unwrap()
            .count(),
        0
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn readonly_execution_rejects_cwd_escape_and_scratch_inside_workspace() {
    let root = root("readonly-paths");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let (client, server) = start_server(&root).await;
    let mut escaped = readonly_request(
        &workspace,
        vec![program("true").to_string_lossy().into_owned()],
    );
    escaped.cwd = root.to_string_lossy().into_owned();
    assert!(client.start(escaped).await.is_err());
    let overlapping = readonly_request(&root, vec![program("true").to_string_lossy().into_owned()]);
    assert!(client.start(overlapping).await.is_err());
    assert!(
        !root.join("data/planner-scratch").exists()
            || std::fs::read_dir(root.join("data/planner-scratch"))
                .unwrap()
                .next()
                .is_none()
    );
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn readonly_timeout_terminates_descendants_and_removes_execution_scratch() {
    let root = root("readonly-timeout");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let (client, server) = start_server(&root).await;
    let mut request = readonly_request(
        &workspace,
        vec![
            program("sh").to_string_lossy().into_owned(),
            "-c".into(),
            "printf '%s\\n' \"$TMPDIR\"; sleep 30 & wait".into(),
        ],
    );
    request.timeout_ms = 300;
    let status = client.start(request).await.unwrap();
    wait(&client, status.execution_id).await;
    assert_eq!(
        client.inspect(status.execution_id).await.unwrap().outcome,
        Some(ExecutionOutcome::TimedOut)
    );
    let output = client.read(status.execution_id, 0, 65536).await.unwrap();
    assert!(output.eof);
    assert!(
        output
            .chunks
            .iter()
            .any(|chunk| chunk.stream == ExecutionOutputStream::Stdout && !chunk.data.is_empty()),
        "{output:?}"
    );
    assert!(
        std::fs::read_dir(root.join("data/planner-scratch"))
            .unwrap()
            .next()
            .is_none()
    );
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn service_recovery_removes_orphaned_planner_scratch() {
    let root = root("readonly-recovery");
    let orphan = root
        .join("planner-scratch")
        .join(agl_execution_api::ExecutionId::generate().to_string());
    std::fs::create_dir_all(&orphan).unwrap();
    std::fs::write(orphan.join("output"), "orphaned").unwrap();
    let service = ExecutionService::start(
        &root,
        PathBuf::from(env!("CARGO_BIN_EXE_agl-execd-launcher")),
    )
    .unwrap();
    assert!(!orphan.exists());
    drop(service);
    std::fs::remove_dir_all(root).unwrap();
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
