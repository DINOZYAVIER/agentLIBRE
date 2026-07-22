#![cfg(target_os = "linux")]

#[path = "../src/sandbox.rs"]
mod sandbox;

use std::env;
use std::ffi::CString;
use std::fs;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd};
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::{Arc, Barrier};
use std::time::{SystemTime, UNIX_EPOCH};

use agl_inference::worker_protocol::SandboxConfiguration;

const PROBE_ENV: &str = "AGL_SANDBOX_PROBE";
const CONTROL_FD_ENV: &str = "AGL_SANDBOX_TEST_CONTROL_FD";
const MODEL_ENV: &str = "AGL_SANDBOX_TEST_MODEL";
const PROJECTOR_ENV: &str = "AGL_SANDBOX_TEST_PROJECTOR";
const NATIVE_ROOT_ENV: &str = "AGL_SANDBOX_TEST_NATIVE_ROOT";
const WORKSPACE_SECRET_ENV: &str = "AGL_SANDBOX_TEST_WORKSPACE_SECRET";
const DAEMON_DB_ENV: &str = "AGL_SANDBOX_TEST_DAEMON_DB";
const PTY_ENV: &str = "AGL_SANDBOX_TEST_PTY";
const TEMP_ENV: &str = "AGL_SANDBOX_TEST_TEMP";
const BAD_TEMP_ENV: &str = "AGL_SANDBOX_TEST_BAD_TEMP";
const SYMLINK_ENV: &str = "AGL_SANDBOX_TEST_SYMLINK";

fn main() -> ExitCode {
    if let Ok(probe) = env::var(PROBE_ENV) {
        return match run_child_probe(&probe) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("sandbox probe {probe} failed: {error}");
                ExitCode::FAILURE
            }
        };
    }

    match run_parent() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("sandbox integration test failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_parent() -> std::result::Result<(), String> {
    let fixture = Fixture::new().map_err(|error| error.to_string())?;
    for probe in [
        "allowed",
        "workspace_denied",
        "daemon_db_denied",
        "host_filesystem_denied",
        "pty_denied",
        "network_denied",
        "shell_exec_denied",
        "process_and_ptrace_denied",
        "device_denied",
        "bad_gpu_rejected",
        "symlink_rejected",
        "bad_temp_mode_rejected",
        "unrelated_fd_rejected",
        "multithreaded_entry_rejected",
    ] {
        run_probe(probe, &fixture)?;
    }
    Ok(())
}

fn run_probe(probe: &str, fixture: &Fixture) -> std::result::Result<(), String> {
    let (host_socket, child_socket) = seqpacket_pair().map_err(|error| error.to_string())?;
    let child_socket = duplicate_for_child(child_socket.as_raw_fd())
        .map_err(|error| format!("failed to pin child control FD: {error}"))?;
    clear_close_on_exec(child_socket.as_raw_fd()).map_err(|error| error.to_string())?;

    let executable = env::current_exe().map_err(|error| error.to_string())?;
    let status = Command::new(executable)
        .env_clear()
        .env(PROBE_ENV, probe)
        .env(CONTROL_FD_ENV, child_socket.as_raw_fd().to_string())
        .env(MODEL_ENV, fixture.model.as_os_str())
        .env(PROJECTOR_ENV, fixture.projector.as_os_str())
        .env(NATIVE_ROOT_ENV, fixture.native_root.as_os_str())
        .env(WORKSPACE_SECRET_ENV, fixture.workspace_secret.as_os_str())
        .env(DAEMON_DB_ENV, fixture.daemon_db.as_os_str())
        .env(PTY_ENV, fixture.pty.path.as_os_str())
        .env(TEMP_ENV, fixture.private_temp.as_os_str())
        .env(BAD_TEMP_ENV, fixture.bad_temp.as_os_str())
        .env(SYMLINK_ENV, fixture.allowed_symlink.as_os_str())
        .status()
        .map_err(|error| format!("failed to start {probe} probe: {error}"))?;
    drop(child_socket);
    drop(host_socket);
    if !status.success() {
        return Err(format!("{probe} probe exited with {status}"));
    }
    Ok(())
}

