use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::File;
use std::io::Read as _;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};

use crate::terminal::environment::PrivateTerminalEnvironment;
use crate::{
    ExecutionIo, ExecutionProfile, ProcessError, ProcessErrorCode, ProcessPlatformDiagnostics,
    Result, TerminalSize,
};

use super::super::{LauncherRequest, LauncherResponse};
use super::{SANDBOX_HOME, SANDBOX_TMP, last_os_error, sandbox, wire};

static FORWARDED_SIGNAL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

pub(super) fn main() -> i32 {
    if std::env::var_os("AGL_PROCESS_LAUNCH_DOCTOR").is_some() {
        return doctor_main();
    }
    match launch_main() {
        Ok(status) => mirror_wait_status(status),
        Err(error) => {
            eprintln!("{}: {}", error.code().as_str(), error.message());
            125
        }
    }
}

fn doctor_main() -> i32 {
    let diagnostics = sandbox::diagnostics();
    match serde_json::to_writer(std::io::stdout(), &diagnostics) {
        Ok(()) if diagnostics.supported => 0,
        Ok(()) => 1,
        Err(error) => {
            eprintln!("failed to render process diagnostics: {error}");
            1
        }
    }
}

fn launch_main() -> Result<i32> {
    let expected_parent = unsafe { libc::getppid() };
    configure_parent_death(expected_parent)?;
    let control_fd = parse_control_fd()?;
    let (request, mut descriptors): (LauncherRequest, Vec<OwnedFd>) =
        wire::receive_json_with_fds(control_fd, 3)?;
    if !(2..=3).contains(&descriptors.len()) {
        return send_failure_and_return(
            control_fd,
            ProcessError::new(
                ProcessErrorCode::LauncherProtocol,
                "process launcher requires admitted working-directory and executable handles plus at most one private environment handle",
            ),
        );
    }
    let cwd = descriptors.remove(0);
    let program = descriptors.remove(0);
    let mut private_environment = descriptors.pop();
    if let Err(error) = request.request.validate() {
        return send_failure_and_return(control_fd, error);
    }
    if let Err(error) = sandbox::validate_executable_admission(&request) {
        return send_failure_and_return(control_fd, error);
    }
    if let Err(error) = verify_program_digest(&request, &program) {
        return send_failure_and_return(control_fd, error);
    }
    if let Err(error) = validate_private_directories(&request) {
        return send_failure_and_return(control_fd, error);
    }

    let mut io = match PreparedIo::new(request.request.io, request.request.terminal_size) {
        Ok(io) => io,
        Err(error) => return send_failure_and_return(control_fd, error),
    };
    let (exec_read, exec_write) = match pipe_pair() {
        Ok(pipe) => pipe,
        Err(error) => return send_failure_and_return(control_fd, error),
    };
    if let Err(error) = sandbox::enter_namespaces(request.request.profile) {
        return send_failure_and_return(control_fd, error);
    }

    let namespace_pid = unsafe { libc::fork() };
    if namespace_pid < 0 {
        return send_failure_and_return(
            control_fd,
            last_os_error(
                ProcessErrorCode::SpawnFailed,
                "failed to fork namespace init",
            ),
        );
    }
    if namespace_pid == 0 {
        unsafe { libc::close(control_fd) };
        drop(exec_read);
        io.close_supervisor_side();
        let parent = unsafe { libc::getppid() };
        if let Err(error) = configure_parent_death(parent) {
            write_exec_failure(exec_write.as_raw_fd(), &error);
            unsafe { libc::_exit(125) }
        }
        let status = namespace_init(
            &request,
            &mut io,
            exec_write,
            &cwd,
            &program,
            private_environment.take(),
        );
        unsafe { libc::_exit(mirror_wait_status(status)) }
    }

    drop(private_environment);
    drop(exec_write);
    io.close_target_side();
    let setup = await_exec_handshake(&exec_read, Duration::from_millis(request.setup_timeout_ms));
    match setup {
        Ok(()) => {
            let response = LauncherResponse {
                ok: true,
                io: Some(request.request.io),
                error_code: None,
                message: None,
            };
            wire::send_json_with_fds(control_fd, &response, &io.supervisor_raw_fds())?;
            io.close_supervisor_side();
            unsafe { libc::close(control_fd) };
            install_signal_forwarders();
            wait_for_pid(namespace_pid)
        }
        Err(error) => {
            unsafe { libc::kill(namespace_pid, libc::SIGKILL) };
            let _ = wait_for_pid(namespace_pid);
            send_failure_and_return(control_fd, error)
        }
    }
}

