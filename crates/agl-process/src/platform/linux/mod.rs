mod launcher;
mod sandbox;
mod wire;

const SANDBOX_HOME: &str = "/.agl-private/home";
const SANDBOX_TMP: &str = "/tmp";

use std::fs::{self, OpenOptions};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::{ExecutionIo, ProcessError, ProcessErrorCode, ProcessPlatformDiagnostics, Result};

use super::{LauncherRequest, LauncherResponse};

pub(crate) struct LaunchedProcess {
    pub child: Child,
    pub stdin: Option<OwnedFd>,
    pub stdout: Option<OwnedFd>,
    pub stderr: Option<OwnedFd>,
    pub terminal: Option<OwnedFd>,
}

pub(crate) fn launch(
    launcher_path: &Path,
    request: &LauncherRequest,
    cancelled: &AtomicBool,
) -> Result<LaunchedProcess> {
    require_launcher(launcher_path)?;
    let cwd = open_working_directory(&request.request.cwd)?;
    let program = open_program(&request.request)?;
    let (parent_socket, child_socket) = wire::socket_pair()?;
    clear_close_on_exec(child_socket.as_raw_fd())?;
    let mut child = Command::new(launcher_path)
        .env_clear()
        .env(
            "AGL_PROCESS_LAUNCH_FD",
            child_socket.as_raw_fd().to_string(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            ProcessError::new(
                ProcessErrorCode::LauncherUnavailable,
                format!("failed to start the process launcher: {error}"),
            )
        })?;
    drop(child_socket);

    if let Err(error) = wire::send_json_with_fds(
        parent_socket.as_raw_fd(),
        request,
        &[cwd.as_raw_fd(), program.as_raw_fd()],
    ) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
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

fn open_program(request: &crate::ExecutionRequest) -> Result<OwnedFd> {
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
                if request.profile == crate::ExecutionProfile::Workspace {
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
    if let Err(error) = require_launcher(launcher_path) {
        return unavailable_diagnostics(error);
    }
    let output = Command::new(launcher_path)
        .env_clear()
        .env("AGL_PROCESS_LAUNCH_DOCTOR", "1")
        .output();
    match output {
        Ok(output) => match serde_json::from_slice::<ProcessPlatformDiagnostics>(&output.stdout) {
            Ok(diagnostics) if output.status.success() == diagnostics.supported => diagnostics,
            Ok(_) => unavailable_diagnostics(ProcessError::new(
                ProcessErrorCode::LauncherProtocol,
                "launcher diagnostics status disagrees with its supported field",
            )),
            Err(error) => unavailable_diagnostics(ProcessError::new(
                ProcessErrorCode::LauncherProtocol,
                format!("launcher diagnostics were invalid: {error}"),
            )),
        },
        Err(error) => unavailable_diagnostics(ProcessError::new(
            ProcessErrorCode::LauncherUnavailable,
            format!("failed to run launcher diagnostics: {error}"),
        )),
    }
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
}
