use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use agl_exec::{
    ExecutionCursor, ExecutionRequestId, InputLease, KillMode, ProcessError, ProcessErrorCode,
};
use agl_terminal::history::TerminalHistorySeed;
use agl_terminal::{
    TerminalDescriptor, TerminalId, TerminalOperation, TerminalRecord, TerminalStreamId,
};
use agl_terminal_client::{EmbeddedTerminalService, TransportError};
use agl_terminal_protocol::{
    ExecutionAdmission, ExecutionOperation, MAX_TERMINAL_EVENT_BATCH, MAX_TERMINAL_FRAME_BYTES,
    ProtocolValidationError, ServiceIdentity, TERMINAL_EVENT_SCHEMA, TERMINAL_RESPONSE_SCHEMA,
    TerminalAdmission, TerminalEvent, TerminalEventBatch, TerminalEventKind, TerminalFailure,
    TerminalFailureCode, TerminalRequest, TerminalRequestKind, TerminalResponse,
    TerminalResponseKind,
};
use futures_util::FutureExt as _;
use futures_util::future::BoxFuture;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use crate::ProcessHandle;
use crate::terminal::registry::{TerminalEnsureRequest, TerminalRegistry};

#[derive(Clone)]
struct AdmissionState {
    authority_fingerprint: agl_exec::AuthorityFingerprint,
    operations: BTreeSet<TerminalOperation>,
}

#[derive(Clone)]
struct ExecutionAdmissionState {
    authority_fingerprint: agl_exec::AuthorityFingerprint,
    operations: BTreeSet<ExecutionOperation>,
}

struct StreamState {
    terminal_id: TerminalId,
    execution_id: agl_exec::ExecutionId,
    lease: InputLease,
    writable: bool,
}

pub struct TerminalService {
    identity: ServiceIdentity,
    registry: Arc<TerminalRegistry>,
    process: ProcessHandle,
    admissions: Mutex<BTreeMap<TerminalId, AdmissionState>>,
    execution_admissions: Mutex<BTreeMap<agl_exec::ExecutionId, ExecutionAdmissionState>>,
    execution_fingerprints: Mutex<BTreeMap<String, agl_exec::ExecutionId>>,
    streams: Mutex<BTreeMap<TerminalStreamId, StreamState>>,
}