fn namespace_init(
    request: &LauncherRequest,
    io: &mut PreparedIo,
    exec_write: OwnedFd,
    cwd: &OwnedFd,
    program: &OwnedFd,
    mut private_environment: Option<OwnedFd>,
) -> i32 {
    if let Err(error) = sandbox::prepare_pid_namespace(request) {
        write_exec_failure(exec_write.as_raw_fd(), &error);
        return exit_status(125);
    }
    let root = if request.request.profile == ExecutionProfile::Workspace {
        Some(request.execution_root.join("rootfs"))
    } else {
        None
    };
    install_signal_forwarders();
    let target_pid = unsafe { libc::fork() };
    if target_pid < 0 {
        let error = last_os_error(
            ProcessErrorCode::SpawnFailed,
            "failed to fork process target",
        );
        write_exec_failure(exec_write.as_raw_fd(), &error);
        return exit_status(125);
    }
    if target_pid == 0 {
        if let Err(error) = target_main(
            request,
            io,
            exec_write.as_raw_fd(),
            root.as_deref(),
            cwd,
            program,
            private_environment.take(),
        ) {
            write_exec_failure(exec_write.as_raw_fd(), &error);
            unsafe { libc::_exit(126) }
        }
        unreachable!("execve returned success");
    }
    drop(private_environment);
    drop(exec_write);
    io.close_target_side();
    reap_namespace(target_pid)
}

fn target_main(
    request: &LauncherRequest,
    io: &mut PreparedIo,
    exec_error_fd: RawFd,
    root: Option<&Path>,
    cwd: &OwnedFd,
    program: &OwnedFd,
    private_environment: Option<OwnedFd>,
) -> Result<()> {
    if unsafe { libc::setsid() } < 0 {
        return Err(last_os_error(
            ProcessErrorCode::SpawnFailed,
            "failed to create the target session",
        ));
    }
    io.attach_target(request.request.terminal_size)?;
    let mut preserved = vec![
        libc::STDIN_FILENO,
        libc::STDOUT_FILENO,
        libc::STDERR_FILENO,
        exec_error_fd,
        cwd.as_raw_fd(),
        program.as_raw_fd(),
    ];
    if let Some(private_environment) = &private_environment {
        preserved.push(private_environment.as_raw_fd());
    }
    close_descriptors_except(&preserved);
    // Execute the identity-checked descriptor reopened inside the target mount
    // view. The pre-admission descriptor points at the host mount hierarchy,
    // which Landlock correctly refuses after the sandbox ruleset is active.
    let target_program =
        sandbox::enter_target(request, root, cwd.as_raw_fd(), program.as_raw_fd())?;
    let mut preserved = vec![
        libc::STDIN_FILENO,
        libc::STDOUT_FILENO,
        libc::STDERR_FILENO,
        exec_error_fd,
        target_program.as_raw_fd(),
    ];
    if let Some(private_environment) = &private_environment {
        preserved.push(private_environment.as_raw_fd());
    }
    close_descriptors_except(&preserved);
    exec_target(request, target_program.as_raw_fd(), private_environment)
}

