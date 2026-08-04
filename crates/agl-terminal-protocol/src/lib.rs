use std::collections::BTreeSet;
use std::path::PathBuf;

use agl_exec::{
    AuthorityFingerprint, ExecutionAuthorization, ExecutionContextSnapshot, ExecutionCorrelation,
    ExecutionCursor, ExecutionGrantLease, ExecutionId, ExecutionLimits, ExecutionProfile,
    ExecutionReadResult, ExecutionRequest, ExecutionStatus, InputLease, KillMode, OpaqueOwnerId,
    ProcessBytes, ServiceGenerationId, TerminalSize,
};
use agl_terminal::environment::TerminalEnvironmentRequest;
use agl_terminal::{
    AdmittedShellProfile, HostStartupPolicy, TerminalDescriptor, TerminalId, TerminalOperation,
    TerminalOwner, TerminalRequestId, TerminalStreamId, TerminalTopologyId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

pub const TERMINAL_REQUEST_SCHEMA: &str = "agentlibre.terminal.request.v1alpha";
pub const TERMINAL_RESPONSE_SCHEMA: &str = "agentlibre.terminal.response.v1alpha";
pub const TERMINAL_EVENT_SCHEMA: &str = "agentlibre.terminal.event.v1alpha";
pub const TERMINAL_PROTOCOL_VERSION: u32 = 1;
pub const MAX_TERMINAL_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_TERMINAL_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_TERMINAL_EVENT_BATCH: usize = 256;
pub const MAX_REQUEST_FINGERPRINT_BYTES: usize = 71;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOperation {
    Inspect,
    Read,
    Write,
    Resize,
    Interrupt,
    Terminate,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionAdmission {
    pub authority_fingerprint: AuthorityFingerprint,
    pub request_fingerprint: String,
    pub request: ExecutionRequest,
    pub operations: BTreeSet<ExecutionOperation>,
}

impl ExecutionAdmission {
    pub fn seal_request_fingerprint(&mut self) -> Result<(), ProtocolValidationError> {
        self.request_fingerprint = self.computed_request_fingerprint()?;
        Ok(())
    }

    pub fn computed_request_fingerprint(&self) -> Result<String, ProtocolValidationError> {
        let mut payload = self.clone();
        payload.request_fingerprint.clear();
        sha256_fingerprint(&payload)
    }

    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        validate_fingerprint(&self.request_fingerprint)?;
        if self.request_fingerprint != self.computed_request_fingerprint()? {
            return Err(ProtocolValidationError::RequestFingerprintMismatch);
        }
        self.request.validate().map_err(|_| {
            ProtocolValidationError::InvalidAdmission("execution request is invalid")
        })?;
        if self.operations.is_empty() {
            return Err(ProtocolValidationError::InvalidAdmission(
                "at least one execution operation must be admitted",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceIdentity {
    pub protocol_version: u32,
    pub crate_version: String,
    pub build_id: AuthorityFingerprint,
    pub generation_id: ServiceGenerationId,
}

impl ServiceIdentity {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        if self.protocol_version != TERMINAL_PROTOCOL_VERSION {
            return Err(ProtocolValidationError::ProtocolVersionMismatch);
        }
        if self.crate_version != env!("CARGO_PKG_VERSION") {
            return Err(ProtocolValidationError::CrateVersionMismatch);
        }
        Ok(())
    }

    pub fn require_exact(&self, actual: &Self) -> Result<(), ProtocolValidationError> {
        self.validate()?;
        actual.validate()?;
        if self == actual {
            Ok(())
        } else {
            Err(ProtocolValidationError::ServiceIdentityMismatch)
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalAdmission {
    pub topology_id: TerminalTopologyId,
    pub owner: TerminalOwner,
    pub authority_scope: OpaqueOwnerId,
    pub correlation: ExecutionCorrelation,
    pub authority_fingerprint: AuthorityFingerprint,
    pub request_fingerprint: String,
    pub context: ExecutionContextSnapshot,
    pub profile: ExecutionProfile,
    pub shell: AdmittedShellProfile,
    pub environment: TerminalEnvironmentRequest,
    pub runtime_read_only_roots: Vec<PathBuf>,
    pub host_startup: HostStartupPolicy,
    pub authorization: ExecutionAuthorization,
    pub grant_lease: Option<ExecutionGrantLease>,
    pub terminal_size: TerminalSize,
    pub limits: ExecutionLimits,
    pub history_seed: Vec<String>,
    pub operations: BTreeSet<TerminalOperation>,
}

impl TerminalAdmission {
    pub fn seal_request_fingerprint(&mut self) -> Result<(), ProtocolValidationError> {
        self.request_fingerprint = self.computed_request_fingerprint()?;
        Ok(())
    }

    pub fn computed_request_fingerprint(&self) -> Result<String, ProtocolValidationError> {
        let mut payload = self.clone();
        payload.request_fingerprint.clear();
        sha256_fingerprint(&payload)
    }

    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        validate_fingerprint(&self.request_fingerprint)?;
        if self.request_fingerprint != self.computed_request_fingerprint()? {
            return Err(ProtocolValidationError::RequestFingerprintMismatch);
        }
        self.context.validate().map_err(|_| {
            ProtocolValidationError::InvalidAdmission("execution context is invalid")
        })?;
        self.shell
            .validate()
            .map_err(|_| ProtocolValidationError::InvalidAdmission("shell profile is invalid"))?;
        self.terminal_size
            .validate()
            .map_err(|_| ProtocolValidationError::InvalidAdmission("terminal size is invalid"))?;
        self.limits
            .validate()
            .map_err(|_| ProtocolValidationError::InvalidAdmission("limits are invalid"))?;
        if self.history_seed.len() > 256 {
            return Err(ProtocolValidationError::InvalidAdmission(
                "history seed exceeds 256 commands",
            ));
        }
        if self.operations.is_empty() {
            return Err(ProtocolValidationError::InvalidAdmission(
                "at least one terminal operation must be admitted",
            ));
        }
        Ok(())
    }
}

fn validate_fingerprint(value: &str) -> Result<(), ProtocolValidationError> {
    let digest = value
        .strip_prefix("sha256:")
        .ok_or(ProtocolValidationError::InvalidFingerprint)?;
    if value.len() != MAX_REQUEST_FINGERPRINT_BYTES
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ProtocolValidationError::InvalidFingerprint);
    }
    Ok(())
}

fn sha256_fingerprint(value: &impl Serialize) -> Result<String, ProtocolValidationError> {
    let encoded = serde_json::to_vec(value)
        .map_err(|error| ProtocolValidationError::MalformedJson(error.to_string()))?;
    let digest = Sha256::digest(encoded);
    let mut fingerprint = String::with_capacity(MAX_REQUEST_FINGERPRINT_BYTES);
    fingerprint.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut fingerprint, "{byte:02x}").expect("writing a digest to a String cannot fail");
    }
    Ok(fingerprint)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum TerminalRequestKind {
    Hello,
    StartExecution {
        admission: Box<ExecutionAdmission>,
    },
    InspectExecution {
        execution_id: ExecutionId,
    },
    ReadExecution {
        execution_id: ExecutionId,
        cursor: ExecutionCursor,
        maximum_bytes: u32,
    },
    AttachExecution {
        execution_id: ExecutionId,
        writable: bool,
    },
    DetachExecution {
        execution_id: ExecutionId,
        lease: InputLease,
    },
    WriteExecution {
        execution_id: ExecutionId,
        lease: InputLease,
        bytes: ProcessBytes,
        eof: bool,
    },
    ResizeExecution {
        execution_id: ExecutionId,
        size: TerminalSize,
    },
    InterruptExecution {
        execution_id: ExecutionId,
    },
    TerminateExecution {
        execution_id: ExecutionId,
        mode: KillMode,
    },
    Ensure {
        admission: Box<TerminalAdmission>,
    },
    Inspect {
        terminal_id: TerminalId,
    },
    Attach {
        terminal_id: TerminalId,
        after_sequence: u64,
        writable: bool,
    },
    ReadEvents {
        stream_id: TerminalStreamId,
        after_sequence: u64,
        maximum_events: u16,
    },
    Input {
        terminal_id: TerminalId,
        stream_id: TerminalStreamId,
        bytes: ProcessBytes,
    },
    SubmitCommand {
        terminal_id: TerminalId,
        topology_id: TerminalTopologyId,
        stream_id: TerminalStreamId,
        expected_command_sequence: u64,
        expected_prompt_generation: u64,
        command: String,
    },
    CancelCommand {
        terminal_id: TerminalId,
        command_sequence: u64,
    },
    Resize {
        terminal_id: TerminalId,
        size: TerminalSize,
    },
    Detach {
        stream_id: TerminalStreamId,
    },
    Terminate {
        terminal_id: TerminalId,
    },
}

impl TerminalRequestKind {
    fn validate(&self) -> Result<(), ProtocolValidationError> {
        match self {
            Self::StartExecution { admission } => admission.validate(),
            Self::ReadExecution { maximum_bytes, .. }
                if *maximum_bytes == 0
                    || usize::try_from(*maximum_bytes).unwrap_or(usize::MAX)
                        > MAX_TERMINAL_FRAME_BYTES =>
            {
                Err(ProtocolValidationError::InvalidReadBound)
            }
            Self::WriteExecution { bytes, .. } => bytes
                .decode(MAX_TERMINAL_INPUT_BYTES)
                .map(|_| ())
                .map_err(|_| ProtocolValidationError::InvalidInputBytes),
            Self::ResizeExecution { size, .. } => size
                .validate()
                .map(|_| ())
                .map_err(|_| ProtocolValidationError::InvalidAdmission("terminal size is invalid")),
            Self::Ensure { admission } => admission.validate(),
            Self::ReadEvents { maximum_events, .. }
                if *maximum_events == 0
                    || usize::from(*maximum_events) > MAX_TERMINAL_EVENT_BATCH =>
            {
                Err(ProtocolValidationError::InvalidEventBatchBound)
            }
            Self::Input { bytes, .. } => bytes
                .decode(MAX_TERMINAL_INPUT_BYTES)
                .map(|_| ())
                .map_err(|error| match error.code() {
                    agl_exec::ProcessErrorCode::InputTooLarge => {
                        ProtocolValidationError::InputTooLarge
                    }
                    _ => ProtocolValidationError::InvalidInputBytes,
                }),
            Self::SubmitCommand { command, .. }
                if command.is_empty()
                    || command.len() > agl_terminal::MAX_HUMAN_TERMINAL_COMMAND_BYTES =>
            {
                Err(ProtocolValidationError::InvalidCommand)
            }
            Self::Resize { size, .. } => size
                .validate()
                .map(|_| ())
                .map_err(|_| ProtocolValidationError::InvalidAdmission("terminal size is invalid")),
            Self::Hello
            | Self::InspectExecution { .. }
            | Self::ReadExecution { .. }
            | Self::AttachExecution { .. }
            | Self::DetachExecution { .. }
            | Self::InterruptExecution { .. }
            | Self::TerminateExecution { .. }
            | Self::Inspect { .. }
            | Self::Attach { .. }
            | Self::ReadEvents { .. }
            | Self::SubmitCommand { .. }
            | Self::CancelCommand { .. }
            | Self::Detach { .. }
            | Self::Terminate { .. } => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalRequest {
    pub schema: String,
    pub request_id: TerminalRequestId,
    pub expected_service: ServiceIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authority_fingerprint: Option<AuthorityFingerprint>,
    pub request: TerminalRequestKind,
}

impl TerminalRequest {
    pub fn new(
        expected_service: ServiceIdentity,
        authority_fingerprint: Option<AuthorityFingerprint>,
        request: TerminalRequestKind,
    ) -> Result<Self, ProtocolValidationError> {
        let value = Self {
            schema: TERMINAL_REQUEST_SCHEMA.to_owned(),
            request_id: TerminalRequestId::generate(),
            expected_service,
            authority_fingerprint,
            request,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        if self.schema != TERMINAL_REQUEST_SCHEMA {
            return Err(ProtocolValidationError::SchemaMismatch);
        }
        self.expected_service.validate()?;
        match (&self.request, &self.authority_fingerprint) {
            (TerminalRequestKind::Hello, None) => {}
            (TerminalRequestKind::Hello, Some(_)) => {
                return Err(ProtocolValidationError::UnexpectedAuthority);
            }
            (_, None) => return Err(ProtocolValidationError::MissingAuthority),
            (_, Some(_)) => {}
        }
        if let (TerminalRequestKind::Ensure { admission }, Some(authority_fingerprint)) =
            (&self.request, &self.authority_fingerprint)
            && &admission.authority_fingerprint != authority_fingerprint
        {
            return Err(ProtocolValidationError::AuthorityMismatch);
        }
        if let (TerminalRequestKind::StartExecution { admission }, Some(authority_fingerprint)) =
            (&self.request, &self.authority_fingerprint)
            && &admission.authority_fingerprint != authority_fingerprint
        {
            return Err(ProtocolValidationError::AuthorityMismatch);
        }
        self.request.validate()?;
        validate_frame_size(self)
    }

    pub fn decode_json(bytes: &[u8]) -> Result<Self, ProtocolValidationError> {
        if bytes.len() > MAX_TERMINAL_FRAME_BYTES {
            return Err(ProtocolValidationError::FrameTooLarge);
        }
        let value: Self = serde_json::from_slice(bytes)
            .map_err(|error| ProtocolValidationError::MalformedJson(error.to_string()))?;
        value.validate()?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalFailureCode {
    InvalidRequest,
    IdentityMismatch,
    AuthorityDenied,
    AuthorityExpired,
    AuthorityRevoked,
    NotFound,
    StateConflict,
    StaleGeneration,
    Backpressure,
    Cancelled,
    DeadlineExceeded,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalFailure {
    pub code: TerminalFailureCode,
    pub message: String,
    pub retryable: bool,
}

impl TerminalFailure {
    fn validate(&self) -> Result<(), ProtocolValidationError> {
        if self.message.is_empty()
            || self.message.len() > 1024
            || self.message.chars().any(char::is_control)
        {
            return Err(ProtocolValidationError::InvalidFailureMessage);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum TerminalResponseKind {
    Hello,
    Execution {
        status: ExecutionStatus,
    },
    ExecutionRead {
        read: ExecutionReadResult,
    },
    ExecutionAttached {
        status: ExecutionStatus,
        lease: InputLease,
    },
    CommandAccepted {
        command_sequence: u64,
        output_after_sequence: u64,
    },
    Terminal {
        descriptor: TerminalDescriptor,
    },
    Attached {
        descriptor: TerminalDescriptor,
        stream_id: TerminalStreamId,
        next_sequence: u64,
        writable: bool,
    },
    Events {
        batch: TerminalEventBatch,
    },
    Ack,
    Failure {
        failure: TerminalFailure,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalResponse {
    pub schema: String,
    pub request_id: TerminalRequestId,
    pub service: ServiceIdentity,
    pub response: TerminalResponseKind,
}

impl TerminalResponse {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        if self.schema != TERMINAL_RESPONSE_SCHEMA {
            return Err(ProtocolValidationError::SchemaMismatch);
        }
        self.service.validate()?;
        match &self.response {
            TerminalResponseKind::Events { batch } => batch.validate()?,
            TerminalResponseKind::Failure { failure } => failure.validate()?,
            _ => {}
        }
        validate_frame_size(self)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum TerminalEventKind {
    Snapshot { descriptor: TerminalDescriptor },
    Output { bytes: ProcessBytes },
    StateChanged { descriptor: TerminalDescriptor },
    WriterRevoked,
    StreamClosed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalEvent {
    pub schema: String,
    pub stream_id: TerminalStreamId,
    pub sequence: u64,
    pub event: TerminalEventKind,
}

impl TerminalEvent {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        if self.schema != TERMINAL_EVENT_SCHEMA {
            return Err(ProtocolValidationError::SchemaMismatch);
        }
        validate_frame_size(self)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalEventBatch {
    pub stream_id: TerminalStreamId,
    pub events: Vec<TerminalEvent>,
    pub next_sequence: u64,
    pub stream_closed: bool,
}

impl TerminalEventBatch {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        if self.events.len() > MAX_TERMINAL_EVENT_BATCH {
            return Err(ProtocolValidationError::InvalidEventBatchBound);
        }
        let mut previous = None;
        for event in &self.events {
            event.validate()?;
            if event.stream_id != self.stream_id
                || previous.is_some_and(|sequence| event.sequence <= sequence)
                || event.sequence > self.next_sequence
            {
                return Err(ProtocolValidationError::InvalidEventSequence);
            }
            previous = Some(event.sequence);
        }
        Ok(())
    }
}

fn validate_frame_size<T: Serialize>(value: &T) -> Result<(), ProtocolValidationError> {
    let size = serde_json::to_vec(value)
        .map_err(|error| ProtocolValidationError::MalformedJson(error.to_string()))?
        .len();
    if size > MAX_TERMINAL_FRAME_BYTES {
        Err(ProtocolValidationError::FrameTooLarge)
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ProtocolValidationError {
    #[error("terminal protocol schema does not match")]
    SchemaMismatch,
    #[error("terminal protocol version does not match")]
    ProtocolVersionMismatch,
    #[error("terminal protocol crate version does not match")]
    CrateVersionMismatch,
    #[error("terminal service identity does not match exactly")]
    ServiceIdentityMismatch,
    #[error("terminal protocol frame exceeds one MiB")]
    FrameTooLarge,
    #[error("terminal input exceeds 64 KiB")]
    InputTooLarge,
    #[error("terminal input bytes are not valid for their declared encoding")]
    InvalidInputBytes,
    #[error("terminal event batch bound is invalid")]
    InvalidEventBatchBound,
    #[error("execution read bound is invalid")]
    InvalidReadBound,
    #[error("terminal command is empty or exceeds its bound")]
    InvalidCommand,
    #[error("terminal event sequence is invalid")]
    InvalidEventSequence,
    #[error("terminal request fingerprint is not canonical sha256")]
    InvalidFingerprint,
    #[error("terminal request fingerprint does not match its immutable admission payload")]
    RequestFingerprintMismatch,
    #[error("terminal operation requires an authority fingerprint")]
    MissingAuthority,
    #[error("terminal handshake must not carry an authority fingerprint")]
    UnexpectedAuthority,
    #[error("terminal request authority does not match its admission")]
    AuthorityMismatch,
    #[error("terminal admission is invalid: {0}")]
    InvalidAdmission(&'static str),
    #[error("terminal failure message is invalid")]
    InvalidFailureMessage,
    #[error("terminal protocol JSON is malformed: {0}")]
    MalformedJson(String),
}

#[cfg(test)]
mod tests {
    use agl_exec::{
        CallerNamespace, CallerOwner, CallerOwnerKind, CallerRole, EnvironmentOverride,
        ExecutionIo, ExecutionKind, ExecutionOwner, OpaqueOwnerId, ProcessBytes,
        ShellProfileSnapshot,
    };
    use agl_terminal::AdmittedShellKind;
    use std::collections::BTreeMap;

    use super::*;

    fn service() -> ServiceIdentity {
        ServiceIdentity {
            protocol_version: TERMINAL_PROTOCOL_VERSION,
            crate_version: env!("CARGO_PKG_VERSION").to_owned(),
            build_id: AuthorityFingerprint::new(format!("sha256:{}", "b".repeat(64))).unwrap(),
            generation_id: ServiceGenerationId::generate(),
        }
    }

    fn admission() -> TerminalAdmission {
        let namespace = CallerNamespace::new("agentlibre", 1).unwrap();
        let owner_id = OpaqueOwnerId::new("opaque-owner").unwrap();
        let authority_scope = OpaqueOwnerId::new("authority-scope").unwrap();
        let correlation_group = OpaqueOwnerId::new("correlation-group").unwrap();
        let correlation_request = OpaqueOwnerId::new("correlation-request").unwrap();
        let workspace = std::env::temp_dir().canonicalize().unwrap();
        let shell_snapshot = ShellProfileSnapshot {
            program: PathBuf::from("/bin/bash"),
            command_args: vec!["--noprofile".to_owned(), "-i".to_owned()],
            login_command_args: None,
            environment_names: vec!["PATH".to_owned()],
            executable_digest: format!("sha256:{}", "d".repeat(64)),
            config_digest: format!("sha256:{}", "e".repeat(64)),
        };
        let mut admission = TerminalAdmission {
            topology_id: TerminalTopologyId::new(owner_id.clone()),
            owner: TerminalOwner::new(CallerOwner::new(
                namespace.clone(),
                owner_id,
                CallerOwnerKind::Persistent,
                CallerRole::Human,
            )),
            authority_scope,
            correlation: ExecutionCorrelation::new(
                namespace,
                correlation_group,
                correlation_request,
            ),
            authority_fingerprint: AuthorityFingerprint::new(format!("sha256:{}", "a".repeat(64)))
                .unwrap(),
            request_fingerprint: format!("sha256:{}", "0".repeat(64)),
            context: ExecutionContextSnapshot {
                workspace_root: workspace.clone(),
                working_directory: workspace.clone(),
                private_execution_roots: Vec::new(),
                shell: shell_snapshot.clone(),
                revision: 1,
                profile_metadata: "protocol-test".to_owned(),
            },
            profile: ExecutionProfile::Workspace,
            shell: AdmittedShellProfile {
                kind: AdmittedShellKind::Bash,
                snapshot: shell_snapshot,
            },
            environment: TerminalEnvironmentRequest::default(),
            runtime_read_only_roots: Vec::new(),
            host_startup: HostStartupPolicy::ManagedOnly,
            authorization: ExecutionAuthorization::default(),
            grant_lease: None,
            terminal_size: TerminalSize {
                columns: 120,
                rows: 40,
            },
            limits: ExecutionLimits {
                timeout_ms: Some(1_000),
                max_input_bytes: 64 * 1024,
                max_output_bytes: 1024 * 1024,
            },
            history_seed: Vec::new(),
            operations: BTreeSet::from([TerminalOperation::Inspect, TerminalOperation::Attach]),
        };
        admission.seal_request_fingerprint().unwrap();
        admission
    }

    fn execution_admission() -> ExecutionAdmission {
        let namespace = CallerNamespace::new("agentlibre", 1).unwrap();
        let authority_scope = OpaqueOwnerId::new("execution-authority").unwrap();
        let workspace = std::env::temp_dir().canonicalize().unwrap();
        let mut admission = ExecutionAdmission {
            authority_fingerprint: AuthorityFingerprint::new(format!("sha256:{}", "9".repeat(64)))
                .unwrap(),
            request_fingerprint: format!("sha256:{}", "0".repeat(64)),
            request: ExecutionRequest {
                owner: ExecutionOwner::new(
                    CallerOwner::new(
                        namespace.clone(),
                        OpaqueOwnerId::new("execution-owner").unwrap(),
                        CallerOwnerKind::Ephemeral,
                        CallerRole::Agent,
                    ),
                    authority_scope,
                ),
                correlation: ExecutionCorrelation::new(
                    namespace,
                    OpaqueOwnerId::new("execution-group").unwrap(),
                    OpaqueOwnerId::new("execution-request").unwrap(),
                ),
                kind: ExecutionKind::Argv,
                program: PathBuf::from("/bin/true"),
                argv0: "/bin/true".to_owned(),
                program_digest: None,
                args: Vec::new(),
                workspace_root: workspace.clone(),
                cwd: workspace,
                read_only_roots: Vec::new(),
                environment: EnvironmentOverride {
                    values: BTreeMap::new(),
                },
                stdin: None,
                close_stdin_after_initial: true,
                io: ExecutionIo::Pipes,
                terminal_size: None,
                profile: ExecutionProfile::Workspace,
                authorization: ExecutionAuthorization::default(),
                grant_lease: None,
                limits: ExecutionLimits {
                    timeout_ms: Some(1_000),
                    max_input_bytes: 1024,
                    max_output_bytes: 4096,
                },
            },
            operations: BTreeSet::from([
                ExecutionOperation::Inspect,
                ExecutionOperation::Read,
                ExecutionOperation::Terminate,
            ]),
        };
        admission.seal_request_fingerprint().unwrap();
        admission
    }

    #[test]
    fn request_round_trip_is_bounded_and_strict() {
        let request = TerminalRequest::new(
            service(),
            Some(admission().authority_fingerprint.clone()),
            TerminalRequestKind::Ensure {
                admission: Box::new(admission()),
            },
        )
        .unwrap();
        let encoded = serde_json::to_vec(&request).unwrap();
        assert_eq!(TerminalRequest::decode_json(&encoded).unwrap(), request);

        let mut unknown = serde_json::to_value(request).unwrap();
        unknown
            .as_object_mut()
            .unwrap()
            .insert("legacy".to_owned(), serde_json::json!(true));
        assert!(TerminalRequest::decode_json(&serde_json::to_vec(&unknown).unwrap()).is_err());
    }

    #[test]
    fn exact_service_identity_rejects_generation_and_build_drift() {
        let expected = service();
        let mut actual = expected.clone();
        actual.generation_id = ServiceGenerationId::generate();
        assert_eq!(
            expected.require_exact(&actual),
            Err(ProtocolValidationError::ServiceIdentityMismatch)
        );
        actual = expected.clone();
        actual.build_id = AuthorityFingerprint::new(format!("sha256:{}", "f".repeat(64))).unwrap();
        assert!(expected.require_exact(&actual).is_err());
    }

    #[test]
    fn execution_admission_is_fingerprinted_and_authority_bound() {
        let admission = execution_admission();
        let request = TerminalRequest::new(
            service(),
            Some(admission.authority_fingerprint.clone()),
            TerminalRequestKind::StartExecution {
                admission: Box::new(admission.clone()),
            },
        )
        .unwrap();
        assert_eq!(
            TerminalRequest::decode_json(&serde_json::to_vec(&request).unwrap()).unwrap(),
            request
        );

        let mut mutated = admission;
        mutated.request.args.push("changed".to_owned());
        assert_eq!(
            mutated.validate(),
            Err(ProtocolValidationError::RequestFingerprintMismatch)
        );
    }

    #[test]
    fn input_and_event_batches_are_fail_closed() {
        let bytes = ProcessBytes::from_bytes(&vec![b'x'; MAX_TERMINAL_INPUT_BYTES + 1]);
        let request = TerminalRequest {
            schema: TERMINAL_REQUEST_SCHEMA.to_owned(),
            request_id: TerminalRequestId::generate(),
            expected_service: service(),
            authority_fingerprint: Some(
                AuthorityFingerprint::new(format!("sha256:{}", "a".repeat(64))).unwrap(),
            ),
            request: TerminalRequestKind::Input {
                terminal_id: TerminalId::generate(),
                stream_id: TerminalStreamId::generate(),
                bytes,
            },
        };
        assert_eq!(
            request.validate(),
            Err(ProtocolValidationError::InputTooLarge)
        );

        let stream_id = TerminalStreamId::generate();
        let batch = TerminalEventBatch {
            stream_id: stream_id.clone(),
            events: vec![TerminalEvent {
                schema: TERMINAL_EVENT_SCHEMA.to_owned(),
                stream_id,
                sequence: 2,
                event: TerminalEventKind::StreamClosed,
            }],
            next_sequence: 1,
            stream_closed: true,
        };
        assert_eq!(
            batch.validate(),
            Err(ProtocolValidationError::InvalidEventSequence)
        );
    }
}
