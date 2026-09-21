use super::*;

pub(crate) fn map_inference_failure(error: InferenceServiceError) -> AgentOperationFailure {
    AgentOperationFailure {
        kind: match error {
            InferenceServiceError::InvalidRequest => AgentOperationFailureKind::InvalidInput,
            InferenceServiceError::ContextExhausted(capacity) => {
                AgentOperationFailureKind::ContextExhausted(capacity.into())
            }
            InferenceServiceError::Cancelled => AgentOperationFailureKind::Cancelled,
            InferenceServiceError::Deadline => AgentOperationFailureKind::Deadline,
            InferenceServiceError::OutcomeUnknown => AgentOperationFailureKind::OutcomeUnknown,
            InferenceServiceError::Stopped
            | InferenceServiceError::Unavailable
            | InferenceServiceError::UnavailableWithReason(_)
            | InferenceServiceError::DeviceLost => AgentOperationFailureKind::Unavailable,
            InferenceServiceError::IdentityMismatch => AgentOperationFailureKind::IdentityMismatch,
            InferenceServiceError::InvalidResult => AgentOperationFailureKind::InvalidResult,
            InferenceServiceError::InvalidModelOutput(_) => {
                AgentOperationFailureKind::InvalidResult
            }
        },
    }
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[derive(Debug)]
pub(crate) enum AgentServiceError {
    Stopped,
    Unavailable,
    InvalidBindings,
    InvalidSnapshot,
    InvalidTransition,
    Store(crate::store::StoreError),
    Fsm(String),
}
impl fmt::Display for AgentServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stopped => f.write_str("agent service is stopped"),
            Self::Unavailable => f.write_str("agent service is unavailable"),
            Self::InvalidBindings => f.write_str("invalid Extension bindings"),
            Self::InvalidSnapshot => f.write_str("invalid AgentRun snapshot"),
            Self::InvalidTransition => f.write_str("invalid Agent transition"),
            Self::Store(error) => error.fmt(f),
            Self::Fsm(error) => f.write_str(error),
        }
    }
}
impl std::error::Error for AgentServiceError {}
impl From<crate::store::StoreError> for AgentServiceError {
    fn from(value: crate::store::StoreError) -> Self {
        Self::Store(value)
    }
}
impl From<agl_core::agent::AgentFsmError> for AgentServiceError {
    fn from(value: agl_core::agent::AgentFsmError) -> Self {
        Self::Fsm(value.to_string())
    }
}
impl From<agl_core::agent::AgentOperationFsmError> for AgentServiceError {
    fn from(value: agl_core::agent::AgentOperationFsmError) -> Self {
        Self::Fsm(value.to_string())
    }
}