fn exec_target(
    request: &LauncherRequest,
    program_fd: RawFd,
    private_environment: Option<OwnedFd>,
) -> Result<()> {
    let program = CString::new(request.request.program.as_os_str().as_bytes()).map_err(|_| {
        ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "program path contains NUL",
        )
    })?;
    let mut argv = Vec::with_capacity(request.request.args.len() + 1);
    argv.push(program.clone());
    for argument in &request.request.args {
        argv.push(CString::new(argument.as_bytes()).map_err(|_| {
            ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "process argument contains NUL",
            )
        })?);
    }
    let private_environment = match private_environment {
        Some(descriptor) => {
            let mut file = File::from(descriptor);
            PrivateTerminalEnvironment::read_launch_transport(&mut file)?
        }
        None => PrivateTerminalEnvironment::default(),
    };
    let environment = encode_environment(request, &private_environment)?;
    drop(private_environment);
    let mut argv_ptrs = argv.iter().map(|value| value.as_ptr()).collect::<Vec<_>>();
    argv_ptrs.push(std::ptr::null());
    let mut env_ptrs = environment
        .iter()
        .map(EncodedEnvironmentValue::as_ptr)
        .collect::<Vec<_>>();
    env_ptrs.push(std::ptr::null());
    let empty = c"";
    let mut result = unsafe {
        libc::syscall(
            libc::SYS_execveat,
            program_fd,
            empty.as_ptr(),
            argv_ptrs.as_ptr(),
            env_ptrs.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    };
    if result != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
        clear_close_on_exec(program_fd)?;
        result = unsafe {
            libc::syscall(
                libc::SYS_execveat,
                program_fd,
                empty.as_ptr(),
                argv_ptrs.as_ptr(),
                env_ptrs.as_ptr(),
                libc::AT_EMPTY_PATH,
            )
        };
    }
    debug_assert_ne!(result, 0, "execveat returned without replacing the target");
    let code = if request.request.profile == ExecutionProfile::Workspace {
        ProcessErrorCode::SandboxExecutableUnavailable
    } else {
        ProcessErrorCode::SpawnFailed
    };
    Err(last_os_error(
        code,
        "failed to execute the admitted program",
    ))
}

fn verify_program_digest(request: &LauncherRequest, program: &OwnedFd) -> Result<()> {
    let Some(expected) = request.request.program_digest.as_deref() else {
        return Ok(());
    };
    let duplicate = unsafe { libc::dup(program.as_raw_fd()) };
    if duplicate < 0 {
        return Err(last_os_error(
            ProcessErrorCode::SandboxExecutableUnavailable,
            "failed to inspect the frozen shell executable",
        ));
    }
    let mut file = unsafe { File::from_raw_fd(duplicate) };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::SandboxExecutableUnavailable,
            format!("failed to read the frozen shell executable: {error}"),
        )
    })?;
    let mut actual = String::with_capacity(71);
    actual.push_str("sha256:");
    use std::fmt::Write as _;
    for byte in Sha256::digest(bytes) {
        write!(&mut actual, "{byte:02x}").expect("writing to a String cannot fail");
    }
    if actual != expected {
        return Err(ProcessError::new(
            ProcessErrorCode::SandboxExecutableUnavailable,
            "frozen shell executable identity changed before launcher admission",
        ));
    }
    Ok(())
}

fn clear_close_on_exec(descriptor: RawFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0
    {
        return Err(last_os_error(
            ProcessErrorCode::SpawnFailed,
            "failed to admit a script executable descriptor",
        ));
    }
    Ok(())
}

fn encode_environment(
    request: &LauncherRequest,
    private_environment: &PrivateTerminalEnvironment,
) -> Result<Vec<EncodedEnvironmentValue>> {
    let mut values = request
        .request
        .environment
        .values
        .iter()
        .map(|(name, value)| (name.clone(), TargetEnvironmentValue::Public(value.as_str())))
        .collect::<BTreeMap<_, _>>();
    for (name, value) in private_environment.exposed_values() {
        values.insert(name.to_owned(), TargetEnvironmentValue::Private(value));
    }
    values.insert(
        "PWD".to_owned(),
        TargetEnvironmentValue::Owned(
            request
                .request
                .cwd
                .as_os_str()
                .to_string_lossy()
                .into_owned(),
        ),
    );
    if request.request.profile == ExecutionProfile::Workspace {
        values.insert(
            "HOME".to_owned(),
            TargetEnvironmentValue::Public(SANDBOX_HOME),
        );
        values.insert(
            "TMPDIR".to_owned(),
            TargetEnvironmentValue::Public(SANDBOX_TMP),
        );
        values
            .entry("TERM".to_owned())
            .or_insert(TargetEnvironmentValue::Public("xterm-256color"));
    }
    values
        .into_iter()
        .map(|(name, value)| EncodedEnvironmentValue::new(&name, value))
        .collect()
}