impl TerminalService {
    pub fn new(
        identity: ServiceIdentity,
        registry: Arc<TerminalRegistry>,
        process: ProcessHandle,
    ) -> std::result::Result<Self, ProtocolValidationError> {
        identity.validate()?;
        Ok(Self {
            identity,
            registry,
            process,
            admissions: Mutex::new(BTreeMap::new()),
            execution_admissions: Mutex::new(BTreeMap::new()),
            execution_fingerprints: Mutex::new(BTreeMap::new()),
            streams: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn identity(&self) -> &ServiceIdentity {
        &self.identity
    }

    pub fn handle_request(&self, request: TerminalRequest) -> TerminalResponse {
        let request_id = request.request_id.clone();
        let response = match self.dispatch(request) {
            Ok(response) => response,
            Err(failure) => TerminalResponseKind::Failure { failure },
        };
        TerminalResponse {
            schema: TERMINAL_RESPONSE_SCHEMA.to_owned(),
            request_id,
            service: self.identity.clone(),
            response,
        }
    }

    fn dispatch(
        &self,
        request: TerminalRequest,
    ) -> std::result::Result<TerminalResponseKind, TerminalFailure> {
        request.validate().map_err(protocol_failure)?;
        request
            .expected_service
            .require_exact(&self.identity)
            .map_err(identity_failure)?;
        let authority = request.authority_fingerprint;
        match request.request {
            TerminalRequestKind::Hello => Ok(TerminalResponseKind::Hello),
            TerminalRequestKind::StartExecution { admission } => {
                self.start_execution(*admission, authority.as_ref().ok_or_else(authority_denied)?)
            }
            TerminalRequestKind::InspectExecution { execution_id } => {
                self.require_execution_operation(
                    &execution_id,
                    ExecutionOperation::Inspect,
                    authority.as_ref().ok_or_else(authority_denied)?,
                )?;
                let status = self
                    .process
                    .operator_status(&execution_id)
                    .map_err(process_failure)?;
                Ok(TerminalResponseKind::Execution { status })
            }
            TerminalRequestKind::ReadExecution {
                execution_id,
                cursor,
                maximum_bytes,
            } => {
                self.require_execution_operation(
                    &execution_id,
                    ExecutionOperation::Read,
                    authority.as_ref().ok_or_else(authority_denied)?,
                )?;
                let read = self
                    .process
                    .operator_read(
                        &execution_id,
                        cursor,
                        usize::try_from(maximum_bytes).unwrap_or(usize::MAX),
                    )
                    .map_err(process_failure)?;
                Ok(TerminalResponseKind::ExecutionRead { read })
            }
            TerminalRequestKind::AttachExecution {
                execution_id,
                writable,
            } => {
                self.require_execution_operation(
                    &execution_id,
                    if writable {
                        ExecutionOperation::Write
                    } else {
                        ExecutionOperation::Read
                    },
                    authority.as_ref().ok_or_else(authority_denied)?,
                )?;
                let lease = self
                    .process
                    .operator_attach(&execution_id, ExecutionRequestId::generate(), writable)
                    .map_err(process_failure)?;
                let status = self
                    .process
                    .operator_status(&execution_id)
                    .map_err(process_failure)?;
                Ok(TerminalResponseKind::ExecutionAttached { status, lease })
            }
            TerminalRequestKind::DetachExecution {
                execution_id,
                lease,
            } => {
                self.require_execution_authority(
                    &execution_id,
                    authority.as_ref().ok_or_else(authority_denied)?,
                )?;
                self.process
                    .operator_detach(&execution_id, lease)
                    .map_err(process_failure)?;
                Ok(TerminalResponseKind::Ack)
            }
            TerminalRequestKind::WriteExecution {
                execution_id,
                lease,
                bytes,
                eof,
            } => {
                self.require_execution_operation(
                    &execution_id,
                    ExecutionOperation::Write,
                    authority.as_ref().ok_or_else(authority_denied)?,
                )?;
                self.process
                    .operator_write(&execution_id, lease, bytes, eof)
                    .map_err(process_failure)?;
                Ok(TerminalResponseKind::Ack)
            }
            TerminalRequestKind::ResizeExecution { execution_id, size } => {
                self.require_execution_operation(
                    &execution_id,
                    ExecutionOperation::Resize,
                    authority.as_ref().ok_or_else(authority_denied)?,
                )?;
                self.process
                    .operator_resize(&execution_id, size)
                    .map_err(process_failure)?;
                Ok(TerminalResponseKind::Ack)
            }
            TerminalRequestKind::InterruptExecution { execution_id } => {
                self.require_execution_operation(
                    &execution_id,
                    ExecutionOperation::Interrupt,
                    authority.as_ref().ok_or_else(authority_denied)?,
                )?;
                self.process
                    .operator_interrupt_foreground(&execution_id)
                    .map_err(process_failure)?;
                Ok(TerminalResponseKind::Ack)
            }
            TerminalRequestKind::TerminateExecution { execution_id, mode } => {
                self.require_execution_operation(
                    &execution_id,
                    ExecutionOperation::Terminate,
                    authority.as_ref().ok_or_else(authority_denied)?,
                )?;
                self.process
                    .operator_kill(&execution_id, mode)
                    .map_err(process_failure)?;
                Ok(TerminalResponseKind::Ack)
            }
            TerminalRequestKind::Ensure { admission } => {
                self.ensure(*admission, authority.as_ref().ok_or_else(authority_denied)?)
            }
            TerminalRequestKind::Inspect { terminal_id } => {
                self.require_operation(
                    &terminal_id,
                    TerminalOperation::Inspect,
                    authority.as_ref().ok_or_else(authority_denied)?,
                )?;
                let record = self
                    .registry
                    .refresh(&terminal_id)
                    .map_err(process_failure)?;
                Ok(TerminalResponseKind::Terminal {
                    descriptor: self.descriptor(&record)?,
                })
            }
            TerminalRequestKind::Attach {
                terminal_id,
                after_sequence,
                writable,
            } => self.attach(
                terminal_id,
                after_sequence,
                writable,
                authority.as_ref().ok_or_else(authority_denied)?,
            ),
            TerminalRequestKind::ReadEvents {
                stream_id,
                after_sequence,
                maximum_events,
            } => self.read_events(
                stream_id,
                after_sequence,
                maximum_events,
                authority.as_ref().ok_or_else(authority_denied)?,
            ),
            TerminalRequestKind::Input {
                terminal_id,
                stream_id,
                bytes,
            } => {
                self.require_operation(
                    &terminal_id,
                    TerminalOperation::Write,
                    authority.as_ref().ok_or_else(authority_denied)?,
                )?;
                let streams = self.lock_streams()?;
                let stream = streams.get(&stream_id).ok_or_else(not_found)?;
                if stream.terminal_id != terminal_id || !stream.writable {
                    return Err(authority_denied());
                }
                if !self
                    .registry
                    .write_raw_human_input_if_managed(
                        &stream.execution_id,
                        stream.lease.clone(),
                        bytes.clone(),
                        false,
                    )
                    .map_err(process_failure)?
                {
                    self.process
                        .operator_write(&stream.execution_id, stream.lease.clone(), bytes, false)
                        .map_err(process_failure)?;
                }
                Ok(TerminalResponseKind::Ack)
            }
            TerminalRequestKind::SubmitCommand {
                terminal_id,
                topology_id,
                stream_id,
                expected_command_sequence,
                expected_prompt_generation,
                command,
            } => {
                self.require_operation(
                    &terminal_id,
                    TerminalOperation::Write,
                    authority.as_ref().ok_or_else(authority_denied)?,
                )?;
                let (execution_id, lease) = self
                    .lock_streams()?
                    .get(&stream_id)
                    .filter(|stream| stream.terminal_id == terminal_id && stream.writable)
                    .map(|stream| (stream.execution_id.clone(), stream.lease.clone()))
                    .ok_or_else(authority_denied)?;
                let admission = self
                    .registry
                    .admit_human_command(
                        &topology_id,
                        &terminal_id,
                        expected_command_sequence,
                        expected_prompt_generation,
                        &command,
                    )
                    .map_err(process_failure)?;
                if admission.execution_id != execution_id {
                    let _ = self
                        .registry
                        .cancel_human_command_admission(&terminal_id, admission.command_sequence);
                    return Err(authority_denied());
                }
                if let Err(error) = self.registry.write_admitted_human_command(
                    &terminal_id,
                    &execution_id,
                    admission.command_sequence,
                    lease,
                    admission.submission,
                ) {
                    let _ = self
                        .registry
                        .cancel_human_command_admission(&terminal_id, admission.command_sequence);
                    return Err(process_failure(error));
                }
                Ok(TerminalResponseKind::CommandAccepted {
                    command_sequence: admission.command_sequence,
                    output_after_sequence: admission.output_after_sequence,
                })
            }
            TerminalRequestKind::CancelCommand {
                terminal_id,
                command_sequence,
            } => {
                self.require_operation(
                    &terminal_id,
                    TerminalOperation::Write,
                    authority.as_ref().ok_or_else(authority_denied)?,
                )?;
                self.registry
                    .cancel_human_command_admission(&terminal_id, command_sequence)
                    .map_err(process_failure)?;
                Ok(TerminalResponseKind::Ack)
            }
            TerminalRequestKind::Resize { terminal_id, size } => {
                self.require_operation(
                    &terminal_id,
                    TerminalOperation::Resize,
                    authority.as_ref().ok_or_else(authority_denied)?,
                )?;
                let record = self
                    .registry
                    .record(&terminal_id)
                    .map_err(process_failure)?;
                self.process
                    .operator_resize(&record.execution_id, size)
                    .map_err(process_failure)?;
                Ok(TerminalResponseKind::Ack)
            }
            TerminalRequestKind::Detach { stream_id } => {
                let terminal_id = self
                    .lock_streams()?
                    .get(&stream_id)
                    .map(|stream| stream.terminal_id.clone())
                    .ok_or_else(not_found)?;
                self.require_operation(
                    &terminal_id,
                    TerminalOperation::Attach,
                    authority.as_ref().ok_or_else(authority_denied)?,
                )?;
                let stream = self
                    .lock_streams()?
                    .remove(&stream_id)
                    .ok_or_else(not_found)?;
                self.process
                    .operator_detach(&stream.execution_id, stream.lease)
                    .map_err(process_failure)?;
                Ok(TerminalResponseKind::Ack)
            }
            TerminalRequestKind::Terminate { terminal_id } => {
                self.require_operation(
                    &terminal_id,
                    TerminalOperation::Terminate,
                    authority.as_ref().ok_or_else(authority_denied)?,
                )?;
                self.registry
                    .terminate_terminal(&terminal_id, KillMode::Graceful)
                    .map_err(process_failure)?;
                Ok(TerminalResponseKind::Ack)
            }
        }
    }

    fn start_execution(
        &self,
        admission: ExecutionAdmission,
        authority_fingerprint: &agl_exec::AuthorityFingerprint,
    ) -> std::result::Result<TerminalResponseKind, TerminalFailure> {
        admission.validate().map_err(protocol_failure)?;
        if &admission.authority_fingerprint != authority_fingerprint {
            return Err(authority_denied());
        }
        let mut fingerprints = self.lock_execution_fingerprints()?;
        if let Some(execution_id) = fingerprints.get(&admission.request_fingerprint) {
            self.require_execution_authority(execution_id, authority_fingerprint)?;
            let status = self
                .process
                .operator_status(execution_id)
                .map_err(process_failure)?;
            return Ok(TerminalResponseKind::Execution { status });
        }
        let request_fingerprint = admission.request_fingerprint;
        let authority_fingerprint = admission.authority_fingerprint;
        let operations = admission.operations;
        let status = self
            .process
            .start(admission.request)
            .map_err(process_failure)?;
        self.lock_execution_admissions()?.insert(
            status.execution_id.clone(),
            ExecutionAdmissionState {
                authority_fingerprint,
                operations,
            },
        );
        fingerprints.insert(request_fingerprint, status.execution_id.clone());
        Ok(TerminalResponseKind::Execution { status })
    }

    fn ensure(
        &self,
        admission: TerminalAdmission,
        authority_fingerprint: &agl_exec::AuthorityFingerprint,
    ) -> std::result::Result<TerminalResponseKind, TerminalFailure> {
        admission.validate().map_err(protocol_failure)?;
        if &admission.authority_fingerprint != authority_fingerprint {
            return Err(authority_denied());
        }
        let history_seed = TerminalHistorySeed::from_commands(admission.history_seed.clone())
            .map_err(process_failure)?;
        let request = TerminalEnsureRequest {
            topology_id: admission.topology_id,
            owner: admission.owner,
            authority_scope: admission.authority_scope,
            correlation: admission.correlation,
            context: admission.context,
            profile: admission.profile,
            shell: admission.shell,
            environment: admission.environment,
            runtime_read_only_roots: admission.runtime_read_only_roots,
            host_startup: admission.host_startup,
            authorization: admission.authorization,
            grant_lease: admission.grant_lease,
            terminal_size: admission.terminal_size,
            limits: admission.limits,
            history_seed,
        };
        let record = self
            .registry
            .ensure_terminal(request)
            .map_err(process_failure)?;
        self.lock_admissions()?.insert(
            record.terminal_id.clone(),
            AdmissionState {
                authority_fingerprint: admission.authority_fingerprint,
                operations: admission.operations,
            },
        );
        Ok(TerminalResponseKind::Terminal {
            descriptor: self.descriptor(&record)?,
        })
    }

    fn attach(
        &self,
        terminal_id: TerminalId,
        after_sequence: u64,
        writable: bool,
        authority_fingerprint: &agl_exec::AuthorityFingerprint,
    ) -> std::result::Result<TerminalResponseKind, TerminalFailure> {
        self.require_operation(
            &terminal_id,
            TerminalOperation::Attach,
            authority_fingerprint,
        )?;
        if writable {
            self.require_operation(
                &terminal_id,
                TerminalOperation::Write,
                authority_fingerprint,
            )?;
        }
        let record = self
            .registry
            .refresh(&terminal_id)
            .map_err(process_failure)?;
        let lease = self
            .process
            .operator_attach(
                &record.execution_id,
                ExecutionRequestId::generate(),
                writable,
            )
            .map_err(process_failure)?;
        let stream_id = TerminalStreamId::generate();
        self.lock_streams()?.insert(
            stream_id.clone(),
            StreamState {
                terminal_id,
                execution_id: record.execution_id.clone(),
                lease,
                writable,
            },
        );
        Ok(TerminalResponseKind::Attached {
            descriptor: self.descriptor(&record)?,
            stream_id,
            next_sequence: after_sequence,
            writable,
        })
    }

    fn read_events(
        &self,
        stream_id: TerminalStreamId,
        after_sequence: u64,
        maximum_events: u16,
        authority_fingerprint: &agl_exec::AuthorityFingerprint,
    ) -> std::result::Result<TerminalResponseKind, TerminalFailure> {
        let streams = self.lock_streams()?;
        let stream = streams.get(&stream_id).ok_or_else(not_found)?;
        self.require_operation(
            &stream.terminal_id,
            TerminalOperation::Read,
            authority_fingerprint,
        )?;
        let maximum_bytes = usize::from(maximum_events)
            .saturating_mul(64 * 1024)
            .min(MAX_TERMINAL_FRAME_BYTES);
        let read = self
            .process
            .operator_read(
                &stream.execution_id,
                ExecutionCursor { after_sequence },
                maximum_bytes,
            )
            .map_err(process_failure)?;
        let events = read
            .chunks
            .into_iter()
            .take(usize::from(maximum_events).min(MAX_TERMINAL_EVENT_BATCH))
            .map(|chunk| TerminalEvent {
                schema: TERMINAL_EVENT_SCHEMA.to_owned(),
                stream_id: stream_id.clone(),
                sequence: chunk.sequence,
                event: TerminalEventKind::Output { bytes: chunk.bytes },
            })
            .collect();
        Ok(TerminalResponseKind::Events {
            batch: TerminalEventBatch {
                stream_id,
                events,
                next_sequence: read.next_sequence,
                stream_closed: read.state.is_terminal(),
            },
        })
    }

    fn descriptor(
        &self,
        record: &TerminalRecord,
    ) -> std::result::Result<TerminalDescriptor, TerminalFailure> {
        let admissions = self.lock_admissions()?;
        let admission = admissions.get(&record.terminal_id).ok_or_else(not_found)?;
        let status = self
            .process
            .operator_status(&record.execution_id)
            .map_err(process_failure)?;
        Ok(TerminalDescriptor {
            terminal_id: record.terminal_id.clone(),
            execution_id: record.execution_id.clone(),
            owner: record.owner.caller().clone(),
            authority_fingerprint: admission.authority_fingerprint.clone(),
            profile: record.profile,
            service_generation: self.identity.generation_id.clone(),
            state: record.state,
            command_sequence: record.command_sequence,
            output_sequence: status.last_sequence,
        })
    }

    fn require_operation(
        &self,
        terminal_id: &TerminalId,
        operation: TerminalOperation,
        authority_fingerprint: &agl_exec::AuthorityFingerprint,
    ) -> std::result::Result<(), TerminalFailure> {
        let admissions = self.lock_admissions()?;
        let admission = admissions.get(terminal_id).ok_or_else(not_found)?;
        if &admission.authority_fingerprint == authority_fingerprint
            && admission.operations.contains(&operation)
        {
            Ok(())
        } else {
            Err(authority_denied())
        }
    }

    fn require_execution_authority(
        &self,
        execution_id: &agl_exec::ExecutionId,
        authority_fingerprint: &agl_exec::AuthorityFingerprint,
    ) -> std::result::Result<(), TerminalFailure> {
        let admissions = self.lock_execution_admissions()?;
        let admission = admissions.get(execution_id).ok_or_else(not_found)?;
        if &admission.authority_fingerprint == authority_fingerprint {
            Ok(())
        } else {
            Err(authority_denied())
        }
    }

    fn require_execution_operation(
        &self,
        execution_id: &agl_exec::ExecutionId,
        operation: ExecutionOperation,
        authority_fingerprint: &agl_exec::AuthorityFingerprint,
    ) -> std::result::Result<(), TerminalFailure> {
        let admissions = self.lock_execution_admissions()?;
        let admission = admissions.get(execution_id).ok_or_else(not_found)?;
        if &admission.authority_fingerprint == authority_fingerprint
            && admission.operations.contains(&operation)
        {
            Ok(())
        } else {
            Err(authority_denied())
        }
    }

    fn lock_admissions(
        &self,
    ) -> std::result::Result<MutexGuard<'_, BTreeMap<TerminalId, AdmissionState>>, TerminalFailure>
    {
        self.admissions.lock().map_err(|_| internal_failure())
    }

    fn lock_streams(
        &self,
    ) -> std::result::Result<MutexGuard<'_, BTreeMap<TerminalStreamId, StreamState>>, TerminalFailure>
    {
        self.streams.lock().map_err(|_| internal_failure())
    }

    fn lock_execution_admissions(
        &self,
    ) -> std::result::Result<
        MutexGuard<'_, BTreeMap<agl_exec::ExecutionId, ExecutionAdmissionState>>,
        TerminalFailure,
    > {
        self.execution_admissions
            .lock()
            .map_err(|_| internal_failure())
    }

    fn lock_execution_fingerprints(
        &self,
    ) -> std::result::Result<MutexGuard<'_, BTreeMap<String, agl_exec::ExecutionId>>, TerminalFailure>
    {
        self.execution_fingerprints
            .lock()
            .map_err(|_| internal_failure())
    }
}

impl EmbeddedTerminalService for TerminalService {
    fn handle<'a>(
        &'a self,
        request: TerminalRequest,
    ) -> BoxFuture<'a, std::result::Result<TerminalResponse, TransportError>> {
        async move { Ok(self.handle_request(request)) }.boxed()
    }
}

pub async fn serve_unix(
    service: Arc<TerminalService>,
    socket_path: &Path,
    cancellation: CancellationToken,
) -> std::io::Result<()> {
    if !socket_path.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "terminal socket path must be absolute",
        ));
    }
    let listener = UnixListener::bind(socket_path)?;
    loop {
        let (stream, _) = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Ok(()),
            accepted = listener.accept() => accepted?,
        };
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            let _ = serve_connection(service, stream).await;
        });
    }
}

