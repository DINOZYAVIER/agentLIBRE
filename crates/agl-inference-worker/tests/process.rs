#![cfg(target_os = "linux")]

use std::env;
use std::fs;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use agl_inference::worker_protocol::{
    HostCommand, MAX_WORKER_STDERR_EVIDENCE_BYTES, OperationId, SandboxConfiguration,
    ShutdownReason, WORKER_BUILD_ID, WORKER_PROTOCOL_ID, WorkerEvent, WorkerExecutable,
    WorkerProcess, WorkerProtocolErrorCode,
};
use agl_inference::worker_resources::discover_worker_launch_resources;
use agl_inference::{InferenceDeviceKind, ModelRuntime, WorkerModelRuntime, WorkerRuntimeOptions};

const PARENT_DEATH_HELPER_ENV: &str = "AGL_TEST_INFERENCE_PARENT_DEATH_HELPER";
const PARENT_DEATH_IDENTITY_PATH_ENV: &str = "AGL_TEST_INFERENCE_PARENT_DEATH_IDENTITY_PATH";

#[test]
fn exact_worker_binary_handshakes_and_shuts_down() {
    let executable = WorkerExecutable::open_exact(env!("CARGO_BIN_EXE_agl-inference-worker"))
        .expect("open exact worker executable");
    let worker =
        WorkerProcess::spawn(&executable, Duration::from_secs(5)).expect("spawn exact worker");

    assert_eq!(worker.identity().protocol_id(), WORKER_PROTOCOL_ID);
    assert_eq!(worker.identity().build_id(), WORKER_BUILD_ID);
    assert!(worker.child_id().is_some());
    let status = fs::read_to_string(format!(
        "/proc/{}/status",
        worker.child_id().expect("worker PID")
    ))
    .expect("read worker process status");
    assert!(status.lines().any(|line| line == "NoNewPrivs:\t1"));
    let identity = ProcessIdentity::capture(worker.child_id().expect("worker PID"));
    worker
        .shutdown(ShutdownReason::HostShutdown, Duration::from_secs(5))
        .expect("orderly worker shutdown");
    assert!(
        identity.is_gone(),
        "orderly host shutdown must reap the exact worker generation"
    );
}

#[test]
fn upgrade_shutdown_reaps_the_exact_worker_generation() {
    let executable = WorkerExecutable::open_exact(env!("CARGO_BIN_EXE_agl-inference-worker"))
        .expect("open exact worker executable");
    let worker =
        WorkerProcess::spawn(&executable, Duration::from_secs(5)).expect("spawn exact worker");
    let identity = ProcessIdentity::capture(worker.child_id().expect("worker PID"));

    worker
        .shutdown(ShutdownReason::Upgrade, Duration::from_secs(5))
        .expect("upgrade shutdown reaps worker");
    assert!(
        identity.is_gone(),
        "upgrade shutdown must not leave the previous worker generation"
    );
}

#[test]
fn dropping_host_owner_forces_reap_without_an_orphan() {
    let executable = WorkerExecutable::open_exact(env!("CARGO_BIN_EXE_agl-inference-worker"))
        .expect("open exact worker executable");
    let worker =
        WorkerProcess::spawn(&executable, Duration::from_secs(5)).expect("spawn exact worker");
    let identity = ProcessIdentity::capture(worker.child_id().expect("worker PID"));

    drop(worker);
    assert!(
        wait_until_process_gone(identity, Duration::from_secs(3)),
        "dropping the host owner must force-kill and reap its worker"
    );
}