enum TargetEnvironmentValue<'a> {
    Public(&'a str),
    Private(&'a str),
    Owned(String),
}

impl TargetEnvironmentValue<'_> {
    fn value(&self) -> &str {
        match self {
            Self::Public(value) | Self::Private(value) => value,
            Self::Owned(value) => value,
        }
    }

    fn is_private(&self) -> bool {
        matches!(self, Self::Private(_))
    }
}

struct EncodedEnvironmentValue {
    value: CString,
    private: bool,
}

impl EncodedEnvironmentValue {
    fn new(name: &str, value: TargetEnvironmentValue<'_>) -> Result<Self> {
        let private = value.is_private();
        let mut encoded = Vec::with_capacity(name.len() + value.value().len() + 1);
        encoded.extend_from_slice(name.as_bytes());
        encoded.push(b'=');
        encoded.extend_from_slice(value.value().as_bytes());
        let value = match CString::new(encoded) {
            Ok(value) => value,
            Err(error) => {
                let mut encoded = error.into_vec();
                if private {
                    crate::terminal::environment::zeroize(&mut encoded);
                }
                return Err(ProcessError::new(
                    ProcessErrorCode::InvalidRequest,
                    "environment name or value contains NUL",
                ));
            }
        };
        Ok(Self { value, private })
    }

    fn as_ptr(&self) -> *const libc::c_char {
        self.value.as_ptr()
    }
}