async fn serve_connection(
    service: Arc<TerminalService>,
    mut stream: UnixStream,
) -> std::io::Result<()> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).await?;
    let length = usize::try_from(u32::from_be_bytes(length)).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid terminal frame length",
        )
    })?;
    if length > MAX_TERMINAL_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "terminal request exceeds the frame bound",
        ));
    }
    let mut encoded = vec![0u8; length];
    stream.read_exact(&mut encoded).await?;
    let request = TerminalRequest::decode_json(&encoded)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    let response = service.handle_request(request);
    let encoded = serde_json::to_vec(&response).map_err(std::io::Error::other)?;
    let length = u32::try_from(encoded.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "terminal response is too large",
        )
    })?;
    stream.write_all(&length.to_be_bytes()).await?;
    stream.write_all(&encoded).await?;
    stream.flush().await
}

fn protocol_failure(error: ProtocolValidationError) -> TerminalFailure {
    TerminalFailure {
        code: TerminalFailureCode::InvalidRequest,
        message: error.to_string(),
        retryable: false,
    }
}

fn identity_failure(_error: ProtocolValidationError) -> TerminalFailure {
    TerminalFailure {
        code: TerminalFailureCode::IdentityMismatch,
        message: "terminal service identity does not match exactly".to_owned(),
        retryable: false,
    }
}

