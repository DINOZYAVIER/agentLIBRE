mod launcher;
mod sandbox;

const SANDBOX_HOME: &str = "/.agl-private/home";
const SANDBOX_TMP: &str = "/tmp";

use std::fs::{self, File, OpenOptions};
use std::io::{Seek as _, SeekFrom};
#[cfg(feature = "native-test-fixtures")]
use std::os::fd::RawFd;
use std::os::fd::{AsRawFd, FromRawFd as _, OwnedFd};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt};
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::{PrivateLaunchEnvironment, ProcessPlatformDiagnostics, wire};
use agl_exec::{ExecutionIo, ProcessError, ProcessErrorCode, Result};

use super::{LauncherDiagnosticsEnvelope, LauncherRequest, LauncherResponse};

const LAUNCHER_PARENT_PID_ENV: &str = "AGL_PROCESS_LAUNCH_PARENT_PID";

#[cfg(feature = "native-test-fixtures")]
const PRE_EXEC_READY_FD_ENV: &str = "AGL_PROCESS_TEST_PRE_EXEC_READY_FD";
#[cfg(feature = "native-test-fixtures")]
const PRE_EXEC_RELEASE_FD_ENV: &str = "AGL_PROCESS_TEST_PRE_EXEC_RELEASE_FD";
#[cfg(feature = "native-test-fixtures")]
const PRE_EXEC_RELEASE_WRITER_FD_ENV: &str = "AGL_PROCESS_TEST_PRE_EXEC_RELEASE_WRITER_FD";

#[doc(hidden)]
pub struct LaunchedProcess {
    pub child: Child,
    pub stdin: Option<OwnedFd>,
    pub stdout: Option<OwnedFd>,
    pub stderr: Option<OwnedFd>,
    pub terminal: Option<OwnedFd>,
}

pub(crate) fn launch(
    launcher_path: &Path,
    request: &LauncherRequest,
    private_environment: Option<PrivateLaunchEnvironment>,
    shell_integration_relay: Option<OwnedFd>,
    cancelled: &AtomicBool,
) -> Result<LaunchedProcess> {
    require_launcher(launcher_path)?;
    let cwd = open_working_directory(&request.request.cwd)?;
    let program = open_program(&request.request)?;
    let private_environment = private_environment_transport(private_environment)?;
    let (parent_socket, child_socket) = wire::socket_pair()?;
    clear_close_on_exec(child_socket.as_raw_fd())?;
    let expected_parent = unsafe { libc::getpid() };
    #[cfg(feature = "native-test-fixtures")]
    let pre_exec_barrier = pre_exec_test_barrier()?;
    let mut command = Command::new(launcher_path);
    command
        .env_clear()
        .env(
            "AGL_PROCESS_LAUNCH_FD",
            child_socket.as_raw_fd().to_string(),
        )
        .env(LAUNCHER_PARENT_PID_ENV, expected_parent.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: after fork this closure performs only raw close/write/read,
    // prctl, and getppid syscalls. All environment parsing and formatting is
    // completed in the parent before Command::spawn invokes the closure.
    unsafe {
        command.pre_exec(move || {
            #[cfg(feature = "native-test-fixtures")]
            if let Some(barrier) = pre_exec_barrier {
                barrier.wait_for_parent_exit()?;
            }
            arm_parent_death_before_exec(expected_parent)
        });
    }
    let mut child = command.spawn().map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::LauncherUnavailable,
            format!("failed to start the process launcher: {error}"),
        )
    })?;
    drop(child_socket);

    let mut descriptors = vec![cwd.as_raw_fd(), program.as_raw_fd()];
    if let Some(private_environment) = &private_environment {
        descriptors.push(private_environment.as_raw_fd());
    }
    if let Some(shell_integration_relay) = &shell_integration_relay {
        descriptors.push(shell_integration_relay.as_raw_fd());
    }
    if let Err(error) = wire::send_json_with_fds(parent_socket.as_raw_fd(), request, &descriptors) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    drop(descriptors);
    drop(private_environment);
    drop(shell_integration_relay);
    if let Err(error) = wait_for_launcher_response(
        parent_socket.as_raw_fd(),
        Duration::from_millis(request.setup_timeout_ms),
        cancelled,
    ) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let (response, mut descriptors) =
        match wire::receive_json_with_fds::<LauncherResponse>(parent_socket.as_raw_fd(), 4) {
            Ok(response) => response,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
    if let Err(error) = response.validate_launcher_identity() {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    if !response.ok {
        let _ = child.wait();
        return Err(ProcessError::new(
            response
                .error_code
                .as_deref()
                .and_then(parse_error_code)
                .unwrap_or(ProcessErrorCode::SpawnFailed),
            response
                .message
                .unwrap_or_else(|| "process launcher rejected the request".to_owned()),
        ));
    }
    let io = match response.io {
        Some(io) => io,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ProcessError::new(
                ProcessErrorCode::LauncherProtocol,
                "successful process launcher response omitted the I/O mode",
            ));
        }
    };
    for descriptor in &descriptors {
        if let Err(error) = set_nonblocking(descriptor.as_raw_fd()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    }
    let (stdin, stdout, stderr, terminal) = match io {
        ExecutionIo::Pipes if descriptors.len() == 3 => (
            Some(descriptors.remove(0)),
            Some(descriptors.remove(0)),
            Some(descriptors.remove(0)),
            None,
        ),
        ExecutionIo::Pty if descriptors.len() == 1 => {
            (None, None, None, Some(descriptors.remove(0)))
        }
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ProcessError::new(
                ProcessErrorCode::LauncherProtocol,
                "process launcher returned the wrong descriptor layout",
            ));
        }
    };
    Ok(LaunchedProcess {
        child,
        stdin,
        stdout,
        stderr,
        terminal,
    })
}