fn run_child_probe(probe: &str) -> std::result::Result<(), String> {
    let control_fd = env::var(CONTROL_FD_ENV)
        .map_err(|error| error.to_string())?
        .parse::<RawFd>()
        .map_err(|error| error.to_string())?;
    let model = PathBuf::from(env::var_os(MODEL_ENV).ok_or("missing model path")?);
    let projector = PathBuf::from(env::var_os(PROJECTOR_ENV).ok_or("missing projector path")?);
    let native_root =
        PathBuf::from(env::var_os(NATIVE_ROOT_ENV).ok_or("missing native root path")?);
    let workspace_secret =
        PathBuf::from(env::var_os(WORKSPACE_SECRET_ENV).ok_or("missing workspace secret path")?);
    let daemon_db = PathBuf::from(env::var_os(DAEMON_DB_ENV).ok_or("missing daemon DB path")?);
    let pty = PathBuf::from(env::var_os(PTY_ENV).ok_or("missing PTY path")?);
    let private_temp = PathBuf::from(env::var_os(TEMP_ENV).ok_or("missing temp path")?);
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } != 0 {
        return Err(format!(
            "failed to install production parent-death behavior: {}",
            io::Error::last_os_error()
        ));
    }

    match probe {
        "bad_gpu_rejected" => {
            let configuration = configuration(
                &model,
                &projector,
                &native_root,
                &private_temp,
                vec!["/dev/null".into()],
            )?;
            expect_entry_error(
                sandbox::enter(&configuration, control_fd),
                sandbox::SandboxErrorCode::InvalidConfiguration,
            )
        }
        "symlink_rejected" => {
            let link = PathBuf::from(env::var_os(SYMLINK_ENV).ok_or("missing symlink path")?);
            let configuration =
                configuration(&link, &projector, &native_root, &private_temp, Vec::new())?;
            expect_entry_error(
                sandbox::enter(&configuration, control_fd),
                sandbox::SandboxErrorCode::InvalidConfiguration,
            )
        }
        "bad_temp_mode_rejected" => {
            let bad_temp = PathBuf::from(env::var_os(BAD_TEMP_ENV).ok_or("missing bad temp path")?);
            let configuration =
                configuration(&model, &projector, &native_root, &bad_temp, Vec::new())?;
            expect_entry_error(
                sandbox::enter(&configuration, control_fd),
                sandbox::SandboxErrorCode::InvalidConfiguration,
            )
        }
        "unrelated_fd_rejected" => {
            let unrelated = fs::File::open(&workspace_secret).map_err(|error| error.to_string())?;
            let configuration =
                configuration(&model, &projector, &native_root, &private_temp, Vec::new())?;
            let result = expect_entry_error(
                sandbox::enter(&configuration, control_fd),
                sandbox::SandboxErrorCode::UnexpectedProcessState,
            );
            drop(unrelated);
            result
        }
        "multithreaded_entry_rejected" => {
            let barrier = Arc::new(Barrier::new(2));
            let child_barrier = Arc::clone(&barrier);
            let thread = std::thread::spawn(move || {
                child_barrier.wait();
                child_barrier.wait();
            });
            barrier.wait();
            let configuration =
                configuration(&model, &projector, &native_root, &private_temp, Vec::new())?;
            let result = expect_entry_error(
                sandbox::enter(&configuration, control_fd),
                sandbox::SandboxErrorCode::UnexpectedProcessState,
            );
            barrier.wait();
            thread
                .join()
                .map_err(|_| "probe thread panicked".to_owned())?;
            result
        }
        _ => {
            let configuration =
                configuration(&model, &projector, &native_root, &private_temp, Vec::new())?;
            let report = sandbox::enter(&configuration, control_fd)
                .map_err(|error| format!("sandbox entry failed: {error}"))?;
            match probe {
                "allowed" => probe_allowed(&model, &projector, &native_root, control_fd, report),
                "workspace_denied" => probe_path_denied(&workspace_secret, "workspace secret"),
                "daemon_db_denied" => probe_path_denied(&daemon_db, "daemon database"),
                "host_filesystem_denied" => probe_host_filesystem_denied(),
                "pty_denied" => probe_pty_denied(&pty),
                "network_denied" => probe_network_denied(),
                "shell_exec_denied" => probe_shell_exec_denied(),
                "process_and_ptrace_denied" => probe_process_and_ptrace_denied(),
                "device_denied" => probe_device_denied(),
                _ => Err(format!("unknown sandbox probe {probe}")),
            }
        }
    }
}

