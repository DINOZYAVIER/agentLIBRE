use std::fmt::{self, Display, Formatter};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agl_exec::{
    AuthorityFingerprint, CallerOwner, ExecutionCursor, ExecutionId, ExecutionListFilter,
    ExecutionReadResult, ExecutionStatus, InputLease, KillMode, ProcessBytes, TerminalSize,
};
use agl_terminal::{
    TerminalCommandResult, TerminalDescriptor, TerminalId, TerminalRecord, TerminalStreamId,
    TerminalTopologyId,
};
use agl_terminal_protocol::{
    ExecutionAdmission, ProtocolValidationError, ServiceIdentity, TerminalAdmission,
    TerminalEventBatch, TerminalFailure, TerminalGenerationIdentity, TerminalRequest,
    TerminalRequestKind, TerminalResponse, TerminalResponseKind,
};
use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use thiserror::Error;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TransportError {
    #[error("terminal transport was cancelled")]
    Cancelled,
    #[error("terminal transport is unavailable: {0}")]
    Unavailable(String),
    #[error("terminal transport rejected the frame: {0}")]
    InvalidFrame(String),
}

pub trait TerminalTransport: Send + Sync {
    fn exchange<'a>(
        &'a self,
        request: TerminalRequest,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<TerminalResponse, TransportError>>;
}

#[derive(Clone, Debug)]
pub struct UnixTerminalTransport {
    socket_path: PathBuf,
}

impl UnixTerminalTransport {
    pub fn new(socket_path: PathBuf) -> Result<Self, TransportError> {
        if !socket_path.is_absolute() {
            return Err(TransportError::InvalidFrame(
                "terminal socket path must be absolute".to_owned(),
            ));
        }
        Ok(Self { socket_path })
    }
}

impl TerminalTransport for UnixTerminalTransport {
    fn exchange<'a>(
        &'a self,
        request: TerminalRequest,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<TerminalResponse, TransportError>> {
        async move {
            let encoded = serde_json::to_vec(&request)
                .map_err(|error| TransportError::InvalidFrame(error.to_string()))?;
            if encoded.len() > agl_terminal_protocol::MAX_TERMINAL_FRAME_BYTES {
                return Err(TransportError::InvalidFrame(
                    "terminal request frame exceeds the protocol bound".to_owned(),
                ));
            }
            let frame_length = u32::try_from(encoded.len()).map_err(|_| {
                TransportError::InvalidFrame("terminal request frame is too large".to_owned())
            })?;
            let mut stream = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(TransportError::Cancelled),
                result = UnixStream::connect(&self.socket_path) => result.map_err(|error| {
                    TransportError::Unavailable(error.to_string())
                })?,
            };
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(TransportError::Cancelled),
                result = async {
                    stream.write_all(&frame_length.to_be_bytes()).await?;
                    stream.write_all(&encoded).await?;
                    stream.flush().await?;
                    Ok::<(), std::io::Error>(())
                } => result.map_err(|error| TransportError::Unavailable(error.to_string()))?,
            }
            let mut length = [0u8; 4];
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(TransportError::Cancelled),
                result = stream.read_exact(&mut length) => result.map_err(|error| {
                    TransportError::Unavailable(error.to_string())
                })?,
            };
            let length = usize::try_from(u32::from_be_bytes(length)).map_err(|_| {
                TransportError::InvalidFrame("terminal response length is invalid".to_owned())
            })?;
            if length > agl_terminal_protocol::MAX_TERMINAL_FRAME_BYTES {
                return Err(TransportError::InvalidFrame(
                    "terminal response frame exceeds the protocol bound".to_owned(),
                ));
            }
            let mut response = vec![0u8; length];
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(TransportError::Cancelled),
                result = stream.read_exact(&mut response) => result.map_err(|error| {
                    TransportError::Unavailable(error.to_string())
                })?,
            };
            serde_json::from_slice(&response)
                .map_err(|error| TransportError::InvalidFrame(error.to_string()))
        }
        .boxed()
    }
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error(transparent)]
    Protocol(#[from] ProtocolValidationError),
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("terminal request was cancelled")]
    Cancelled,
    #[error("terminal response request identity does not match")]
    CorrelationMismatch,
    #[error("terminal service returned an unexpected response kind")]
    UnexpectedResponse,
    #[error("terminal client live identity state is unavailable")]
    IdentityState,
    #[error("terminal runtime projection is invalid: {0}")]
    Projection(String),
    #[error("terminal service rejected the operation: {0}")]
    Remote(TerminalFailureDisplay),
}