pub(super) fn private_environment_transport(
    private_environment: Option<PrivateLaunchEnvironment>,
) -> Result<Option<OwnedFd>> {
    let Some(private_environment) = private_environment else {
        return Ok(None);
    };
    if private_environment.is_empty() {
        return Ok(None);
    }

    let descriptor = unsafe {
        libc::memfd_create(
            c"agl-private-terminal-environment".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if descriptor < 0 {
        return Err(private_environment_transport_error(
            "failed to create private terminal environment transport",
        ));
    }
    let mut file = unsafe { File::from_raw_fd(descriptor) };
    private_environment.write_launch_transport(&mut file)?;
    drop(private_environment);
    file.seek(SeekFrom::Start(0)).map_err(|_| {
        private_environment_transport_error("failed to rewind private terminal environment")
    })?;
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
        return Err(private_environment_transport_error(
            "failed to seal private terminal environment transport",
        ));
    }
    Ok(Some(file.into()))
}

fn private_environment_transport_error(context: &str) -> ProcessError {
    ProcessError::new(
        ProcessErrorCode::LauncherProtocol,
        format!("{context}: {}", std::io::Error::last_os_error()),
    )
}

fn wait_for_launcher_response(
    descriptor: libc::c_int,
    setup_timeout: Duration,
    cancelled: &AtomicBool,
) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(setup_timeout)
        .unwrap_or_else(Instant::now);
    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(ProcessError::new(
                ProcessErrorCode::Cancelled,
                "process admission was cancelled during launcher setup",
            ));
        }
        if Instant::now() >= deadline {
            return Err(ProcessError::new(
                ProcessErrorCode::SpawnFailed,
                "process launcher did not complete setup before its admitted timeout",
            ));
        }
        let mut poll = libc::pollfd {
            fd: descriptor,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, 10) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(ProcessError::new(
                ProcessErrorCode::LauncherProtocol,
                format!("failed to wait for process launcher response: {error}"),
            ));
        }
        if ready > 0 && poll.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            return Ok(());
        }
    }
}

fn open_program(request: &agl_exec::ExecutionRequest) -> Result<OwnedFd> {
    let mut options = OpenOptions::new();
    options.read(true);
    let mut flags = libc::O_CLOEXEC | libc::O_NOFOLLOW;
    if request.program_digest.is_none() {
        flags |= libc::O_PATH;
    }
    let file = options
        .custom_flags(flags)
        .open(&request.program)
        .map_err(|error| {
            ProcessError::new(
                if request.profile == agl_exec::ExecutionProfile::Workspace {
                    ProcessErrorCode::SandboxExecutableUnavailable
                } else {
                    ProcessErrorCode::SpawnFailed
                },
                format!("admitted executable is no longer available: {error}"),
            )
        })?;
    let metadata = file.metadata().map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::SpawnFailed,
            format!("failed to inspect admitted executable: {error}"),
        )
    })?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(ProcessError::new(
            ProcessErrorCode::SpawnFailed,
            "admitted program is not a regular executable",
        ));
    }
    Ok(file.into())
}

