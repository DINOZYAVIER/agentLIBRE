use std::path::{Path, PathBuf};

use agl_execution_api::{
    ExecutionCommand, ExecutionProtocolError, ExecutionProtocolErrorCode, ExecutionProtocolRequest,
    ExecutionProtocolResponse, ExecutionResponse, MAX_EXECUTION_FRAME_BYTES,
};
use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::{DEFAULT_SOCKET_FILE, ExecutionService};

pub enum ListenerSource {
    Bind(PathBuf),
    Systemd,
}

pub fn default_socket_path(state_root: impl AsRef<Path>) -> PathBuf {
    state_root.as_ref().join("execd").join(DEFAULT_SOCKET_FILE)
}

pub struct ExecutionServer {
    service: ExecutionService,
}

impl ExecutionServer {
    pub fn start(data_root: &Path, launcher: PathBuf) -> Result<Self> {
        Ok(Self {
            service: ExecutionService::start(data_root, launcher)?,
        })
    }

    pub async fn serve(self, source: ListenerSource) -> Result<()> {
        let listener = match source {
            ListenerSource::Bind(path) => bind_listener(&path).await?,
            ListenerSource::Systemd => claim_systemd_listener()?,
        };
        tracing::info!("execution service listening");
        loop {
            let (stream, _) = listener.accept().await?;
            let service = self.service.clone();
            tokio::spawn(async move {
                if let Err(error) = serve_connection(stream, service).await {
                    tracing::warn!(%error, "execution client connection failed");
                }
            });
        }
    }
}

async fn serve_connection(stream: UnixStream, service: ExecutionService) -> Result<()> {
    verify_peer(&stream)?;
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader).take((MAX_EXECUTION_FRAME_BYTES + 2) as u64);
    let mut frame = Vec::new();
    let read = reader.read_until(b'\n', &mut frame).await?;
    if read == 0 {
        return Ok(());
    }
    anyhow::ensure!(frame.ends_with(b"\n"), "invalid execution request frame");
    frame.pop();
    anyhow::ensure!(
        frame.len() <= MAX_EXECUTION_FRAME_BYTES,
        "invalid execution request frame"
    );
    let request: ExecutionProtocolRequest = serde_json::from_slice(&frame)?;
    let request_id = request.request_id.clone();
    let result = match request.validate() {
        Ok(()) => dispatch(service, request.command).await,
        Err(error) => Err(error),
    };
    let response = match result {
        Ok(response) => ExecutionProtocolResponse::success(request_id, response),
        Err(error) => ExecutionProtocolResponse::error(request_id, error),
    };
    let mut bytes = serde_json::to_vec(&response)?;
    anyhow::ensure!(
        bytes.len() <= MAX_EXECUTION_FRAME_BYTES,
        "execution response is oversized"
    );
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    Ok(())
}

async fn dispatch(
    service: ExecutionService,
    command: ExecutionCommand,
) -> Result<ExecutionResponse, ExecutionProtocolError> {
    let result = match command {
        ExecutionCommand::Start { request } => service
            .launch(request)
            .map(|status| ExecutionResponse::Started { status }),
        ExecutionCommand::Inspect { execution_id } => service
            .status(execution_id)
            .map(|status| ExecutionResponse::Status { status }),
        ExecutionCommand::InspectTerminal { terminal_id } => service
            .status_by_terminal(terminal_id)
            .map(|status| ExecutionResponse::Status { status }),
        ExecutionCommand::Read {
            execution_id,
            after,
            max_bytes,
        } => service
            .read(execution_id, after, max_bytes)
            .map(|output| ExecutionResponse::Output { output }),
        ExecutionCommand::Write { terminal_id, data } => service
            .write(terminal_id, &data)
            .map(|()| ExecutionResponse::Acknowledged),
        ExecutionCommand::Resize { terminal_id, size } => service
            .resize(terminal_id, size)
            .map(|()| ExecutionResponse::Acknowledged),
        ExecutionCommand::Signal {
            execution_id,
            signal,
        } => service
            .signal(execution_id, signal)
            .map(|()| ExecutionResponse::Acknowledged),
    };
    result.map_err(map_service_error)
}

