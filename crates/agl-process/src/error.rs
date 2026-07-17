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