fn open_working_directory(path: &Path) -> Result<OwnedFd> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_DIRECTORY)
        .open(path)
        .map(Into::into)
        .map_err(|error| {
            ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                format!("admitted working directory is no longer available: {error}"),
            )
        })
}

pub(crate) fn diagnostics(launcher_path: &Path) -> ProcessPlatformDiagnostics {
    read_launcher_diagnostics(launcher_path).unwrap_or_else(unavailable_diagnostics)
}

pub(crate) fn verify_launcher_identity(launcher_path: &Path) -> Result<()> {
    read_launcher_diagnostics(launcher_path).map(|_| ())
}

fn read_launcher_diagnostics(launcher_path: &Path) -> Result<ProcessPlatformDiagnostics> {
    require_launcher(launcher_path)?;
    let output = Command::new(launcher_path)
        .env_clear()
        .env("AGL_PROCESS_LAUNCH_DOCTOR", "1")
        .output()
        .map_err(|error| {
            ProcessError::new(
                ProcessErrorCode::LauncherUnavailable,
                format!("failed to run launcher diagnostics: {error}"),
            )
        })?;
    let envelope =
        serde_json::from_slice::<LauncherDiagnosticsEnvelope>(&output.stdout).map_err(|error| {
            ProcessError::new(
                ProcessErrorCode::LauncherProtocol,
                format!("launcher diagnostics were invalid: {error}"),
            )
        })?;
    envelope.validate_identity()?;
    if output.status.success() != envelope.diagnostics.supported {
        return Err(ProcessError::new(
            ProcessErrorCode::LauncherProtocol,
            "launcher diagnostics status disagrees with its supported field",
        ));
    }
    Ok(envelope.diagnostics)
}

pub(crate) fn launcher_main() -> i32 {
    launcher::main()
}

fn require_launcher(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path).map_err(|error| {
        ProcessError::new(
            ProcessErrorCode::LauncherUnavailable,
            format!(
                "process launcher {} is unavailable: {error}",
                path.display()
            ),
        )
    })?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(ProcessError::new(
            ProcessErrorCode::LauncherUnavailable,
            format!("process launcher {} is not executable", path.display()),
        ));
    }
    Ok(())
}

fn arm_parent_death_before_exec(expected_parent: libc::pid_t) -> std::io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::getppid() } != expected_parent {
        return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
    }
    Ok(())
}

#[cfg(feature = "native-test-fixtures")]
#[derive(Clone, Copy)]
struct PreExecTestBarrier {
    ready: RawFd,
    release: RawFd,
    release_writer: RawFd,
}

#[cfg(feature = "native-test-fixtures")]
impl PreExecTestBarrier {
    fn wait_for_parent_exit(self) -> std::io::Result<()> {
        if unsafe { libc::close(self.release_writer) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let byte = [1u8];
        loop {
            let written = unsafe { libc::write(self.ready, byte.as_ptr().cast(), byte.len()) };
            if written == 1 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if written < 0 && error.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(if written < 0 {
                error
            } else {
                std::io::Error::from_raw_os_error(libc::EIO)
            });
        }
        loop {
            let mut release = [0u8];
            let read = unsafe { libc::read(self.release, release.as_mut_ptr().cast(), 1) };
            if read >= 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(error);
            }
        }
        unsafe {
            libc::close(self.ready);
            libc::close(self.release);
        }
        Ok(())
    }
}

