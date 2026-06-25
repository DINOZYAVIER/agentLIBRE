use agl_protocol::{ProtocolError, ProtocolErrorCode};

pub(crate) fn busy_error() -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::Busy, "session is busy", true)
}

pub(crate) fn not_found_error(session_id: &str) -> ProtocolError {
    ProtocolError::new(
        ProtocolErrorCode::NotFound,
        format!("session {session_id} was not found"),
        false,
    )
}

pub(crate) fn runtime_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::RuntimeFailure, error.to_string(), false)
}

pub(crate) fn invalid_request_error(error: impl std::fmt::Display) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, error.to_string(), false)
}