fn configuration(
    model: &Path,
    projector: &Path,
    native_root: &Path,
    private_temp: &Path,
    gpu_paths: Vec<String>,
) -> std::result::Result<SandboxConfiguration, String> {
    SandboxConfiguration::new(
        vec![path_string(model)?],
        vec![path_string(projector)?],
        vec![path_string(native_root)?],
        gpu_paths,
        path_string(private_temp)?,
    )
    .map_err(|error| error.to_string())
}

fn expect_entry_error(
    result: sandbox::Result<sandbox::SandboxReport>,
    expected: sandbox::SandboxErrorCode,
) -> std::result::Result<(), String> {
    match result {
        Err(error) if error.code() == expected => Ok(()),
        Err(error) => Err(format!(
            "expected {} but received {error}",
            expected.as_str()
        )),
        Ok(_) => Err(format!(
            "expected {} but sandbox entry succeeded",
            expected.as_str()
        )),
    }
}

fn probe_allowed(
    model: &Path,
    projector: &Path,
    native_root: &Path,
    control_fd: RawFd,
    report: sandbox::SandboxReport,
) -> std::result::Result<(), String> {
    let bytes = fs::read(model).map_err(|error| format!("admitted model read failed: {error}"))?;
    if bytes != b"admitted model bytes" {
        return Err("admitted model contents changed".to_owned());
    }
    let bytes =
        fs::read(projector).map_err(|error| format!("admitted projector read failed: {error}"))?;
    if bytes != b"admitted projector bytes" {
        return Err("admitted projector contents changed".to_owned());
    }
    let native_library = native_root.join("libllama.so");
    let bytes = fs::read(&native_library)
        .map_err(|error| format!("admitted native library read failed: {error}"))?;
    if bytes != b"admitted native library bytes" {
        return Err("admitted native library contents changed".to_owned());
    }
    for (path, label) in [
        (model, "model root"),
        (projector, "projector root"),
        (native_library.as_path(), "native root"),
    ] {
        expect_permission_denied(
            fs::OpenOptions::new().write(true).open(path),
            &format!("write to admitted read-only {label}"),
        )?;
    }
    expect_permission_denied(
        fs::write(native_root.join("injected.so"), b"not admitted"),
        "create below admitted read-only native root",
    )?;
    let cache = PathBuf::from(format!("cache-{}", unsafe { libc::getpid() }));
    fs::write(&cache, b"private cache")
        .map_err(|error| format!("private temp write failed: {error}"))?;
    if fs::read(&cache).map_err(|error| error.to_string())? != b"private cache" {
        return Err("private temp contents changed".to_owned());
    }

    if unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err("worker remained dumpable".to_owned());
    }
    if unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } != 1 {
        return Err("worker lacks no_new_privs".to_owned());
    }
    let mut parent_death_signal = 0;
    if unsafe { libc::prctl(libc::PR_GET_PDEATHSIG, &mut parent_death_signal, 0, 0, 0) } != 0 {
        return Err(format!(
            "failed to inspect worker parent-death behavior: {}",
            io::Error::last_os_error()
        ));
    }
    if parent_death_signal != libc::SIGKILL {
        return Err("worker parent-death behavior was not SIGKILL".to_owned());
    }
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, 0, 0, 0, 0) } != -1 {
        return Err("sandbox cleared worker parent-death behavior".to_owned());
    }
    expect_errno(libc::EPERM, "PR_SET_PDEATHSIG")?;
    parent_death_signal = 0;
    if unsafe { libc::prctl(libc::PR_GET_PDEATHSIG, &mut parent_death_signal, 0, 0, 0) } != 0
        || parent_death_signal != libc::SIGKILL
    {
        return Err("denied prctl changed worker parent-death behavior".to_owned());
    }
    if report.landlock_abi < 5 || !report.seccomp_tsync {
        return Err("sandbox report omitted kernel enforcement".to_owned());
    }
    if report.resource_limits.open_files > 256
        || report.resource_limits.file_size_bytes > 512 * 1024 * 1024
        || report.resource_limits.address_space_bytes > 512 * 1024 * 1024 * 1024
        || report.resource_limits.processes_and_threads > 4096
        || report.resource_limits.stack_bytes > 64 * 1024 * 1024
        || report.resource_limits.locked_memory_bytes > 8 * 1024 * 1024 * 1024
    {
        return Err("sandbox resource limits were not bounded".to_owned());
    }
    let mut core = mem::MaybeUninit::<libc::rlimit>::zeroed();
    if unsafe { libc::getrlimit(libc::RLIMIT_CORE, core.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    let core = unsafe { core.assume_init() };
    if core.rlim_cur != 0 || core.rlim_max != 0 {
        return Err("core dump resource limit was not zero".to_owned());
    }

    let thread = std::thread::spawn(|| 41_u32 + 1);
    if thread
        .join()
        .map_err(|_| "sandbox thread panicked".to_owned())?
        != 42
    {
        return Err("ordinary thread returned the wrong value".to_owned());
    }
    send_control_byte(control_fd)
}

fn probe_path_denied(path: &Path, label: &str) -> std::result::Result<(), String> {
    expect_permission_denied(fs::read(path), label)
}

fn probe_host_filesystem_denied() -> std::result::Result<(), String> {
    expect_permission_denied(fs::read("/etc/passwd"), "unadmitted host file")?;
    expect_permission_denied(fs::read("/proc/self/status"), "procfs")
}

fn probe_pty_denied(pty: &Path) -> std::result::Result<(), String> {
    expect_permission_denied(
        fs::OpenOptions::new().read(true).write(true).open(pty),
        "human terminal PTY",
    )
}

fn probe_network_denied() -> std::result::Result<(), String> {
    for (domain, label) in [
        (libc::AF_INET, "IPv4 socket"),
        (libc::AF_INET6, "IPv6 socket"),
        (libc::AF_UNIX, "additional Unix socket"),
    ] {
        let descriptor = unsafe { libc::socket(domain, libc::SOCK_STREAM, 0) };
        if descriptor >= 0 {
            unsafe {
                libc::close(descriptor);
            }
            return Err(format!("sandbox created {label}"));
        }
        expect_errno(libc::EPERM, label)?;
    }
    Ok(())
}

fn probe_shell_exec_denied() -> std::result::Result<(), String> {
    let program = CString::new("/bin/sh").expect("static shell path");
    let argument = program.as_ptr();
    let arguments = [argument, std::ptr::null()];
    let environment = [std::ptr::null::<libc::c_char>()];
    let result = unsafe {
        libc::syscall(
            libc::SYS_execve,
            program.as_ptr(),
            arguments.as_ptr(),
            environment.as_ptr(),
        )
    };
    if result != -1 {
        return Err("sandbox executed a second program".to_owned());
    }
    expect_errno(libc::EPERM, "shell execve")
}

fn probe_process_and_ptrace_denied() -> std::result::Result<(), String> {
    let child = unsafe { libc::fork() };
    if child == 0 {
        unsafe { libc::_exit(90) }
    }
    if child > 0 {
        unsafe {
            libc::kill(child, libc::SIGKILL);
            libc::waitpid(child, std::ptr::null_mut(), 0);
        }
        return Err("sandbox forked a child process".to_owned());
    }
    expect_errno(libc::EPERM, "fork")?;

    let ptrace = unsafe {
        libc::ptrace(
            libc::PTRACE_TRACEME,
            0,
            std::ptr::null_mut::<libc::c_void>(),
            std::ptr::null_mut::<libc::c_void>(),
        )
    };
    if ptrace != -1 {
        return Err("sandbox enabled ptrace".to_owned());
    }
    expect_errno(libc::EPERM, "ptrace")?;

    let parent = unsafe { libc::getppid() };
    let signal = unsafe { libc::syscall(libc::SYS_tgkill, parent, parent, 0) };
    if signal != -1 {
        return Err("sandbox could signal its parent process".to_owned());
    }
    expect_errno(libc::EPERM, "cross-process tgkill")
}

fn probe_device_denied() -> std::result::Result<(), String> {
    expect_permission_denied(fs::File::open("/dev/null"), "unadmitted device")?;
    if let Ok(entries) = fs::read_dir("/dev/dri") {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("renderD") {
                return expect_permission_denied(
                    fs::File::open(entry.path()),
                    "unadmitted GPU render node",
                );
            }
        }
    }
    Ok(())
}