#[cfg(feature = "native-test-fixtures")]
fn pre_exec_test_barrier() -> Result<Option<PreExecTestBarrier>> {
    let values = [
        std::env::var_os(PRE_EXEC_READY_FD_ENV),
        std::env::var_os(PRE_EXEC_RELEASE_FD_ENV),
        std::env::var_os(PRE_EXEC_RELEASE_WRITER_FD_ENV),
    ];
    if values.iter().all(Option::is_none) {
        return Ok(None);
    }
    if values.iter().any(Option::is_none) {
        return Err(ProcessError::new(
            ProcessErrorCode::LauncherProtocol,
            "native pre-exec barrier descriptors must be supplied together",
        ));
    }
    let mut descriptors = values.into_iter().map(|value| {
        value
            .and_then(|value| value.to_str().and_then(|value| value.parse::<RawFd>().ok()))
            .filter(|descriptor| *descriptor >= 0)
            .ok_or_else(|| {
                ProcessError::new(
                    ProcessErrorCode::LauncherProtocol,
                    "native pre-exec barrier descriptor is invalid",
                )
            })
    });
    let barrier = PreExecTestBarrier {
        ready: descriptors.next().expect("three barrier values")?,
        release: descriptors.next().expect("three barrier values")?,
        release_writer: descriptors.next().expect("three barrier values")?,
    };
    if barrier.ready == barrier.release
        || barrier.ready == barrier.release_writer
        || barrier.release == barrier.release_writer
    {
        return Err(ProcessError::new(
            ProcessErrorCode::LauncherProtocol,
            "native pre-exec barrier descriptors must be distinct",
        ));
    }
    Ok(Some(barrier))
}

fn clear_close_on_exec(fd: libc::c_int) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(last_os_error(
            ProcessErrorCode::LauncherProtocol,
            "failed to admit the launcher control descriptor",
        ));
    }
    Ok(())
}

fn set_nonblocking(fd: libc::c_int) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(last_os_error(
            ProcessErrorCode::LauncherProtocol,
            "failed to make a launcher descriptor nonblocking",
        ));
    }
    Ok(())
}

pub(super) fn last_os_error(code: ProcessErrorCode, context: &str) -> ProcessError {
    ProcessError::new(
        code,
        format!("{context}: {}", std::io::Error::last_os_error()),
    )
}

fn unavailable_diagnostics(error: ProcessError) -> ProcessPlatformDiagnostics {
    ProcessPlatformDiagnostics {
        platform: "linux".to_owned(),
        supported: false,
        launcher: error.code() != ProcessErrorCode::LauncherUnavailable,
        user_namespace: false,
        pid_namespace: false,
        mount_namespace: false,
        network_namespace: false,
        landlock_abi: None,
        seccomp: false,
        pidfd: false,
        pty: false,
        error_code: Some(error.code().as_str().to_owned()),
        remediation: Some(error.message().to_owned()),
    }
}