impl Drop for EncodedEnvironmentValue {
    fn drop(&mut self) {
        if !self.private {
            return;
        }
        for offset in 0..self.value.as_bytes().len() {
            // SAFETY: this value is exclusively borrowed during destruction;
            // overwriting initialized bytes does not change its allocation.
            unsafe {
                std::ptr::write_volatile(self.value.as_ptr().add(offset).cast_mut(), 0);
            }
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

struct PreparedIo {
    supervisor: Vec<OwnedFd>,
    target: Vec<OwnedFd>,
    mode: ExecutionIo,
}

impl PreparedIo {
    fn new(mode: ExecutionIo, terminal_size: Option<TerminalSize>) -> Result<Self> {
        match mode {
            ExecutionIo::Pipes => {
                let (stdin_read, stdin_write) = pipe_pair()?;
                let (stdout_read, stdout_write) = pipe_pair()?;
                let (stderr_read, stderr_write) = pipe_pair()?;
                Ok(Self {
                    supervisor: vec![stdin_write, stdout_read, stderr_read],
                    target: vec![stdin_read, stdout_write, stderr_write],
                    mode,
                })
            }
            ExecutionIo::Pty => {
                let (master, slave) = pty_pair(terminal_size.unwrap_or_default())?;
                Ok(Self {
                    supervisor: vec![master],
                    target: vec![slave],
                    mode,
                })
            }
        }
    }

    fn supervisor_raw_fds(&self) -> Vec<RawFd> {
        self.supervisor.iter().map(AsRawFd::as_raw_fd).collect()
    }

    fn close_supervisor_side(&mut self) {
        self.supervisor.clear();
    }

    fn close_target_side(&mut self) {
        self.target.clear();
    }

    fn attach_target(&self, size: Option<TerminalSize>) -> Result<()> {
        match self.mode {
            ExecutionIo::Pipes if self.target.len() == 3 => {
                duplicate_to(self.target[0].as_raw_fd(), libc::STDIN_FILENO)?;
                duplicate_to(self.target[1].as_raw_fd(), libc::STDOUT_FILENO)?;
                duplicate_to(self.target[2].as_raw_fd(), libc::STDERR_FILENO)?;
            }
            ExecutionIo::Pty if self.target.len() == 1 => {
                let slave = self.target[0].as_raw_fd();
                if unsafe { libc::ioctl(slave, libc::TIOCSCTTY, 0) } != 0 {
                    return Err(last_os_error(
                        ProcessErrorCode::SpawnFailed,
                        "failed to acquire the controlling terminal",
                    ));
                }
                set_terminal_size(slave, size.unwrap_or_default())?;
                duplicate_to(slave, libc::STDIN_FILENO)?;
                duplicate_to(slave, libc::STDOUT_FILENO)?;
                duplicate_to(slave, libc::STDERR_FILENO)?;
            }
            _ => {
                return Err(ProcessError::new(
                    ProcessErrorCode::LauncherProtocol,
                    "launcher I/O descriptor layout is invalid",
                ));
            }
        }
        Ok(())
    }
}

fn pty_pair(size: TerminalSize) -> Result<(OwnedFd, OwnedFd)> {
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if master < 0 {
        return Err(last_os_error(
            ProcessErrorCode::SpawnFailed,
            "failed to open PTY master",
        ));
    }
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    if unsafe { libc::grantpt(master.as_raw_fd()) } != 0
        || unsafe { libc::unlockpt(master.as_raw_fd()) } != 0
    {
        return Err(last_os_error(
            ProcessErrorCode::SpawnFailed,
            "failed to unlock PTY slave",
        ));
    }
    let mut name = vec![0i8; 4096];
    if unsafe { libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) } != 0 {
        return Err(last_os_error(
            ProcessErrorCode::SpawnFailed,
            "failed to resolve PTY slave",
        ));
    }
    let slave = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if slave < 0 {
        return Err(last_os_error(
            ProcessErrorCode::SpawnFailed,
            "failed to open PTY slave",
        ));
    }
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    set_terminal_size(slave.as_raw_fd(), size)?;
    Ok((master, slave))
}

fn pipe_pair() -> Result<(OwnedFd, OwnedFd)> {
    let mut descriptors = [-1; 2];
    if unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(last_os_error(
            ProcessErrorCode::SpawnFailed,
            "failed to create process pipe",
        ));
    }
    Ok(unsafe {
        (
            OwnedFd::from_raw_fd(descriptors[0]),
            OwnedFd::from_raw_fd(descriptors[1]),
        )
    })
}

fn duplicate_to(source: RawFd, target: RawFd) -> Result<()> {
    if unsafe { libc::dup2(source, target) } < 0 {
        return Err(last_os_error(
            ProcessErrorCode::SpawnFailed,
            "failed to attach process I/O",
        ));
    }
    Ok(())
}

fn set_terminal_size(fd: RawFd, size: TerminalSize) -> Result<()> {
    let size = size.validate()?;
    let dimensions = libc::winsize {
        ws_row: size.rows,
        ws_col: size.columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &dimensions) } != 0 {
        return Err(last_os_error(
            ProcessErrorCode::InvalidTerminalSize,
            "failed to set terminal size",
        ));
    }
    Ok(())
}

fn await_exec_handshake(fd: &OwnedFd, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ProcessError::new(
                ProcessErrorCode::SpawnFailed,
                "process setup exceeded its admitted timeout",
            ));
        }
        let timeout_ms = i32::try_from(remaining.as_millis())
            .unwrap_or(i32::MAX)
            .max(1);
        let mut poll = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, timeout_ms) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(last_os_error(
                ProcessErrorCode::LauncherProtocol,
                "failed while waiting for the target exec handshake",
            ));
        }
        if ready == 0 {
            continue;
        }
        loop {
            let mut buffer = [0u8; 4096];
            let read =
                unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
            if read > 0 {
                bytes.extend_from_slice(&buffer[..read as usize]);
                if bytes.len() > 64 * 1024 {
                    return Err(ProcessError::new(
                        ProcessErrorCode::LauncherProtocol,
                        "target exec failure exceeded the private protocol bound",
                    ));
                }
                continue;
            }
            if read == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(ProcessError::new(
                ProcessErrorCode::LauncherProtocol,
                format!("failed to read the target exec handshake: {error}"),
            ));
        }
        break;
    }
    if bytes.is_empty() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&bytes);
    let (code, message) = message
        .split_once('\0')
        .unwrap_or(("spawn_failed", &message));
    Err(ProcessError::new(
        parse_error_code(code),
        message.to_owned(),
    ))
}

