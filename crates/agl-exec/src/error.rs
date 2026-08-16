use serde::{Deserialize, Serialize};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, ProcessError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessErrorCode {
    PlatformUnsupported,
    LauncherUnavailable,
    LauncherProtocol,
    SandboxUnavailable,
    SandboxExecutableUnavailable,
    HostAuthorityRequired,
    LoginAuthorityRequired,
    GrantRevoked,
    GrantExpired,
    Cancelled,
    TimedOut,
    ActiveLimitReached,
    SpawnFailed,
    InvalidRequest,
    InvalidBytes,
    InputTooLarge,
    InputBackpressure,
    InvalidTerminalSize,
    ExecutionNotFound,
    ExecutionNotOwned,
    ExecutionNotLive,
    IoModeMismatch,
    InputLeaseBusy,
    InputLeaseExpired,
    OutputExpired,
    OutputLimitExceeded,
    SupervisorShutdown,
    StateConflict,
    StoreCorrupt,
    Internal,
}

impl ProcessErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PlatformUnsupported => "platform_unsupported",
            Self::LauncherUnavailable => "launcher_unavailable",
            Self::LauncherProtocol => "launcher_protocol",
            Self::SandboxUnavailable => "sandbox_unavailable",
            Self::SandboxExecutableUnavailable => "sandbox_executable_unavailable",
            Self::HostAuthorityRequired => "host_authority_required",
            Self::LoginAuthorityRequired => "login_authority_required",
            Self::GrantRevoked => "grant_revoked",
            Self::GrantExpired => "grant_expired",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::ActiveLimitReached => "active_limit_reached",
            Self::SpawnFailed => "spawn_failed",
            Self::InvalidRequest => "invalid_request",
            Self::InvalidBytes => "invalid_bytes",
            Self::InputTooLarge => "input_too_large",
            Self::InputBackpressure => "input_backpressure",
            Self::InvalidTerminalSize => "invalid_terminal_size",
            Self::ExecutionNotFound => "execution_not_found",
            Self::ExecutionNotOwned => "execution_not_owned",
            Self::ExecutionNotLive => "execution_not_live",
            Self::IoModeMismatch => "io_mode_mismatch",
            Self::InputLeaseBusy => "input_lease_busy",
            Self::InputLeaseExpired => "input_lease_expired",
            Self::OutputExpired => "output_expired",
            Self::OutputLimitExceeded => "output_limit_exceeded",
            Self::SupervisorShutdown => "supervisor_shutdown",
            Self::StateConflict => "state_conflict",
            Self::StoreCorrupt => "store_corrupt",
            Self::Internal => "internal",
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq, Serialize, Deserialize)]
#[error("{code}: {message}", code = .code.as_str())]
#[serde(deny_unknown_fields)]
pub struct ProcessError {
    code: ProcessErrorCode,
    message: String,
}

impl ProcessError {
    pub fn new(code: ProcessErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub const fn code(&self) -> ProcessErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODES: [(ProcessErrorCode, &str); 30] = [
        (
            ProcessErrorCode::PlatformUnsupported,
            "platform_unsupported",
        ),
        (
            ProcessErrorCode::LauncherUnavailable,
            "launcher_unavailable",
        ),
        (ProcessErrorCode::LauncherProtocol, "launcher_protocol"),
        (ProcessErrorCode::SandboxUnavailable, "sandbox_unavailable"),
        (
            ProcessErrorCode::SandboxExecutableUnavailable,
            "sandbox_executable_unavailable",
        ),
        (
            ProcessErrorCode::HostAuthorityRequired,
            "host_authority_required",
        ),
        (
            ProcessErrorCode::LoginAuthorityRequired,
            "login_authority_required",
        ),
        (ProcessErrorCode::GrantRevoked, "grant_revoked"),
        (ProcessErrorCode::GrantExpired, "grant_expired"),
        (ProcessErrorCode::Cancelled, "cancelled"),
        (ProcessErrorCode::TimedOut, "timed_out"),
        (ProcessErrorCode::ActiveLimitReached, "active_limit_reached"),
        (ProcessErrorCode::SpawnFailed, "spawn_failed"),
        (ProcessErrorCode::InvalidRequest, "invalid_request"),
        (ProcessErrorCode::InvalidBytes, "invalid_bytes"),
        (ProcessErrorCode::InputTooLarge, "input_too_large"),
        (ProcessErrorCode::InputBackpressure, "input_backpressure"),
        (
            ProcessErrorCode::InvalidTerminalSize,
            "invalid_terminal_size",
        ),
        (ProcessErrorCode::ExecutionNotFound, "execution_not_found"),
        (ProcessErrorCode::ExecutionNotOwned, "execution_not_owned"),
        (ProcessErrorCode::ExecutionNotLive, "execution_not_live"),
        (ProcessErrorCode::IoModeMismatch, "io_mode_mismatch"),
        (ProcessErrorCode::InputLeaseBusy, "input_lease_busy"),
        (ProcessErrorCode::InputLeaseExpired, "input_lease_expired"),
        (ProcessErrorCode::OutputExpired, "output_expired"),
        (
            ProcessErrorCode::OutputLimitExceeded,
            "output_limit_exceeded",
        ),
        (ProcessErrorCode::SupervisorShutdown, "supervisor_shutdown"),
        (ProcessErrorCode::StateConflict, "state_conflict"),
        (ProcessErrorCode::StoreCorrupt, "store_corrupt"),
        (ProcessErrorCode::Internal, "internal"),
    ];

    #[test]
    fn error_codes_retain_their_wire_values() {
        for (code, expected) in CODES {
            assert_eq!(code.as_str(), expected);
        }
    }

    #[test]
    fn error_schema_round_trips_and_rejects_unknown_fields() {
        let error = ProcessError::new(ProcessErrorCode::TimedOut, "execution timed out");
        let encoded = serde_json::to_value(&error).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({
                "code": "timed_out",
                "message": "execution timed out"
            })
        );
        assert_eq!(
            serde_json::from_value::<ProcessError>(encoded).unwrap(),
            error
        );
        assert!(
            serde_json::from_value::<ProcessError>(serde_json::json!({
                "code": "timed_out",
                "message": "execution timed out",
                "retry": true
            }))
            .is_err()
        );
        assert_eq!(error.to_string(), "timed_out: execution timed out");
    }
}