fn process_failure(error: ProcessError) -> TerminalFailure {
    let code = match error.code() {
        ProcessErrorCode::ExecutionNotFound => TerminalFailureCode::NotFound,
        ProcessErrorCode::StateConflict
        | ProcessErrorCode::InputLeaseBusy
        | ProcessErrorCode::InputLeaseExpired => TerminalFailureCode::StateConflict,
        ProcessErrorCode::GrantExpired => TerminalFailureCode::AuthorityExpired,
        ProcessErrorCode::GrantRevoked => TerminalFailureCode::AuthorityRevoked,
        ProcessErrorCode::HostAuthorityRequired | ProcessErrorCode::InvalidRequest => {
            TerminalFailureCode::AuthorityDenied
        }
        ProcessErrorCode::InputBackpressure => TerminalFailureCode::Backpressure,
        _ => TerminalFailureCode::Internal,
    };
    TerminalFailure {
        code,
        message: format!("terminal operation failed with {}", error.code().as_str()),
        retryable: matches!(
            error.code(),
            ProcessErrorCode::InputBackpressure | ProcessErrorCode::SupervisorShutdown
        ),
    }
}

fn not_found() -> TerminalFailure {
    TerminalFailure {
        code: TerminalFailureCode::NotFound,
        message: "terminal resource was not found".to_owned(),
        retryable: false,
    }
}

fn authority_denied() -> TerminalFailure {
    TerminalFailure {
        code: TerminalFailureCode::AuthorityDenied,
        message: "terminal operation is not present in the immutable grant".to_owned(),
        retryable: false,
    }
}

fn internal_failure() -> TerminalFailure {
    TerminalFailure {
        code: TerminalFailureCode::Internal,
        message: "terminal service state is unavailable".to_owned(),
        retryable: true,
    }
}