fn write_exec_failure(fd: RawFd, error: &ProcessError) {
    let mut bytes = format!("{}\0{}", error.code().as_str(), error.message()).into_bytes();
    bytes.truncate(64 * 1024);
    let _ = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
}

fn send_failure_and_return(control_fd: RawFd, error: ProcessError) -> Result<i32> {
    let response = LauncherResponse {
        ok: false,
        io: None,
        error_code: Some(error.code().as_str().to_owned()),
        message: Some(error.message().to_owned()),
    };
    wire::send_json_with_fds(control_fd, &response, &[])?;
    Ok(exit_status(125))
}

fn validate_private_directories(request: &LauncherRequest) -> Result<()> {
    for path in [
        &request.execution_root,
        &request.private_home,
        &request.private_tmp,
    ] {
        if !path.is_absolute() || path.as_os_str().as_bytes().contains(&0) {
            return Err(ProcessError::new(
                ProcessErrorCode::LauncherProtocol,
                "launcher private paths must be absolute and contain no NUL",
            ));
        }
    }
    Ok(())
}

fn parse_control_fd() -> Result<RawFd> {
    let value = std::env::var("AGL_PROCESS_LAUNCH_FD").map_err(|_| {
        ProcessError::new(
            ProcessErrorCode::LauncherProtocol,
            "launcher control descriptor is missing",
        )
    })?;
    value.parse::<RawFd>().map_err(|_| {
        ProcessError::new(
            ProcessErrorCode::LauncherProtocol,
            "launcher control descriptor is invalid",
        )
    })
}

fn configure_parent_death(expected_parent: libc::pid_t) -> Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
        return Err(last_os_error(
            ProcessErrorCode::SandboxUnavailable,
            "failed to arm parent-death protection",
        ));
    }
    if unsafe { libc::getppid() } != expected_parent {
        return Err(ProcessError::new(
            ProcessErrorCode::SpawnFailed,
            "launcher parent changed during parent-death setup",
        ));
    }
    Ok(())
}

extern "C" fn forward_signal(signal: libc::c_int) {
    FORWARDED_SIGNAL.store(signal, std::sync::atomic::Ordering::Relaxed);
}

fn install_signal_forwarders() {
    unsafe {
        let handler = forward_signal as *const () as libc::sighandler_t;
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
        libc::signal(libc::SIGHUP, handler);
    }
}

fn reap_namespace(target_pid: libc::pid_t) -> i32 {
    let mut target_status = None;
    loop {
        let signal = FORWARDED_SIGNAL.swap(0, std::sync::atomic::Ordering::Relaxed);
        if signal != 0 {
            unsafe {
                libc::kill(target_pid, signal);
                libc::kill(-target_pid, signal);
            }
        }
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid == target_pid {
            target_status = Some(status);
        }
        if let Some(status) = target_status {
            unsafe { libc::kill(-1, libc::SIGKILL) };
            let mut descendant_status = 0;
            while unsafe { libc::waitpid(-1, &mut descendant_status, 0) } > 0 {}
            return status;
        }
        if pid < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
            return exit_status(125);
        }
        let mut pause = libc::timespec {
            tv_sec: 0,
            tv_nsec: 10_000_000,
        };
        unsafe { libc::nanosleep(&pause, &mut pause) };
    }
}