#[derive(Clone, Debug)]
pub struct TerminalFailureDisplay(pub TerminalFailure);

impl Display for TerminalFailureDisplay {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:?}: {}", self.0.code, self.0.message)
    }
}

pub struct TerminalClient<T> {
    transport: T,
    expected_generation: TerminalGenerationIdentity,
    live_identity: Mutex<Option<ServiceIdentity>>,
    runtime_projection: Option<PathBuf>,
    authority_fingerprint: Option<AuthorityFingerprint>,
}

impl<T> TerminalClient<T>
where
    T: TerminalTransport,
{
    pub fn new(transport: T, expected_service: ServiceIdentity) -> Result<Self, ClientError> {
        expected_service.validate()?;
        Ok(Self {
            transport,
            expected_generation: expected_service.installed_generation().clone(),
            live_identity: Mutex::new(Some(expected_service)),
            runtime_projection: None,
            authority_fingerprint: None,
        })
    }

    pub fn authorized(
        transport: T,
        expected_service: ServiceIdentity,
        authority_fingerprint: AuthorityFingerprint,
    ) -> Result<Self, ClientError> {
        let mut client = Self::new(transport, expected_service)?;
        client.authority_fingerprint = Some(authority_fingerprint);
        Ok(client)
    }

    pub fn for_generation(
        transport: T,
        expected_generation: TerminalGenerationIdentity,
        authority_fingerprint: Option<AuthorityFingerprint>,
    ) -> Result<Self, ClientError> {
        expected_generation
            .validate()
            .map_err(|_| ProtocolValidationError::InvalidInstalledGeneration)?;
        Ok(Self {
            transport,
            expected_generation,
            live_identity: Mutex::new(None),
            runtime_projection: None,
            authority_fingerprint,
        })
    }

    pub fn for_generation_with_runtime_projection(
        transport: T,
        expected_generation: TerminalGenerationIdentity,
        authority_fingerprint: Option<AuthorityFingerprint>,
        runtime_projection: PathBuf,
    ) -> Result<Self, ClientError> {
        if !runtime_projection.is_absolute() {
            return Err(ClientError::Projection(
                "terminal runtime projection path must be absolute".to_owned(),
            ));
        }
        let mut client =
            Self::for_generation(transport, expected_generation, authority_fingerprint)?;
        client.runtime_projection = Some(runtime_projection);
        Ok(client)
    }

    pub fn expected_generation(&self) -> &TerminalGenerationIdentity {
        &self.expected_generation
    }

    pub async fn hello(&self, cancellation: CancellationToken) -> Result<(), ClientError> {
        self.bootstrap(cancellation).await.map(|_| ())
    }

    pub async fn bootstrap_identity(
        &self,
        cancellation: CancellationToken,
    ) -> Result<ServiceIdentity, ClientError> {
        self.bootstrap(cancellation).await
    }

    pub async fn start_execution(
        &self,
        admission: ExecutionAdmission,
        cancellation: CancellationToken,
    ) -> Result<ExecutionStatus, ClientError> {
        match self
            .call(
                TerminalRequestKind::StartExecution {
                    admission: Box::new(admission),
                },
                cancellation,
            )
            .await?
        {
            TerminalResponseKind::Execution { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn inspect_execution(
        &self,
        execution_id: ExecutionId,
        cancellation: CancellationToken,
    ) -> Result<ExecutionStatus, ClientError> {
        match self
            .call(
                TerminalRequestKind::InspectExecution { execution_id },
                cancellation,
            )
            .await?
        {
            TerminalResponseKind::Execution { status } => Ok(status),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn read_execution(
        &self,
        execution_id: ExecutionId,
        cursor: ExecutionCursor,
        maximum_bytes: u32,
        cancellation: CancellationToken,
    ) -> Result<ExecutionReadResult, ClientError> {
        match self
            .call(
                TerminalRequestKind::ReadExecution {
                    execution_id,
                    cursor,
                    maximum_bytes,
                },
                cancellation,
            )
            .await?
        {
            TerminalResponseKind::ExecutionRead { read } => Ok(read),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn attach_execution(
        &self,
        execution_id: ExecutionId,
        writable: bool,
        cancellation: CancellationToken,
    ) -> Result<ExecutionAttachment, ClientError> {
        match self
            .call(
                TerminalRequestKind::AttachExecution {
                    execution_id,
                    writable,
                },
                cancellation,
            )
            .await?
        {
            TerminalResponseKind::ExecutionAttached { status, lease } => {
                Ok(ExecutionAttachment { status, lease })
            }
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn detach_execution(
        &self,
        execution_id: ExecutionId,
        lease: InputLease,
        cancellation: CancellationToken,
    ) -> Result<(), ClientError> {
        self.expect_ack(
            TerminalRequestKind::DetachExecution {
                execution_id,
                lease,
            },
            cancellation,
        )
        .await
    }

    pub async fn write_execution(
        &self,
        execution_id: ExecutionId,
        lease: InputLease,
        bytes: ProcessBytes,
        eof: bool,
        cancellation: CancellationToken,
    ) -> Result<(), ClientError> {
        self.expect_ack(
            TerminalRequestKind::WriteExecution {
                execution_id,
                lease,
                bytes,
                eof,
            },
            cancellation,
        )
        .await
    }

    pub async fn resize_execution(
        &self,
        execution_id: ExecutionId,
        size: TerminalSize,
        cancellation: CancellationToken,
    ) -> Result<(), ClientError> {
        self.expect_ack(
            TerminalRequestKind::ResizeExecution { execution_id, size },
            cancellation,
        )
        .await
    }

    pub async fn interrupt_execution(
        &self,
        execution_id: ExecutionId,
        cancellation: CancellationToken,
    ) -> Result<(), ClientError> {
        self.expect_ack(
            TerminalRequestKind::InterruptExecution { execution_id },
            cancellation,
        )
        .await
    }

    pub async fn terminate_execution(
        &self,
        execution_id: ExecutionId,
        mode: KillMode,
        cancellation: CancellationToken,
    ) -> Result<(), ClientError> {
        self.expect_ack(
            TerminalRequestKind::TerminateExecution { execution_id, mode },
            cancellation,
        )
        .await
    }

    pub async fn list_executions(
        &self,
        filter: ExecutionListFilter,
        cancellation: CancellationToken,
    ) -> Result<Vec<ExecutionStatus>, ClientError> {
        match self
            .call(TerminalRequestKind::ListExecutions { filter }, cancellation)
            .await?
        {
            TerminalResponseKind::ExecutionList { statuses } => Ok(statuses),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn revoke_grant(
        &self,
        grant_id: String,
        cancellation: CancellationToken,
    ) -> Result<u32, ClientError> {
        match self
            .call(TerminalRequestKind::RevokeGrant { grant_id }, cancellation)
            .await?
        {
            TerminalResponseKind::GrantRevoked {
                terminated_executions,
            } => Ok(terminated_executions),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn ensure(
        &self,
        admission: TerminalAdmission,
        cancellation: CancellationToken,
    ) -> Result<TerminalDescriptor, ClientError> {
        match self
            .call(
                TerminalRequestKind::Ensure {
                    admission: Box::new(admission),
                },
                cancellation,
            )
            .await?
        {
            TerminalResponseKind::Terminal { descriptor } => Ok(descriptor),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn execute_agent_command(
        &self,
        admission: TerminalAdmission,
        command: String,
        timeout_ms: Option<u64>,
        cancellation: CancellationToken,
    ) -> Result<TerminalCommandResult, ClientError> {
        match self
            .call(
                TerminalRequestKind::ExecuteAgentCommand {
                    admission: Box::new(admission),
                    command,
                    timeout_ms,
                },
                cancellation,
            )
            .await?
        {
            TerminalResponseKind::AgentCommand { result } => Ok(result),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn list_topology(
        &self,
        topology_id: TerminalTopologyId,
        cancellation: CancellationToken,
    ) -> Result<Vec<TerminalRecord>, ClientError> {
        match self
            .call(
                TerminalRequestKind::ListTopology { topology_id },
                cancellation,
            )
            .await?
        {
            TerminalResponseKind::TerminalList { records } => Ok(records),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn promote(
        &self,
        terminal_id: TerminalId,
        topology_id: TerminalTopologyId,
        owner: CallerOwner,
        cancellation: CancellationToken,
    ) -> Result<TerminalRecord, ClientError> {
        match self
            .call(
                TerminalRequestKind::Promote {
                    terminal_id,
                    topology_id,
                    owner,
                },
                cancellation,
            )
            .await?
        {
            TerminalResponseKind::TerminalRecord { record } => Ok(*record),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn retire(
        &self,
        terminal_id: TerminalId,
        cancellation: CancellationToken,
    ) -> Result<TerminalRecord, ClientError> {
        match self
            .call(TerminalRequestKind::Retire { terminal_id }, cancellation)
            .await?
        {
            TerminalResponseKind::TerminalRecord { record } => Ok(*record),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn inspect(
        &self,
        terminal_id: TerminalId,
        cancellation: CancellationToken,
    ) -> Result<TerminalDescriptor, ClientError> {
        match self
            .call(TerminalRequestKind::Inspect { terminal_id }, cancellation)
            .await?
        {
            TerminalResponseKind::Terminal { descriptor } => Ok(descriptor),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn attach(
        &self,
        terminal_id: TerminalId,
        after_sequence: u64,
        writable: bool,
        cancellation: CancellationToken,
    ) -> Result<TerminalAttachment, ClientError> {
        match self
            .call(
                TerminalRequestKind::Attach {
                    terminal_id,
                    after_sequence,
                    writable,
                },
                cancellation,
            )
            .await?
        {
            TerminalResponseKind::Attached {
                descriptor,
                stream_id,
                next_sequence,
                writable,
            } => Ok(TerminalAttachment {
                descriptor,
                stream_id,
                next_sequence,
                writable,
            }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn read_events(
        &self,
        stream_id: TerminalStreamId,
        after_sequence: u64,
        maximum_events: u16,
        cancellation: CancellationToken,
    ) -> Result<TerminalEventBatch, ClientError> {
        match self
            .call(
                TerminalRequestKind::ReadEvents {
                    stream_id,
                    after_sequence,
                    maximum_events,
                },
                cancellation,
            )
            .await?
        {
            TerminalResponseKind::Events { batch } => Ok(batch),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn input(
        &self,
        terminal_id: TerminalId,
        stream_id: TerminalStreamId,
        bytes: ProcessBytes,
        cancellation: CancellationToken,
    ) -> Result<(), ClientError> {
        self.expect_ack(
            TerminalRequestKind::Input {
                terminal_id,
                stream_id,
                bytes,
            },
            cancellation,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn submit_command(
        &self,
        terminal_id: TerminalId,
        topology_id: agl_terminal::TerminalTopologyId,
        stream_id: TerminalStreamId,
        expected_command_sequence: u64,
        expected_prompt_generation: u64,
        command: String,
        cancellation: CancellationToken,
    ) -> Result<CommandAccepted, ClientError> {
        match self
            .call(
                TerminalRequestKind::SubmitCommand {
                    terminal_id,
                    topology_id,
                    stream_id,
                    expected_command_sequence,
                    expected_prompt_generation,
                    command,
                },
                cancellation,
            )
            .await?
        {
            TerminalResponseKind::CommandAccepted {
                command_sequence,
                output_after_sequence,
            } => Ok(CommandAccepted {
                command_sequence,
                output_after_sequence,
            }),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    pub async fn cancel_command(
        &self,
        terminal_id: TerminalId,
        command_sequence: u64,
        cancellation: CancellationToken,
    ) -> Result<(), ClientError> {
        self.expect_ack(
            TerminalRequestKind::CancelCommand {
                terminal_id,
                command_sequence,
            },
            cancellation,
        )
        .await
    }

    pub async fn resize(
        &self,
        terminal_id: TerminalId,
        size: TerminalSize,
        cancellation: CancellationToken,
    ) -> Result<(), ClientError> {
        self.expect_ack(
            TerminalRequestKind::Resize { terminal_id, size },
            cancellation,
        )
        .await
    }

    pub async fn detach(
        &self,
        stream_id: TerminalStreamId,
        cancellation: CancellationToken,
    ) -> Result<(), ClientError> {
        self.expect_ack(TerminalRequestKind::Detach { stream_id }, cancellation)
            .await
    }

    pub async fn terminate(
        &self,
        terminal_id: TerminalId,
        cancellation: CancellationToken,
    ) -> Result<(), ClientError> {
        self.expect_ack(TerminalRequestKind::Terminate { terminal_id }, cancellation)
            .await
    }

    async fn expect_ack(
        &self,
        request: TerminalRequestKind,
        cancellation: CancellationToken,
    ) -> Result<(), ClientError> {
        match self.call(request, cancellation).await? {
            TerminalResponseKind::Ack => Ok(()),
            _ => Err(ClientError::UnexpectedResponse),
        }
    }

    async fn call(
        &self,
        request: TerminalRequestKind,
        cancellation: CancellationToken,
    ) -> Result<TerminalResponseKind, ClientError> {
        if cancellation.is_cancelled() {
            return Err(ClientError::Cancelled);
        }
        if matches!(request, TerminalRequestKind::Hello) {
            self.bootstrap(cancellation).await?;
            return Ok(TerminalResponseKind::Hello);
        }
        let expected_service = match self.current_live_identity()? {
            Some(identity) => identity,
            None => self.bootstrap(cancellation.clone()).await?,
        };
        let request = TerminalRequest::new(
            expected_service.clone(),
            self.authority_fingerprint.clone(),
            request,
        )?;
        let request_id = request.request_id.clone();
        let response = self
            .transport
            .exchange(request, cancellation.clone())
            .await?;
        if cancellation.is_cancelled() {
            return Err(ClientError::Cancelled);
        }
        response.validate()?;
        if response.request_id != request_id {
            return Err(ClientError::CorrelationMismatch);
        }
        expected_service.require_exact(&response.service)?;
        match response.response {
            TerminalResponseKind::Failure { failure } => {
                Err(ClientError::Remote(TerminalFailureDisplay(failure)))
            }
            response => Ok(response),
        }
    }

    async fn bootstrap(
        &self,
        cancellation: CancellationToken,
    ) -> Result<ServiceIdentity, ClientError> {
        if cancellation.is_cancelled() {
            return Err(ClientError::Cancelled);
        }
        let request = TerminalRequest::hello(self.expected_generation.clone())?;
        let request_id = request.request_id.clone();
        let response = self
            .transport
            .exchange(request, cancellation.clone())
            .await?;
        if cancellation.is_cancelled() {
            return Err(ClientError::Cancelled);
        }
        response.validate()?;
        if response.request_id != request_id {
            return Err(ClientError::CorrelationMismatch);
        }
        self.expected_generation
            .require_exact(response.service.installed_generation())
            .map_err(|_| ProtocolValidationError::ServiceIdentityMismatch)?;
        if !matches!(response.response, TerminalResponseKind::Hello) {
            return Err(ClientError::UnexpectedResponse);
        }
        let identity = response.service;
        if let Some(path) = &self.runtime_projection {
            let projected = read_runtime_projection(path)?;
            identity
                .require_exact(&projected)
                .map_err(|_| ProtocolValidationError::ServiceIdentityMismatch)?;
        }
        *self
            .live_identity
            .lock()
            .map_err(|_| ClientError::IdentityState)? = Some(identity.clone());
        Ok(identity)
    }

    fn current_live_identity(&self) -> Result<Option<ServiceIdentity>, ClientError> {
        let identity = self
            .live_identity
            .lock()
            .map_err(|_| ClientError::IdentityState)?
            .clone();
        Ok(identity)
    }
}

fn read_runtime_projection(path: &std::path::Path) -> Result<ServiceIdentity, ClientError> {
    use std::io::Read as _;
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let mut file = options
        .open(path)
        .map_err(|error| ClientError::Projection(error.to_string()))?;
    let metadata = file
        .metadata()
        .map_err(|error| ClientError::Projection(error.to_string()))?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > 16 * 1024 {
        return Err(ClientError::Projection(
            "terminal runtime projection is not a bounded regular file".to_owned(),
        ));
    }
    #[cfg(unix)]
    if metadata.mode() & 0o077 != 0 || metadata.nlink() != 1 {
        return Err(ClientError::Projection(
            "terminal runtime projection permissions or link count are unsafe".to_owned(),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| ClientError::Projection(error.to_string()))?;
    let identity: ServiceIdentity = serde_json::from_slice(&bytes)
        .map_err(|error| ClientError::Projection(error.to_string()))?;
    identity
        .validate()
        .map_err(|error| ClientError::Projection(error.to_string()))?;
    Ok(identity)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalAttachment {
    pub descriptor: TerminalDescriptor,
    pub stream_id: TerminalStreamId,
    pub next_sequence: u64,
    pub writable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionAttachment {
    pub status: ExecutionStatus,
    pub lease: InputLease,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandAccepted {
    pub command_sequence: u64,
    pub output_after_sequence: u64,
}

pub trait EmbeddedTerminalService: Send + Sync {
    fn handle<'a>(
        &'a self,
        request: TerminalRequest,
    ) -> BoxFuture<'a, Result<TerminalResponse, TransportError>>;
}

pub struct EmbeddedTransport<S> {
    service: Arc<S>,
}

impl<S> EmbeddedTransport<S> {
    pub fn new(service: Arc<S>) -> Self {
        Self { service }
    }
}

impl<S> Clone for EmbeddedTransport<S> {
    fn clone(&self) -> Self {
        Self {
            service: Arc::clone(&self.service),
        }
    }
}

impl<S> TerminalTransport for EmbeddedTransport<S>
where
    S: EmbeddedTerminalService + 'static,
{
    fn exchange<'a>(
        &'a self,
        request: TerminalRequest,
        cancellation: CancellationToken,
    ) -> BoxFuture<'a, Result<TerminalResponse, TransportError>> {
        async move {
            tokio::select! {
                biased;
                () = cancellation.cancelled() => Err(TransportError::Cancelled),
                response = self.service.handle(request) => response,
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Mutex;
    use std::time::Duration;

    use agl_exec::{AuthorityFingerprint, ServiceGenerationId};
    use agl_terminal_protocol::{
        TERMINAL_PROTOCOL_VERSION, TERMINAL_RESPONSE_SCHEMA, TerminalFailureCode,
        TerminalGenerationIdentity,
    };

    use super::*;

    fn service_identity() -> ServiceIdentity {
        ServiceIdentity::new(
            TerminalGenerationIdentity::new(
                AuthorityFingerprint::new(format!("sha256:{}", "b".repeat(64))).unwrap(),
                "c".repeat(40),
                AuthorityFingerprint::new(format!("sha256:{}", "a".repeat(64))).unwrap(),
                TERMINAL_PROTOCOL_VERSION,
            )
            .unwrap(),
            ServiceGenerationId::generate(),
        )
        .unwrap()
    }

    struct EchoService {
        identity: ServiceIdentity,
        seen: Mutex<Vec<TerminalRequestKind>>,
    }

    impl EmbeddedTerminalService for EchoService {
        fn handle<'a>(
            &'a self,
            request: TerminalRequest,
        ) -> BoxFuture<'a, Result<TerminalResponse, TransportError>> {
            async move {
                self.seen.lock().unwrap().push(request.request);
                Ok(TerminalResponse {
                    schema: TERMINAL_RESPONSE_SCHEMA.to_owned(),
                    request_id: request.request_id,
                    service: self.identity.clone(),
                    response: TerminalResponseKind::Hello,
                })
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn embedded_transport_uses_the_same_exact_protocol() {
        let identity = service_identity();
        let service = Arc::new(EchoService {
            identity: identity.clone(),
            seen: Mutex::new(Vec::new()),
        });
        let client =
            TerminalClient::new(EmbeddedTransport::new(service.clone()), identity).unwrap();
        client.hello(CancellationToken::new()).await.unwrap();
        assert_eq!(
            *service.seen.lock().unwrap(),
            vec![TerminalRequestKind::Hello]
        );
    }

    #[tokio::test]
    async fn non_handshake_calls_require_explicit_authority() {
        let identity = service_identity();
        let service = Arc::new(EchoService {
            identity: identity.clone(),
            seen: Mutex::new(Vec::new()),
        });
        let client =
            TerminalClient::new(EmbeddedTransport::new(service.clone()), identity).unwrap();
        assert!(matches!(
            client
                .inspect(TerminalId::generate(), CancellationToken::new())
                .await,
            Err(ClientError::Protocol(
                ProtocolValidationError::MissingAuthority
            ))
        ));
        assert!(service.seen.lock().unwrap().is_empty());
    }

    struct BlockingService;

    impl EmbeddedTerminalService for BlockingService {
        fn handle<'a>(
            &'a self,
            _request: TerminalRequest,
        ) -> BoxFuture<'a, Result<TerminalResponse, TransportError>> {
            async move {
                std::future::pending::<()>().await;
                unreachable!()
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn cancellation_interrupts_an_in_flight_embedding_call() {
        let client = TerminalClient::new(
            EmbeddedTransport::new(Arc::new(BlockingService)),
            service_identity(),
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        let child = cancellation.clone();
        let operation = async move { client.hello(child).await };
        tokio::pin!(operation);
        tokio::time::sleep(Duration::from_millis(1)).await;
        cancellation.cancel();
        assert!(matches!(operation.await, Err(ClientError::Cancelled)));
    }

    struct FailureService {
        identity: ServiceIdentity,
    }

    impl EmbeddedTerminalService for FailureService {
        fn handle<'a>(
            &'a self,
            request: TerminalRequest,
        ) -> BoxFuture<'a, Result<TerminalResponse, TransportError>> {
            async move {
                Ok(TerminalResponse {
                    schema: TERMINAL_RESPONSE_SCHEMA.to_owned(),
                    request_id: request.request_id,
                    service: self.identity.clone(),
                    response: TerminalResponseKind::Failure {
                        failure: TerminalFailure {
                            code: TerminalFailureCode::AuthorityDenied,
                            message: "authority does not admit this operation".to_owned(),
                            retryable: false,
                        },
                    },
                })
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn typed_remote_failures_do_not_become_transport_success() {
        let identity = service_identity();
        let client = TerminalClient::new(
            EmbeddedTransport::new(Arc::new(FailureService {
                identity: identity.clone(),
            })),
            identity,
        )
        .unwrap();
        assert!(matches!(
            client.hello(CancellationToken::new()).await,
            Err(ClientError::Remote(_))
        ));
    }

    #[tokio::test]
    async fn unix_transport_uses_the_same_bounded_request_response_frames() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::UnixListener;

        let root = std::env::temp_dir().join(format!(
            "agl-terminal-client-{}-{}",
            std::process::id(),
            agl_terminal::TerminalRequestId::generate()
        ));
        fs::create_dir_all(&root).unwrap();
        let socket = root.join("terminald.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let identity = service_identity();
        let server_identity = identity.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut length = [0u8; 4];
            stream.read_exact(&mut length).await.unwrap();
            let mut frame = vec![0u8; u32::from_be_bytes(length) as usize];
            stream.read_exact(&mut frame).await.unwrap();
            let request = TerminalRequest::decode_json(&frame).unwrap();
            let response = TerminalResponse {
                schema: TERMINAL_RESPONSE_SCHEMA.to_owned(),
                request_id: request.request_id,
                service: server_identity,
                response: TerminalResponseKind::Hello,
            };
            let frame = serde_json::to_vec(&response).unwrap();
            stream
                .write_all(&(frame.len() as u32).to_be_bytes())
                .await
                .unwrap();
            stream.write_all(&frame).await.unwrap();
        });
        let client =
            TerminalClient::new(UnixTerminalTransport::new(socket).unwrap(), identity).unwrap();
        client.hello(CancellationToken::new()).await.unwrap();
        server.await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