#[test]
fn production_parent_death_kills_worker_without_an_orphan() {
    if env::var_os(PARENT_DEATH_HELPER_ENV).is_some() {
        run_parent_death_helper();
    }

    let directory = TestDirectory::new();
    let identity_path = directory.path().join("worker-identity");
    let status = Command::new(env::current_exe().expect("resolve process test executable"))
        .arg("--exact")
        .arg("production_parent_death_kills_worker_without_an_orphan")
        .arg("--nocapture")
        .env(PARENT_DEATH_HELPER_ENV, "1")
        .env(PARENT_DEATH_IDENTITY_PATH_ENV, &identity_path)
        .status()
        .expect("start disposable production worker host");
    assert!(status.success(), "parent-death helper exited with {status}");

    let identity = ProcessIdentity::parse(
        &fs::read_to_string(&identity_path).expect("read exact child process identity"),
    );
    if !wait_until_process_gone(identity, Duration::from_secs(5)) {
        identity.kill_if_current();
        panic!(
            "worker PID {} with start time {} survived its exact parent",
            identity.pid, identity.start_time_ticks
        );
    }
}

fn run_parent_death_helper() -> ! {
    let executable = WorkerExecutable::open_exact(env!("CARGO_BIN_EXE_agl-inference-worker"))
        .expect("open exact worker executable");
    let mut worker =
        WorkerProcess::spawn(&executable, Duration::from_secs(5)).expect("spawn exact worker");
    let identity = ProcessIdentity::capture(worker.child_id().expect("worker PID"));
    let identity_path = env::var_os(PARENT_DEATH_IDENTITY_PATH_ENV)
        .map(PathBuf::from)
        .expect("parent-death identity path");
    let private_temp = identity_path
        .parent()
        .expect("identity file has private parent")
        .join("parent-death-worker-temp");
    fs::create_dir(&private_temp).expect("create parent-death worker private temp");
    fs::set_permissions(&private_temp, fs::Permissions::from_mode(0o700))
        .expect("secure parent-death worker private temp");
    let configure_operation = OperationId::new(1).expect("configure operation ID");
    worker
        .channel_mut()
        .send(HostCommand::ConfigureSandbox {
            operation_id: configure_operation,
            configuration: production_sandbox_configuration(&private_temp),
        })
        .expect("configure helper worker sandbox");
    assert!(matches!(
        worker
            .channel_mut()
            .receive_timeout(Duration::from_secs(5))
            .expect("helper worker enters production sandbox"),
        WorkerEvent::SandboxReady { operation_id } if operation_id == configure_operation
    ));
    fs::write(
        identity_path,
        format!("{} {}\n", identity.pid, identity.start_time_ticks),
    )
    .expect("publish exact worker identity before parent exit");

    // Bypass `WorkerProcess::drop` to exercise the kernel parent-death path
    // used when a daemon is killed or crashes. The child has completed the
    // production launch handshake, including its exact parent check.
    std::mem::forget(worker);
    unsafe { libc::_exit(0) }
}

#[test]
fn forced_worker_termination_is_bounded_and_reaped() {
    let executable = WorkerExecutable::open_exact(env!("CARGO_BIN_EXE_agl-inference-worker"))
        .expect("open exact worker executable");
    let mut worker =
        WorkerProcess::spawn(&executable, Duration::from_secs(5)).expect("spawn exact worker");
    let pid = worker.child_id().expect("worker PID");

    worker
        .terminate_and_reap_with_timeout(Duration::from_secs(2))
        .expect("SIGKILL worker is reaped within the bounded deadline");
    assert_eq!(worker.child_id(), None);
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
}

