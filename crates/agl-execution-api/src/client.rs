use std::fmt;
use std::io::{BufRead as _, Read as _, Write as _};
use std::net::Shutdown;
use std::os::unix::net::UnixStream as BlockingUnixStream;
use std::path::PathBuf;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::ExecutionId;
use crate::{
    ExecutionCommand, ExecutionOutput, ExecutionProtocolError, ExecutionProtocolRequest,
    ExecutionProtocolResponse, ExecutionResponse, ExecutionSignal, ExecutionStartRequest,
    ExecutionStatus, MAX_EXECUTION_FRAME_BYTES, TerminalId, TerminalSize,
};

#[derive(Clone, Debug)]
pub struct ExecutionClient {
    socket_path: PathBuf,
}

impl ExecutionClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub fn blocking(&self) -> BlockingExecutionClient {
        BlockingExecutionClient::new(self.socket_path.clone())
    }

    pub async fn start(
        &self,
        request: ExecutionStartRequest,
    ) -> Result<ExecutionStatus, ExecutionClientError> {
        match self.request(ExecutionCommand::Start { request }).await? {
            ExecutionResponse::Started { status } => Ok(status),
            _ => Err(ExecutionClientError::UnexpectedResponse),
        }
    }

    pub async fn inspect(
        &self,
        execution_id: ExecutionId,
    ) -> Result<ExecutionStatus, ExecutionClientError> {
        match self
            .request(ExecutionCommand::Inspect { execution_id })
            .await?
        {
            ExecutionResponse::Status { status } => Ok(status),
            _ => Err(ExecutionClientError::UnexpectedResponse),
        }
    }

    pub async fn inspect_terminal(
        &self,
        terminal_id: TerminalId,
    ) -> Result<ExecutionStatus, ExecutionClientError> {
        match self
            .request(ExecutionCommand::InspectTerminal { terminal_id })
            .await?
        {
            ExecutionResponse::Status { status } => Ok(status),
            _ => Err(ExecutionClientError::UnexpectedResponse),
        }
    }

    pub async fn read(
        &self,
        execution_id: ExecutionId,
        after: u64,
        max_bytes: u32,
    ) -> Result<ExecutionOutput, ExecutionClientError> {
        match self
            .request(ExecutionCommand::Read {
                execution_id,
                after,
                max_bytes,
            })
            .await?
        {
            ExecutionResponse::Output { output } => Ok(output),
            _ => Err(ExecutionClientError::UnexpectedResponse),
        }
    }

    pub async fn write(
        &self,
        terminal_id: TerminalId,
        data: Vec<u8>,
    ) -> Result<(), ExecutionClientError> {
        self.acknowledge(ExecutionCommand::Write { terminal_id, data })
            .await
    }

    pub async fn resize(
        &self,
        terminal_id: TerminalId,
        size: TerminalSize,
    ) -> Result<(), ExecutionClientError> {
        self.acknowledge(ExecutionCommand::Resize { terminal_id, size })
            .await
    }

    pub async fn signal(
        &self,
        execution_id: ExecutionId,
        signal: ExecutionSignal,
    ) -> Result<(), ExecutionClientError> {
        self.acknowledge(ExecutionCommand::Signal {
            execution_id,
            signal,
        })
        .await
    }

    async fn acknowledge(&self, command: ExecutionCommand) -> Result<(), ExecutionClientError> {
        match self.request(command).await? {
            ExecutionResponse::Acknowledged => Ok(()),
            _ => Err(ExecutionClientError::UnexpectedResponse),
        }
    }

    async fn request(
        &self,
        command: ExecutionCommand,
    ) -> Result<ExecutionResponse, ExecutionClientError> {
        let request = ExecutionProtocolRequest::new(command);
        request.validate()?;
        let request_id = request.request_id.clone();
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(ExecutionClientError::Io)?;
        verify_peer(std::os::fd::AsRawFd::as_raw_fd(&stream))?;
        let mut bytes = serde_json::to_vec(&request).map_err(ExecutionClientError::Json)?;
        bytes.push(b'\n');
        stream
            .write_all(&bytes)
            .await
            .map_err(ExecutionClientError::Io)?;
        stream.shutdown().await.map_err(ExecutionClientError::Io)?;

        let reader = BufReader::new(stream);
        let mut response = Vec::new();
        let read = reader
            .take((MAX_EXECUTION_FRAME_BYTES + 2) as u64)
            .read_until(b'\n', &mut response)
            .await
            .map_err(ExecutionClientError::Io)?;
        if read == 0 || !response.ends_with(b"\n") {
            return Err(ExecutionClientError::InvalidFrame);
        }
        response.pop();
        if response.len() > MAX_EXECUTION_FRAME_BYTES {
            return Err(ExecutionClientError::InvalidFrame);
        }
        let response: ExecutionProtocolResponse =
            serde_json::from_slice(&response).map_err(ExecutionClientError::Json)?;
        if response.schema != crate::EXECUTION_PROTOCOL_SCHEMA || response.request_id != request_id
        {
            return Err(ExecutionClientError::InvalidFrame);
        }
        response.result.map_err(ExecutionClientError::Protocol)
    }
}

