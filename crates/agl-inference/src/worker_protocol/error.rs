use serde::{Deserialize, Serialize};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, WorkerProtocolError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerProtocolErrorCode {
    WorkerUnavailable,
    WorkerUntrusted,
    SpawnFailed,
    TimedOut,
    PeerClosed,
    FrameTooLarge,
    DescriptorLimit,
    MalformedFrame,
    SequenceViolation,
    UnexpectedDescriptors,
    IdentityMismatch,
    UnexpectedMessage,
    PayloadTooLarge,
    InvalidPayload,
    Io,
}

impl WorkerProtocolErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WorkerUnavailable => "worker_unavailable",
            Self::WorkerUntrusted => "worker_untrusted",
            Self::SpawnFailed => "spawn_failed",
            Self::TimedOut => "timed_out",
            Self::PeerClosed => "peer_closed",
            Self::FrameTooLarge => "frame_too_large",
            Self::DescriptorLimit => "descriptor_limit",
            Self::MalformedFrame => "malformed_frame",
            Self::SequenceViolation => "sequence_violation",
            Self::UnexpectedDescriptors => "unexpected_descriptors",
            Self::IdentityMismatch => "identity_mismatch",
            Self::UnexpectedMessage => "unexpected_message",
            Self::PayloadTooLarge => "payload_too_large",
            Self::InvalidPayload => "invalid_payload",
            Self::Io => "io",
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq, Serialize, Deserialize)]
#[error("{code}: {message}", code = .code.as_str())]
#[serde(deny_unknown_fields)]
pub struct WorkerProtocolError {
    code: WorkerProtocolErrorCode,
    message: String,
    /// Private host-side evidence is never serialized onto the worker
    /// protocol and is deliberately excluded from `Display`.
    #[serde(skip)]
    private_log: String,
}

impl WorkerProtocolError {
    pub fn new(code: WorkerProtocolErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            private_log: String::new(),
        }
    }

    pub const fn code(&self) -> WorkerProtocolErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// Private, bounded evidence suitable only for a typed worker-loss log.
    pub fn private_log(&self) -> &str {
        &self.private_log
    }

    pub(crate) fn with_private_log(mut self, log: impl Into<String>) -> Self {
        self.private_log = log.into();
        self
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn last_os_error(code: WorkerProtocolErrorCode, context: &str) -> Self {
        Self::new(
            code,
            format!("{context}: {}", std::io::Error::last_os_error()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_worker_loss_log_is_not_displayed_or_serialized() {
        let secret = "private-stderr-observation";
        let error = WorkerProtocolError::new(WorkerProtocolErrorCode::PeerClosed, "worker exited")
            .with_private_log(secret);

        assert!(!error.to_string().contains(secret));
        let encoded = serde_json::to_string(&error).expect("serialize protocol error");
        assert!(!encoded.contains(secret));
        let decoded: WorkerProtocolError =
            serde_json::from_str(&encoded).expect("deserialize protocol error");
        assert!(decoded.private_log().is_empty());
    }
}