fn wait_for_pid(pid: libc::pid_t) -> Result<i32> {
    let mut status = 0;
    loop {
        let signal = FORWARDED_SIGNAL.swap(0, std::sync::atomic::Ordering::Relaxed);
        if signal != 0 {
            unsafe { libc::kill(pid, signal) };
        }
        let result = unsafe { libc::waitpid(pid, &mut status, 0) };
        if result == pid {
            return Ok(status);
        }
        if result < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(last_os_error(
            ProcessErrorCode::Internal,
            "failed to reap the namespace init",
        ));
    }
}

fn close_descriptors_except(keep: &[RawFd]) {
    let maximum = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
    let maximum = if maximum <= 0 {
        1024
    } else {
        maximum.min(65_536)
    };
    for fd in 3..maximum as RawFd {
        if !keep.contains(&fd) {
            unsafe { libc::close(fd) };
        }
    }
}

fn exit_status(code: i32) -> i32 {
    code << 8
}

fn mirror_wait_status(status: i32) -> i32 {
    if libc::WIFEXITED(status) {
        return libc::WEXITSTATUS(status);
    }
    if libc::WIFSIGNALED(status) {
        let signal = libc::WTERMSIG(status);
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            libc::raise(signal);
        }
        return 128 + signal;
    }
    125
}

fn parse_error_code(value: &str) -> ProcessErrorCode {
    [
        ProcessErrorCode::SandboxUnavailable,
        ProcessErrorCode::SandboxExecutableUnavailable,
        ProcessErrorCode::SpawnFailed,
        ProcessErrorCode::InvalidRequest,
        ProcessErrorCode::InvalidTerminalSize,
        ProcessErrorCode::LauncherProtocol,
    ]
    .into_iter()
    .find(|code| code.as_str() == value)
    .unwrap_or(ProcessErrorCode::SpawnFailed)
}

#[allow(dead_code)]
fn _diagnostics_type(_: ProcessPlatformDiagnostics) {}

#[cfg(test)]
mod tests {
    use std::os::unix::process::CommandExt as _;
    use std::process::Command;

    use agl_ids::{RunId, SessionId, StepId};

    use super::*;
    use crate::terminal::environment::{
        TerminalEnvironmentRequest, TerminalEnvironmentValue, TerminalSecretReference,
        TerminalSecretResolver, TerminalSecretValue,
    };
    use crate::{
        EnvironmentOverride, ExecutionAuthorization, ExecutionKind, ExecutionLimits, ExecutionOwner,
    };

    const SECRET_NAME: &str = "AGL_TEST_PRIVATE_EXEC_VALUE";
    const SENTINEL: &str = "private-exec-sentinel-9a071c";
    const HELPER_DESCRIPTOR: RawFd = 198;
    const HELPER_DESCRIPTOR_ENV: &str = "AGL_TEST_PRIVATE_ENVIRONMENT_FD";
    const HELPER_TEST: &str = "platform::linux::launcher::tests::private_environment_exec_helper";