#[test]
fn aborting_hostile_worker_stderr_is_drained_without_prompt_disclosure() {
    let directory = TestDirectory::new();
    let source = directory.path().join("hostile-worker.rs");
    let worker_path = directory.path().join("hostile-worker");
    let secret = "HOSTILE_PROMPT_BODY_must_not_leave_the_stderr_pipe";
    fs::write(
        &source,
        format!(
            r#"
fn main() {{
    for _ in 0..4096 {{
        eprintln!("VK_ERROR_DEVICE_LOST {{}}", {secret:?});
    }}
    std::process::abort();
}}
"#
        ),
    )
    .expect("write hostile worker source");
    let compiler = option_env!("RUSTC").unwrap_or("rustc");
    let gcc_runtime = loaded_library_directory("libgcc_s.so.1");
    let status = Command::new(compiler)
        .arg("--edition=2024")
        .arg("-C")
        .arg("strip=symbols")
        .arg("-C")
        .arg("panic=abort")
        .arg("-C")
        .arg(format!("link-arg=-Wl,-rpath,{}", gcc_runtime.display()))
        .arg("-C")
        .arg(format!(
            "link-arg=-Wl,-rpath,$ORIGIN/{}",
            native_bundle_relative_directory().display()
        ))
        .arg(&source)
        .arg("-o")
        .arg(&worker_path)
        .status()
        .expect("compile hostile worker fixture");
    assert!(status.success(), "hostile worker fixture must compile");

    let production_worker = Path::new(env!("CARGO_BIN_EXE_agl-inference-worker"));
    copy_native_bundle_for_worker(production_worker, &worker_path);
    let executable = WorkerExecutable::open_exact(&worker_path).expect("open hostile worker");
    let error = WorkerProcess::spawn(&executable, Duration::from_secs(5))
        .expect_err("aborting worker cannot complete the exact handshake");

    assert!(
        matches!(
            error.code(),
            WorkerProtocolErrorCode::PeerClosed | WorkerProtocolErrorCode::Io
        ),
        "unexpected hostile worker failure: {error}"
    );
    assert!(!error.to_string().contains(secret));
    let evidence = error.private_log();
    assert!(!evidence.is_empty());
    assert!(evidence.len() <= MAX_WORKER_STDERR_EVIDENCE_BYTES);
    assert!(evidence.contains("markers_retained=32"));
    assert!(!evidence.contains("markers_dropped=0"));
    assert!(evidence.contains("observed_non_authoritative_marker=vk_error_device_lost"));
    assert!(!evidence.contains(secret));
}

fn loaded_library_directory(file_name: &str) -> PathBuf {
    let maps = fs::read_to_string("/proc/self/maps").expect("read test process memory map");
    maps.lines()
        .filter_map(|line| line.split_whitespace().last())
        .map(Path::new)
        .find(|path| path.file_name().is_some_and(|name| name == file_name))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| panic!("test process did not map {file_name}"))
}

#[test]
fn production_worker_enters_sandbox_and_serves_status() {
    let private_temp = TestDirectory::new();
    let worker_path = Path::new(env!("CARGO_BIN_EXE_agl-inference-worker"));
    let native_bundle = native_bundle_for_worker(worker_path);
    let configuration = SandboxConfiguration::new(
        Vec::new(),
        Vec::new(),
        vec![native_bundle.to_string_lossy().into_owned()],
        Vec::new(),
        private_temp.path().to_string_lossy(),
    )
    .expect("build private worker sandbox configuration");
    let executable = WorkerExecutable::open_exact(env!("CARGO_BIN_EXE_agl-inference-worker"))
        .expect("open exact worker executable");
    let mut worker =
        WorkerProcess::spawn(&executable, Duration::from_secs(5)).expect("spawn exact worker");

    let configure_operation = OperationId::new(1).expect("configure operation ID");
    worker
        .channel_mut()
        .send(HostCommand::ConfigureSandbox {
            operation_id: configure_operation,
            configuration,
        })
        .expect("configure production worker sandbox");
    assert!(matches!(
        worker
            .channel_mut()
            .receive_timeout(Duration::from_secs(5))
            .expect("receive sandbox admission"),
        WorkerEvent::SandboxReady { operation_id } if operation_id == configure_operation
    ));

    let status_operation = OperationId::new(2).expect("status operation ID");
    worker
        .channel_mut()
        .send(HostCommand::Status {
            operation_id: status_operation,
        })
        .unwrap_or_else(|error| {
            let status = worker.try_wait().expect("inspect failed worker");
            panic!("request worker status: {error}; child status: {status:?}")
        });
    assert!(matches!(
        worker
            .channel_mut()
            .receive_timeout(Duration::from_secs(5))
            .expect("receive worker status"),
        WorkerEvent::Status {
            operation_id,
            snapshot,
        } if operation_id == status_operation
            && snapshot.loaded_models() == 0
            && snapshot.live_contexts() == 0
    ));

    worker
        .shutdown(ShutdownReason::Requested, Duration::from_secs(5))
        .expect("orderly sandboxed worker shutdown");
}