fn map_service_error(error: anyhow::Error) -> ExecutionProtocolError {
    use crate::store::ExecutionStoreLookupError;
    use crate::supervisor::ExecutionControlError;

    let (code, retryable) = if error.downcast_ref::<ExecutionStoreLookupError>().is_some() {
        (ExecutionProtocolErrorCode::NotFound, false)
    } else if error
        .downcast_ref::<ExecutionControlError>()
        .is_some_and(|cause| {
            matches!(
                cause,
                ExecutionControlError::InactiveTerminal | ExecutionControlError::InactiveExecution
            )
        })
    {
        (ExecutionProtocolErrorCode::Conflict, false)
    } else if error
        .downcast_ref::<ExecutionControlError>()
        .is_some_and(|cause| matches!(cause, ExecutionControlError::Unavailable))
    {
        (ExecutionProtocolErrorCode::Unavailable, true)
    } else {
        (ExecutionProtocolErrorCode::Internal, false)
    };
    ExecutionProtocolError::new(code, error.to_string(), retryable)
}

async fn bind_listener(path: &Path) -> Result<UnixListener> {
    use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _, PermissionsExt as _};

    anyhow::ensure!(path.is_absolute(), "execd socket path must be absolute");
    let parent = path.parent().context("execd socket path has no parent")?;
    std::fs::create_dir_all(parent)?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        anyhow::ensure!(
            metadata.file_type().is_socket()
                && metadata.uid() == unsafe { libc::geteuid() }
                && UnixStream::connect(path).await.is_err(),
            "execd socket is not one stale same-user socket"
        );
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn verify_peer(stream: &UnixStream) -> Result<()> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the socket and output buffer are live for the duration of getsockopt.
    let result = unsafe {
        libc::getsockopt(
            std::os::fd::AsRawFd::as_raw_fd(stream),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    anyhow::ensure!(result == 0, "failed to read execd peer credentials");
    // SAFETY: geteuid has no preconditions.
    anyhow::ensure!(
        credentials.uid == unsafe { libc::geteuid() },
        "execd peer UID differs"
    );
    Ok(())
}

fn claim_systemd_listener() -> Result<UnixListener> {
    use std::os::fd::FromRawFd as _;

    let listen_pid: u32 = std::env::var("LISTEN_PID")?.parse()?;
    let listen_fds: u32 = std::env::var("LISTEN_FDS")?.parse()?;
    anyhow::ensure!(
        listen_pid == std::process::id() && listen_fds == 1,
        "invalid activation FDs"
    );
    // SAFETY: systemd transfers ownership of descriptor 3 to this process.
    let listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(3) };
    listener.set_nonblocking(true)?;
    UnixListener::from_std(listener).context("failed to claim systemd execd listener")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::ExecutionStoreLookupError;
    use crate::supervisor::ExecutionControlError;

    #[test]
    fn service_causes_map_to_stable_protocol_codes() {
        for (error, code, retryable) in [
            (
                anyhow::Error::new(ExecutionStoreLookupError::ExecutionNotFound),
                ExecutionProtocolErrorCode::NotFound,
                false,
            ),
            (
                anyhow::Error::new(ExecutionControlError::InactiveTerminal),
                ExecutionProtocolErrorCode::Conflict,
                false,
            ),
            (
                anyhow::Error::new(ExecutionControlError::Unavailable),
                ExecutionProtocolErrorCode::Unavailable,
                true,
            ),
            (
                anyhow::anyhow!("sqlite write failed"),
                ExecutionProtocolErrorCode::Internal,
                false,
            ),
        ] {
            let mapped = map_service_error(error);
            assert_eq!(mapped.code, code);
            assert_eq!(mapped.retryable, retryable);
        }
    }
}