fn expect_permission_denied<T>(
    result: io::Result<T>,
    label: &str,
) -> std::result::Result<(), String> {
    match result {
        Err(error) if matches!(error.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)) => {
            Ok(())
        }
        Err(error) => Err(format!("{label} failed with unexpected error: {error}")),
        Ok(_) => Err(format!("sandbox accessed {label}")),
    }
}

fn expect_errno(expected: i32, label: &str) -> std::result::Result<(), String> {
    let actual = io::Error::last_os_error().raw_os_error();
    if actual == Some(expected) {
        Ok(())
    } else {
        Err(format!(
            "{label} returned errno {actual:?}, expected {expected}"
        ))
    }
}

fn send_control_byte(control_fd: RawFd) -> std::result::Result<(), String> {
    let mut byte = [0x5a_u8];
    let mut vector = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let mut message = unsafe { mem::zeroed::<libc::msghdr>() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    let sent = unsafe { libc::sendmsg(control_fd, &message, libc::MSG_NOSIGNAL) };
    if sent == 1 {
        Ok(())
    } else {
        Err(format!(
            "private control sendmsg failed: {}",
            io::Error::last_os_error()
        ))
    }
}

fn path_string(path: &Path) -> std::result::Result<String, String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| "test path is not UTF-8".to_owned())
}