#[test]
fn production_worker_inventory_uses_only_explicit_vulkan_resources_when_available() {
    let resources = discover_worker_launch_resources().expect("discover admitted GPU resources");
    if resources.render_devices().is_empty() || resources.environment().is_empty() {
        return;
    }
    let roots = TestDirectory::new();
    let private_temp = roots.path().join("worker-temp");
    let device_leases = roots.path().join("device-leases");
    let health = roots.path().join("health");
    for path in [&private_temp, &device_leases, &health] {
        fs::create_dir(path).expect("create private production runtime root");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("make production runtime root private");
    }
    let worker_path = Path::new(env!("CARGO_BIN_EXE_agl-inference-worker"));
    let options = WorkerRuntimeOptions::from_launch_resources(&private_temp, &resources)
        .with_worker_executable(worker_path)
        .with_device_lease_root(&device_leases)
        .with_health_root(&health);
    let mut runtime = WorkerModelRuntime::new(options).expect("create production worker runtime");
    let inventory = runtime
        .device_inventory()
        .expect("inventory exact sandboxed Vulkan worker");
    assert!(
        inventory.iter().any(|device| {
            matches!(
                device.kind,
                InferenceDeviceKind::DiscreteGpu | InferenceDeviceKind::IntegratedGpu
            ) && device.usable
                && device.supports_gpu_offload
        }),
        "sandboxed worker inventory did not expose an admitted Vulkan GPU: {inventory:?}"
    );
}

#[test]
fn worker_exec_closes_unrelated_inheritable_descriptors() {
    let directory = TestDirectory::new();
    let marker_path = directory.path().join("must-not-reach-worker");
    fs::write(&marker_path, b"descriptor marker").expect("write descriptor marker");
    let marker = fs::File::open(&marker_path).expect("open descriptor marker");
    let marker_fd = marker.as_raw_fd();
    let flags = unsafe { libc::fcntl(marker_fd, libc::F_GETFD) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(marker_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) },
        0
    );

    let executable = WorkerExecutable::open_exact(env!("CARGO_BIN_EXE_agl-inference-worker"))
        .expect("open exact worker executable");
    let mut worker =
        WorkerProcess::spawn(&executable, Duration::from_secs(5)).expect("spawn exact worker");
    let private_temp = TestDirectory::new();
    let configuration = production_sandbox_configuration(private_temp.path());
    let configure_operation = OperationId::new(1).expect("configure operation ID");
    worker
        .channel_mut()
        .send(HostCommand::ConfigureSandbox {
            operation_id: configure_operation,
            configuration,
        })
        .expect("send production sandbox configuration");
    assert!(matches!(
        worker
            .channel_mut()
            .receive_timeout(Duration::from_secs(5))
            .expect("unrelated descriptor must have been closed before sandbox validation"),
        WorkerEvent::SandboxReady { operation_id } if operation_id == configure_operation
    ));
    worker
        .shutdown(ShutdownReason::Requested, Duration::from_secs(5))
        .expect("orderly worker shutdown");
}

fn production_sandbox_configuration(private_temp: &Path) -> SandboxConfiguration {
    let worker_path = Path::new(env!("CARGO_BIN_EXE_agl-inference-worker"));
    let native_bundle = native_bundle_for_worker(worker_path);
    SandboxConfiguration::new(
        Vec::new(),
        Vec::new(),
        vec![native_bundle.to_string_lossy().into_owned()],
        Vec::new(),
        private_temp.to_string_lossy(),
    )
    .expect("build private worker sandbox configuration")
}