#[derive(Clone, Debug)]
pub struct BlockingExecutionClient {
    socket_path: PathBuf,
}

impl BlockingExecutionClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    pub fn start(
        &self,
        request: ExecutionStartRequest,
    ) -> Result<ExecutionStatus, ExecutionClientError> {
        match self.request(ExecutionCommand::Start { request })? {
            ExecutionResponse::Started { status } => Ok(status),
            _ => Err(ExecutionClientError::UnexpectedResponse),
        }
    }

    pub fn inspect(
        &self,
        execution_id: ExecutionId,
    ) -> Result<ExecutionStatus, ExecutionClientError> {
        match self.request(ExecutionCommand::Inspect { execution_id })? {
            ExecutionResponse::Status { status } => Ok(status),
            _ => Err(ExecutionClientError::UnexpectedResponse),
        }
    }

    pub fn read(
        &self,
        execution_id: ExecutionId,
        after: u64,
        max_bytes: u32,
    ) -> Result<ExecutionOutput, ExecutionClientError> {
        match self.request(ExecutionCommand::Read {
            execution_id,
            after,
            max_bytes,
        })? {
            ExecutionResponse::Output { output } => Ok(output),
            _ => Err(ExecutionClientError::UnexpectedResponse),
        }
    }

    pub fn signal(
        &self,
        execution_id: ExecutionId,
        signal: ExecutionSignal,
    ) -> Result<(), ExecutionClientError> {
        match self.request(ExecutionCommand::Signal {
            execution_id,
            signal,
        })? {
            ExecutionResponse::Acknowledged => Ok(()),
            _ => Err(ExecutionClientError::UnexpectedResponse),
        }
    }

    fn request(
        &self,
        command: ExecutionCommand,
    ) -> Result<ExecutionResponse, ExecutionClientError> {
        let request = ExecutionProtocolRequest::new(command);
        request.validate()?;
        let request_id = request.request_id.clone();
        let mut stream =
            BlockingUnixStream::connect(&self.socket_path).map_err(ExecutionClientError::Io)?;
        verify_peer(std::os::fd::AsRawFd::as_raw_fd(&stream))?;
        let mut bytes = serde_json::to_vec(&request).map_err(ExecutionClientError::Json)?;
        bytes.push(b'\n');
        stream.write_all(&bytes).map_err(ExecutionClientError::Io)?;
        stream
            .shutdown(Shutdown::Write)
            .map_err(ExecutionClientError::Io)?;
        let mut response = Vec::new();
        let read = std::io::BufReader::new(stream)
            .take((MAX_EXECUTION_FRAME_BYTES + 2) as u64)
            .read_until(b'\n', &mut response)
            .map_err(ExecutionClientError::Io)?;
        if read == 0 || !response.ends_with(b"\n") {
            return Err(ExecutionClientError::InvalidFrame);
        }
        response.pop();
        if response.len() > MAX_EXECUTION_FRAME_BYTES {
            return Err(ExecutionClientError::InvalidFrame);
        }
        let response: ExecutionProtocolResponse =
            serde_json::from_slice(&response).map_err(ExecutionClientError::Json)?;
        if response.schema != crate::EXECUTION_PROTOCOL_SCHEMA || response.request_id != request_id
        {
            return Err(ExecutionClientError::InvalidFrame);
        }
        response.result.map_err(ExecutionClientError::Protocol)
    }
}

fn verify_peer(descriptor: std::os::fd::RawFd) -> Result<(), ExecutionClientError> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: descriptor is a connected Unix socket and the output buffer is initialized.
    let result = unsafe {
        libc::getsockopt(
            descriptor,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(ExecutionClientError::Io(std::io::Error::last_os_error()));
    }
    // SAFETY: geteuid has no preconditions.
    if credentials.uid != unsafe { libc::geteuid() } {
        return Err(ExecutionClientError::PeerOwnership);
    }
    Ok(())
}

#[derive(Debug)]
pub enum ExecutionClientError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Protocol(ExecutionProtocolError),
    InvalidFrame,
    UnexpectedResponse,
    PeerOwnership,
}

impl fmt::Display for ExecutionClientError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "execution transport failed: {error}"),
            Self::Json(error) => write!(formatter, "execution frame JSON failed: {error}"),
            Self::Protocol(error) => error.fmt(formatter),
            Self::InvalidFrame => {
                formatter.write_str("execution service returned an invalid frame")
            }
            Self::UnexpectedResponse => {
                formatter.write_str("execution service returned an unexpected response")
            }
            Self::PeerOwnership => formatter.write_str("execution service peer UID differs"),
        }
    }
}

impl std::error::Error for ExecutionClientError {}

impl From<ExecutionProtocolError> for ExecutionClientError {
    fn from(error: ExecutionProtocolError) -> Self {
        Self::Protocol(error)
    }
}