fn parse_error_code(value: &str) -> Option<ProcessErrorCode> {
    [
        ProcessErrorCode::PlatformUnsupported,
        ProcessErrorCode::LauncherUnavailable,
        ProcessErrorCode::LauncherProtocol,
        ProcessErrorCode::SandboxUnavailable,
        ProcessErrorCode::SandboxExecutableUnavailable,
        ProcessErrorCode::HostAuthorityRequired,
        ProcessErrorCode::LoginAuthorityRequired,
        ProcessErrorCode::Cancelled,
        ProcessErrorCode::TimedOut,
        ProcessErrorCode::ActiveLimitReached,
        ProcessErrorCode::SpawnFailed,
        ProcessErrorCode::InvalidRequest,
        ProcessErrorCode::InvalidBytes,
        ProcessErrorCode::InputTooLarge,
        ProcessErrorCode::InputBackpressure,
        ProcessErrorCode::InvalidTerminalSize,
        ProcessErrorCode::ExecutionNotFound,
        ProcessErrorCode::ExecutionNotOwned,
        ProcessErrorCode::ExecutionNotLive,
        ProcessErrorCode::IoModeMismatch,
        ProcessErrorCode::InputLeaseBusy,
        ProcessErrorCode::OutputExpired,
        ProcessErrorCode::OutputLimitExceeded,
        ProcessErrorCode::SupervisorShutdown,
        ProcessErrorCode::StateConflict,
        ProcessErrorCode::StoreCorrupt,
        ProcessErrorCode::Internal,
    ]
    .into_iter()
    .find(|code| code.as_str() == value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_response_wait_is_cancellation_aware() {
        let (waiting, _peer) = wire::socket_pair().unwrap();
        let cancelled = AtomicBool::new(true);

        let error =
            wait_for_launcher_response(waiting.as_raw_fd(), Duration::from_secs(30), &cancelled)
                .unwrap_err();

        assert_eq!(error.code(), ProcessErrorCode::Cancelled);
    }

    #[test]
    fn mismatched_launcher_response_payload_is_rejected_on_the_private_wire() {
        let (sender, receiver) = wire::socket_pair().unwrap();
        let response = LauncherResponse {
            protocol_version: "agl-process-launcher.future/test".to_owned(),
            build_id: super::super::LAUNCHER_BUILD_ID.to_owned(),
            ok: false,
            io: None,
            error_code: None,
            message: None,
        };
        wire::send_json_with_fds(sender.as_raw_fd(), &response, &[]).unwrap();
        let (response, descriptors) =
            wire::receive_json_with_fds::<LauncherResponse>(receiver.as_raw_fd(), 0).unwrap();

        assert!(descriptors.is_empty());
        let error = response.validate_launcher_identity().unwrap_err();
        assert_eq!(error.code(), ProcessErrorCode::LauncherProtocol);
        assert!(error.message().contains("protocol mismatch"));
    }

    #[test]
    fn matching_protocol_with_mismatched_launcher_build_is_rejected_on_the_private_wire() {
        let (sender, receiver) = wire::socket_pair().unwrap();
        let response = LauncherResponse {
            protocol_version: super::super::LAUNCHER_PROTOCOL_VERSION.to_owned(),
            build_id: "sha256:stale-launcher-build".to_owned(),
            ok: false,
            io: None,
            error_code: None,
            message: None,
        };
        wire::send_json_with_fds(sender.as_raw_fd(), &response, &[]).unwrap();
        let (response, descriptors) =
            wire::receive_json_with_fds::<LauncherResponse>(receiver.as_raw_fd(), 0).unwrap();

        assert!(descriptors.is_empty());
        let error = response.validate_launcher_identity().unwrap_err();
        assert_eq!(error.code(), ProcessErrorCode::LauncherProtocol);
        assert!(error.message().contains("build identity mismatch"));
    }

    #[test]
    fn stale_launcher_response_without_a_version_is_not_deserializable() {
        let stale = serde_json::json!({
            "ok": false,
            "io": null,
            "error_code": "launcher_protocol",
            "message": "stale launcher"
        });

        assert!(serde_json::from_value::<LauncherResponse>(stale).is_err());
    }

    #[test]
    fn stale_launcher_response_without_a_build_id_is_not_deserializable() {
        let mut stale = serde_json::to_value(LauncherResponse {
            protocol_version: super::super::LAUNCHER_PROTOCOL_VERSION.to_owned(),
            build_id: super::super::LAUNCHER_BUILD_ID.to_owned(),
            ok: false,
            io: None,
            error_code: None,
            message: None,
        })
        .unwrap();
        stale.as_object_mut().unwrap().remove("build_id").unwrap();

        assert!(serde_json::from_value::<LauncherResponse>(stale).is_err());
    }

    #[test]
    fn matching_protocol_with_mismatched_diagnostics_build_is_rejected() {
        let diagnostics = unavailable_diagnostics(ProcessError::new(
            ProcessErrorCode::PlatformUnsupported,
            "test diagnostics",
        ));
        let mut envelope = LauncherDiagnosticsEnvelope::current(diagnostics);
        envelope.build_id = "sha256:stale-launcher-build".to_owned();

        let error = envelope.validate_identity().unwrap_err();
        assert_eq!(error.code(), ProcessErrorCode::LauncherProtocol);
        assert!(error.message().contains("build identity mismatch"));
    }

    #[test]
    fn stale_launcher_diagnostics_without_a_build_id_are_not_deserializable() {
        let diagnostics = unavailable_diagnostics(ProcessError::new(
            ProcessErrorCode::PlatformUnsupported,
            "test diagnostics",
        ));
        let mut stale =
            serde_json::to_value(LauncherDiagnosticsEnvelope::current(diagnostics)).unwrap();
        stale.as_object_mut().unwrap().remove("build_id").unwrap();

        assert!(serde_json::from_value::<LauncherDiagnosticsEnvelope>(stale).is_err());
    }
}
