use std::collections::BTreeSet;
use std::path::PathBuf;

use agl_exec::{
    AuthorityFingerprint, CallerOwner, ExecutionLimits, ExecutionProfile, ProcessBytes,
    ServiceGenerationId, ShellProfileSnapshot, TerminalSize,
};
use agl_terminal::{
    TerminalDescriptor, TerminalId, TerminalOperation, TerminalRequestId, TerminalStreamId,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const TERMINAL_REQUEST_SCHEMA: &str = "agentlibre.terminal.request.v1alpha";
pub const TERMINAL_RESPONSE_SCHEMA: &str = "agentlibre.terminal.response.v1alpha";
pub const TERMINAL_EVENT_SCHEMA: &str = "agentlibre.terminal.event.v1alpha";
pub const TERMINAL_PROTOCOL_VERSION: u32 = 1;
pub const MAX_TERMINAL_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_TERMINAL_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_TERMINAL_EVENT_BATCH: usize = 256;
pub const MAX_REQUEST_FINGERPRINT_BYTES: usize = 71;

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
    pub owner: CallerOwner,
    pub authority_fingerprint: AuthorityFingerprint,
    pub request_fingerprint: String,
    pub profile: ExecutionProfile,
    pub workspace_root: PathBuf,
    pub cwd: PathBuf,
    pub shell: ShellProfileSnapshot,
    pub terminal_size: TerminalSize,
    pub limits: ExecutionLimits,
    pub operations: BTreeSet<TerminalOperation>,
}

impl TerminalAdmission {
    pub fn validate(&self) -> Result<(), ProtocolValidationError> {
        validate_fingerprint(&self.request_fingerprint)?;
        if !self.workspace_root.is_absolute() || !self.cwd.is_absolute() {
            return Err(ProtocolValidationError::InvalidAdmission(
                "workspace root and cwd must be absolute",
            ));
        }
        if self.profile == ExecutionProfile::Workspace
            && !self.cwd.starts_with(&self.workspace_root)
        {
            return Err(ProtocolValidationError::InvalidAdmission(
                "workspace cwd must remain inside the admitted workspace root",
            ));
        }
        self.shell
            .validate()
            .map_err(|_| ProtocolValidationError::InvalidAdmission("shell profile is invalid"))?;
        self.terminal_size
            .validate()
            .map_err(|_| ProtocolValidationError::InvalidAdmission("terminal size is invalid"))?;
        self.limits
            .validate()
            .map_err(|_| ProtocolValidationError::InvalidAdmission("limits are invalid"))?;
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum TerminalRequestKind {
    Hello,
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
            Self::Resize { size, .. } => size
                .validate()
                .map(|_| ())
                .map_err(|_| ProtocolValidationError::InvalidAdmission("terminal size is invalid")),
            Self::Hello
            | Self::Inspect { .. }
            | Self::Attach { .. }
            | Self::ReadEvents { .. }
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
    pub request: TerminalRequestKind,
}

impl TerminalRequest {
    pub fn new(
        expected_service: ServiceIdentity,
        request: TerminalRequestKind,
    ) -> Result<Self, ProtocolValidationError> {
        let value = Self {
            schema: TERMINAL_REQUEST_SCHEMA.to_owned(),
            request_id: TerminalRequestId::generate(),
            expected_service,
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
                || event.sequence >= self.next_sequence
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
    #[error("terminal event sequence is invalid")]
    InvalidEventSequence,
    #[error("terminal request fingerprint is not canonical sha256")]
    InvalidFingerprint,
    #[error("terminal admission is invalid: {0}")]
    InvalidAdmission(&'static str),
    #[error("terminal failure message is invalid")]
    InvalidFailureMessage,
    #[error("terminal protocol JSON is malformed: {0}")]
    MalformedJson(String),
}

#[cfg(test)]
mod tests {
    use agl_exec::{CallerNamespace, CallerOwnerKind, CallerRole, OpaqueOwnerId, ProcessBytes};

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
        TerminalAdmission {
            owner: CallerOwner::new(
                CallerNamespace::new("agentlibre", 1).unwrap(),
                OpaqueOwnerId::new("opaque-owner").unwrap(),
                CallerOwnerKind::Persistent,
                CallerRole::Human,
            ),
            authority_fingerprint: AuthorityFingerprint::new(format!("sha256:{}", "a".repeat(64)))
                .unwrap(),
            request_fingerprint: format!("sha256:{}", "c".repeat(64)),
            profile: ExecutionProfile::Workspace,
            workspace_root: PathBuf::from("/workspace"),
            cwd: PathBuf::from("/workspace/project"),
            shell: ShellProfileSnapshot {
                program: PathBuf::from("/bin/bash"),
                command_args: vec!["--noprofile".to_owned(), "-i".to_owned()],
                login_command_args: None,
                environment_names: vec!["PATH".to_owned()],
                executable_digest: format!("sha256:{}", "d".repeat(64)),
                config_digest: format!("sha256:{}", "e".repeat(64)),
            },
            terminal_size: TerminalSize {
                columns: 120,
                rows: 40,
            },
            limits: ExecutionLimits {
                timeout_ms: Some(1_000),
                max_input_bytes: 64 * 1024,
                max_output_bytes: 1024 * 1024,
            },
            operations: BTreeSet::from([TerminalOperation::Inspect, TerminalOperation::Attach]),
        }
    }

    #[test]
    fn request_round_trip_is_bounded_and_strict() {
        let request = TerminalRequest::new(
            service(),
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
    fn input_and_event_batches_are_fail_closed() {
        let bytes = ProcessBytes::from_bytes(&vec![b'x'; MAX_TERMINAL_INPUT_BYTES + 1]);
        let request = TerminalRequest {
            schema: TERMINAL_REQUEST_SCHEMA.to_owned(),
            request_id: TerminalRequestId::generate(),
            expected_service: service(),
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
            next_sequence: 2,
            stream_closed: true,
        };
        assert_eq!(
            batch.validate(),
            Err(ProtocolValidationError::InvalidEventSequence)
        );
    }
}