    #[test]
    fn final_exec_child_receives_private_environment_outside_the_launcher_dto() {
        struct Secret;
        impl TerminalSecretResolver for Secret {
            fn resolve(&self, _reference: &TerminalSecretReference) -> Result<TerminalSecretValue> {
                TerminalSecretValue::new(SENTINEL)
            }
        }

        let request = exec_request();

        let mut environment = TerminalEnvironmentRequest::default();
        environment.agl_env.insert(
            SECRET_NAME.to_owned(),
            TerminalEnvironmentValue::Secret(
                TerminalSecretReference::new("test:private-exec").unwrap(),
            ),
        );
        let (public, private) = environment.resolve(&Secret).unwrap().into_launch_parts();
        assert!(public.values.is_empty());
        assert!(!format!("{request:?} {private:?}").contains(SENTINEL));
        assert!(!serde_json::to_string(&request).unwrap().contains(SENTINEL));
        let private_descriptor = super::super::private_environment_transport(Some(private))
            .unwrap()
            .unwrap();
        let inherited_descriptor = private_descriptor.as_raw_fd();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg(HELPER_TEST)
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env(HELPER_DESCRIPTOR_ENV, HELPER_DESCRIPTOR.to_string());
        // SAFETY: the pre-exec closure performs only async-signal-safe fcntl
        // and dup2 syscalls and does not allocate or touch shared Rust state.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(inherited_descriptor, HELPER_DESCRIPTOR) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let flags = libc::fcntl(HELPER_DESCRIPTOR, libc::F_GETFD);
                if flags < 0
                    || libc::fcntl(HELPER_DESCRIPTOR, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut output = command.output().unwrap();
        drop(private_descriptor);
        assert!(
            !output
                .stderr
                .windows(SENTINEL.len())
                .any(|window| window == SENTINEL.as_bytes()),
            "private environment value escaped through the launch failure channel"
        );
        assert!(
            output.status.success(),
            "private environment exec helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected = format!("{SECRET_NAME}={SENTINEL}");
        assert!(
            output
                .stdout
                .windows(expected.len())
                .any(|window| window == expected.as_bytes()),
            "final exec child did not receive its private environment entry"
        );
        crate::terminal::environment::zeroize(&mut output.stdout);
        crate::terminal::environment::zeroize(&mut output.stderr);
    }

    #[test]
    fn private_environment_exec_helper() {
        let Some(descriptor) = std::env::var_os(HELPER_DESCRIPTOR_ENV) else {
            return;
        };
        let descriptor = descriptor
            .to_string_lossy()
            .parse::<RawFd>()
            .expect("private environment helper descriptor must be numeric");
        assert_eq!(descriptor, HELPER_DESCRIPTOR);
        // SAFETY: the parent test transfers unique ownership of this inherited
        // descriptor to the helper process at the agreed fixed number.
        let private_environment = unsafe { OwnedFd::from_raw_fd(descriptor) };
        let request = exec_request();
        let program = File::open(&request.request.program).unwrap();
        if let Err(error) = exec_target(&request, program.as_raw_fd(), Some(private_environment)) {
            panic!("private environment exec helper failed: {error}");
        }
        unreachable!("exec_target returned success without replacing the helper");
    }

    fn exec_request() -> LauncherRequest {
        let program = ["/usr/bin/env", "/bin/env"]
            .into_iter()
            .map(Path::new)
            .find(|candidate| candidate.is_file())
            .expect("Linux launcher test requires env(1)")
            .to_path_buf();
        let cwd = std::env::temp_dir().canonicalize().unwrap();
        let run_id = RunId::generate();
        let request = LauncherRequest {
            execution_id: agl_ids::ExecutionId::generate(),
            request: crate::ExecutionRequest {
                owner: ExecutionOwner::Session {
                    session_id: SessionId::generate(),
                    root_run_id: run_id.clone(),
                },
                creating_run_id: run_id,
                creating_step_id: StepId::generate(),
                kind: ExecutionKind::Argv,
                program,
                program_digest: None,
                args: Vec::new(),
                workspace_root: cwd.clone(),
                cwd: cwd.clone(),
                read_only_roots: Vec::new(),
                environment: EnvironmentOverride {
                    values: BTreeMap::from([("PUBLIC_MARKER".to_owned(), "visible".to_owned())]),
                },
                stdin: None,
                close_stdin_after_initial: false,
                io: ExecutionIo::Pipes,
                terminal_size: None,
                profile: ExecutionProfile::Workspace,
                authorization: ExecutionAuthorization::default(),
                grant_lease: None,
                limits: ExecutionLimits {
                    timeout_ms: None,
                    max_input_bytes: 1024,
                    max_output_bytes: 4096,
                },
            },
            execution_root: cwd.join("unused-execution-root"),
            private_home: cwd.join("unused-private-home"),
            private_tmp: cwd.join("unused-private-tmp"),
            setup_timeout_ms: 1_000,
        };
        request.request.validate().unwrap();
        request
    }
}