#[test]
fn opened_worker_inode_is_pinned_across_path_substitution() {
    let directory = TestDirectory::new();
    let worker_path = directory.path().join("agl-inference-worker");
    let source_worker = Path::new(env!("CARGO_BIN_EXE_agl-inference-worker"));
    fs::copy(source_worker, &worker_path).expect("copy worker fixture");
    fs::set_permissions(&worker_path, fs::Permissions::from_mode(0o755))
        .expect("make worker fixture executable");
    copy_native_bundle_for_worker(source_worker, &worker_path);
    let executable = WorkerExecutable::open_exact(&worker_path).expect("pin worker inode");

    fs::remove_file(&worker_path).expect("unlink opened worker generation");
    fs::write(&worker_path, b"not the opened worker generation").expect("substitute worker path");
    fs::set_permissions(&worker_path, fs::Permissions::from_mode(0o755))
        .expect("make substituted path executable");

    let worker = WorkerProcess::spawn(&executable, Duration::from_secs(5))
        .expect("spawn pinned worker generation");
    worker
        .shutdown(ShutdownReason::Requested, Duration::from_secs(5))
        .expect("shutdown pinned worker");
}

fn native_bundle_relative_directory() -> &'static Path {
    Path::new(env!("AGL_INFERENCE_NATIVE_RELATIVE_DIR"))
}

fn native_bundle_for_worker(worker: &Path) -> PathBuf {
    worker
        .parent()
        .expect("worker has a sibling directory")
        .join(native_bundle_relative_directory())
}

fn copy_native_bundle_for_worker(source_worker: &Path, destination_worker: &Path) {
    let source = native_bundle_for_worker(source_worker);
    let destination = native_bundle_for_worker(destination_worker);
    fs::create_dir(
        destination
            .parent()
            .expect("native bundle leaf has a base directory"),
    )
    .expect("create copied native bundle base");
    fs::create_dir(&destination).expect("create copied native bundle");
    for entry in fs::read_dir(&source).expect("list source native bundle") {
        let entry = entry.expect("read source native bundle entry");
        assert!(entry.file_type().expect("inspect source entry").is_file());
        let destination_file = destination.join(entry.file_name());
        fs::copy(entry.path(), &destination_file).expect("copy native bundle entry");
        fs::set_permissions(&destination_file, fs::Permissions::from_mode(0o555))
            .expect("make copied native bundle entry immutable");
    }
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o555))
        .expect("make copied native bundle immutable");
}

#[test]
fn symlink_worker_is_rejected_before_spawn() {
    let directory = TestDirectory::new();
    let worker_path = directory.path().join("agl-inference-worker");
    symlink(env!("CARGO_BIN_EXE_agl-inference-worker"), &worker_path)
        .expect("create worker symlink");
    let error = WorkerExecutable::open_exact(worker_path).expect_err("reject worker symlink");
    assert_eq!(
        error.code(),
        agl_inference::worker_protocol::WorkerProtocolErrorCode::WorkerUntrusted
    );
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "agl-inference-worker-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create test directory");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .expect("make test directory private");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Clone, Copy, Debug)]
struct ProcessIdentity {
    pid: u32,
    start_time_ticks: u64,
}

impl ProcessIdentity {
    fn capture(pid: u32) -> Self {
        Self {
            pid,
            start_time_ticks: read_process_start_time(pid).expect("read worker process identity"),
        }
    }

    fn parse(serialized: &str) -> Self {
        let mut fields = serialized.split_whitespace();
        let pid = fields
            .next()
            .expect("worker PID field")
            .parse()
            .expect("valid worker PID");
        let start_time_ticks = fields
            .next()
            .expect("worker start-time field")
            .parse()
            .expect("valid worker start time");
        assert!(
            fields.next().is_none(),
            "unexpected process identity fields"
        );
        Self {
            pid,
            start_time_ticks,
        }
    }

    fn is_gone(self) -> bool {
        read_process_start_time(self.pid) != Some(self.start_time_ticks)
    }

    fn kill_if_current(self) {
        if !self.is_gone() {
            unsafe {
                libc::kill(self.pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

fn read_process_start_time(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = stat.rsplit_once(") ")?.1;
    // The suffix starts at field 3 (`state`); process start time is field 22.
    after_name.split_whitespace().nth(19)?.parse().ok()
}

fn wait_until_process_gone(identity: ProcessIdentity, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if identity.is_gone() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    identity.is_gone()
}