fn seqpacket_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            descriptors.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    })
}

fn clear_close_on_exec(descriptor: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn duplicate_for_child(descriptor: RawFd) -> io::Result<OwnedFd> {
    let duplicated = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, 100) };
    if duplicated < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

struct Fixture {
    root: PathBuf,
    model: PathBuf,
    projector: PathBuf,
    native_root: PathBuf,
    workspace_secret: PathBuf,
    daemon_db: PathBuf,
    pty: PseudoTerminal,
    private_temp: PathBuf,
    bad_temp: PathBuf,
    allowed_symlink: PathBuf,
}

impl Fixture {
    fn new() -> io::Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "agl-inference-sandbox-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;

        let private_temp = root.join("private-temp");
        fs::create_dir(&private_temp)?;
        fs::set_permissions(&private_temp, fs::Permissions::from_mode(0o700))?;
        let bad_temp = root.join("bad-temp");
        fs::create_dir(&bad_temp)?;
        fs::set_permissions(&bad_temp, fs::Permissions::from_mode(0o755))?;

        let model = root.join("model.gguf");
        fs::write(&model, b"admitted model bytes")?;
        let projector = root.join("projector.mmproj");
        fs::write(&projector, b"admitted projector bytes")?;
        let native_root = root
            .join("agl-inference-native")
            .join(format!("sha256-{}", "a".repeat(64)));
        fs::create_dir_all(&native_root)?;
        fs::write(
            native_root.join("libllama.so"),
            b"admitted native library bytes",
        )?;
        let workspace = root.join("workspace");
        fs::create_dir(&workspace)?;
        let workspace_secret = workspace.join("secret.txt");
        fs::write(&workspace_secret, b"workspace must remain private")?;
        let daemon_state = root.join("daemon-state");
        fs::create_dir(&daemon_state)?;
        let daemon_db = daemon_state.join("agentlibre.sqlite3");
        fs::write(&daemon_db, b"daemon DB must remain private")?;
        let allowed_symlink = root.join("model-link.gguf");
        symlink(&model, &allowed_symlink)?;
        let pty = PseudoTerminal::open()?;

        Ok(Self {
            root,
            model,
            projector,
            native_root,
            workspace_secret,
            daemon_db,
            pty,
            private_temp,
            bad_temp,
            allowed_symlink,
        })
    }
}

struct PseudoTerminal {
    _master: OwnedFd,
    path: PathBuf,
}

impl PseudoTerminal {
    fn open() -> io::Result<Self> {
        let descriptor =
            unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        let master = unsafe { OwnedFd::from_raw_fd(descriptor) };
        if unsafe { libc::grantpt(master.as_raw_fd()) } != 0
            || unsafe { libc::unlockpt(master.as_raw_fd()) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut path = [0_i8; 256];
        if unsafe { libc::ptsname_r(master.as_raw_fd(), path.as_mut_ptr(), path.len()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let path = unsafe { std::ffi::CStr::from_ptr(path.as_ptr()) };
        Ok(Self {
            _master: master,
            path: PathBuf::from(path.to_string_lossy().into_owned()),
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
