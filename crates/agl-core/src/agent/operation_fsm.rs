use std::fmt;
use std::num::NonZeroU32;

use crate::{Fsm, Transition};

pub use super::operation::AgentOperationDeliveryState;
use super::{
    AgentEventData, AgentOperationDispatch, AgentOperationFailure, AgentOperationFailureKind,
    AgentOperationKey, AgentOperationKind, AgentOperationRequest, AgentOperationResult,
    AgentOperationTerminal, AgentOperationTerminalStatus, DeliveryClass,
};

pub type AgentOperationFsmState = AgentOperationDeliveryState;

#[derive(Clone, Debug, PartialEq)]
pub enum AgentOperationFsmInput {
    Start {
        key: AgentOperationKey,
        delivery_attempt: NonZeroU32,
        request: AgentOperationRequest,
    },
    Result {
        key: AgentOperationKey,
        delivery_attempt: NonZeroU32,
        result: AgentOperationResult,
    },
    Failure {
        key: AgentOperationKey,
        delivery_attempt: NonZeroU32,
        failure: AgentOperationFailure,
        retry_at_ms: Option<i64>,
    },
    RetryDue {
        key: AgentOperationKey,
        delivery_attempt: NonZeroU32,
        request: AgentOperationRequest,
    },
    Cancel {
        key: AgentOperationKey,
        delivery_attempt: NonZeroU32,
    },
    Recover {
        key: AgentOperationKey,
        delivery_attempt: NonZeroU32,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentOperationFsmOutput {
    pub dispatch: Option<AgentOperationDispatch>,
    pub terminal: Option<AgentOperationTerminal>,
    pub events: Vec<AgentEventData>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentOperationFsmError {
    IllegalTransition,
    IdentityMismatch,
    RequestMismatch,
    RequestKindMismatch,
    ResultKindMismatch,
    DeliveryAttemptOverflow,
    InvalidRetryTime,
}

impl fmt::Display for AgentOperationFsmError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::IllegalTransition => "illegal AgentOperation transition",
            Self::IdentityMismatch => "operation identity differs from the FSM owner",
            Self::RequestMismatch => "operation request differs from the FSM owner",
            Self::RequestKindMismatch => "operation request kind differs from the FSM kind",
            Self::ResultKindMismatch => "operation result kind differs from the FSM kind",
            Self::DeliveryAttemptOverflow => "delivery attempt overflow",
            Self::InvalidRetryTime => "retry timestamp must be nonnegative UTC milliseconds",
        })
    }
}

impl std::error::Error for AgentOperationFsmError {}

#[derive(Clone, Debug)]
pub struct AgentOperationFsm {
    key: AgentOperationKey,
    request: AgentOperationRequest,
    kind: AgentOperationKind,
    delivery: DeliveryClass,
}

impl AgentOperationFsm {
    pub fn for_operation(operation: &super::AgentOperation) -> Self {
        Self {
            key: operation.key.clone(),
            request: operation.request.clone(),
            kind: operation.kind(),
            delivery: operation.delivery,
        }
    }

    fn output() -> AgentOperationFsmOutput {
        AgentOperationFsmOutput {
            dispatch: None,
            terminal: None,
            events: Vec::new(),
        }
    }

    fn request_kind(request: &AgentOperationRequest) -> AgentOperationKind {
        match request {
            AgentOperationRequest::ModelGeneration(_) => AgentOperationKind::ModelGeneration,
            AgentOperationRequest::Compaction(_) => AgentOperationKind::Compaction,
            AgentOperationRequest::Tool(_) => AgentOperationKind::Tool,
        }
    }

    fn result_kind(result: &AgentOperationResult) -> AgentOperationKind {
        match result {
            AgentOperationResult::ModelGeneration(_) => AgentOperationKind::ModelGeneration,
            AgentOperationResult::Compaction(_) => AgentOperationKind::Compaction,
            AgentOperationResult::Tool(_) => AgentOperationKind::Tool,
        }
    }

    fn ensure_request_kind(
        &self,
        request: &AgentOperationRequest,
    ) -> Result<(), AgentOperationFsmError> {
        (Self::request_kind(request) == self.kind)
            .then_some(())
            .ok_or(AgentOperationFsmError::RequestKindMismatch)
    }

    fn ensure_result_kind(
        &self,
        result: &AgentOperationResult,
    ) -> Result<(), AgentOperationFsmError> {
        (Self::result_kind(result) == self.kind)
            .then_some(())
            .ok_or(AgentOperationFsmError::ResultKindMismatch)
    }

    fn validate_input(&self, input: &AgentOperationFsmInput) -> Result<(), AgentOperationFsmError> {
        let (key, request) = match input {
            AgentOperationFsmInput::Start { key, request, .. }
            | AgentOperationFsmInput::RetryDue { key, request, .. } => (key, Some(request)),
            AgentOperationFsmInput::Result { key, .. }
            | AgentOperationFsmInput::Failure { key, .. }
            | AgentOperationFsmInput::Cancel { key, .. }
            | AgentOperationFsmInput::Recover { key, .. } => (key, None),
        };
        if key != &self.key {
            return Err(AgentOperationFsmError::IdentityMismatch);
        }
        if request.is_some_and(|request| request != &self.request) {
            return Err(AgentOperationFsmError::RequestMismatch);
        }
        Ok(())
    }
}

impl Fsm for AgentOperationFsm {
    type State = AgentOperationFsmState;
    type Input = AgentOperationFsmInput;
    type Output = AgentOperationFsmOutput;
    type Error = AgentOperationFsmError;

    fn transition(
        &self,
        state: &Self::State,
        input: Self::Input,
    ) -> Result<Transition<Self::State, Self::Output>, Self::Error> {
        self.validate_input(&input)?;
        let mut output = Self::output();
        let next = match (*state, input) {
            (
                AgentOperationDeliveryState::Pending,
                AgentOperationFsmInput::Start {
                    key,
                    delivery_attempt,
                    request,
                },
            ) => {
                self.ensure_request_kind(&request)?;
                output.dispatch = Some(AgentOperationDispatch {
                    key: key.clone(),
                    delivery_attempt,
                    request,
                });
                output.events.push(AgentEventData::OperationStarted {
                    key,
                    delivery_attempt,
                });
                AgentOperationDeliveryState::Running
            }
            (
                AgentOperationDeliveryState::RetryScheduled,
                AgentOperationFsmInput::RetryDue {
                    key,
                    delivery_attempt,
                    request,
                },
            ) => {
                self.ensure_request_kind(&request)?;
                let delivery_attempt = NonZeroU32::new(
                    delivery_attempt
                        .get()
                        .checked_add(1)
                        .ok_or(AgentOperationFsmError::DeliveryAttemptOverflow)?,
                )
                .expect("incremented delivery attempt remains nonzero");
                output.dispatch = Some(AgentOperationDispatch {
                    key: key.clone(),
                    delivery_attempt,
                    request,
                });
                output.events.push(AgentEventData::OperationStarted {
                    key,
                    delivery_attempt,
                });
                AgentOperationDeliveryState::Running
            }
            (
                AgentOperationDeliveryState::Running,
                AgentOperationFsmInput::Result {
                    key,
                    delivery_attempt,
                    result,
                },
            ) => {
                self.ensure_result_kind(&result)?;
                output.terminal = Some(AgentOperationTerminal {
                    key: key.clone(),
                    status: AgentOperationTerminalStatus::Succeeded,
                    result: Some(result),
                    failure: None,
                });
                output.events.push(AgentEventData::OperationCompleted {
                    key,
                    delivery_attempt,
                    status: AgentOperationTerminalStatus::Succeeded,
                });
                AgentOperationDeliveryState::Succeeded
            }
            (
                AgentOperationDeliveryState::Running,
                AgentOperationFsmInput::Failure {
                    key,
                    delivery_attempt,
                    failure:
                        AgentOperationFailure {
                            kind: AgentOperationFailureKind::OutcomeUnknown,
                        },
                    ..
                },
            ) => {
                output.events.push(AgentEventData::OperationOutcomeUnknown {
                    key,
                    delivery_attempt,
                });
                AgentOperationDeliveryState::OutcomeUnknown
            }
            (
                AgentOperationDeliveryState::Running,
                AgentOperationFsmInput::Failure {
                    key,
                    delivery_attempt,
                    failure: _,
                    retry_at_ms: Some(retry_at_ms),
                },
            ) if self.delivery == DeliveryClass::Retryable
                && self.kind != AgentOperationKind::Tool
                && retry_at_ms >= 0 =>
            {
                output.events.push(AgentEventData::OperationRetryScheduled {
                    key,
                    delivery_attempt,
                    retry_at_ms,
                });
                AgentOperationDeliveryState::RetryScheduled
            }
            (
                AgentOperationDeliveryState::Running,
                AgentOperationFsmInput::Failure {
                    retry_at_ms: Some(retry_at_ms),
                    ..
                },
            ) if retry_at_ms < 0 => return Err(AgentOperationFsmError::InvalidRetryTime),
            (
                AgentOperationDeliveryState::Running,
                AgentOperationFsmInput::Failure {
                    key,
                    delivery_attempt,
                    failure,
                    retry_at_ms: _,
                },
            ) => {
                output.terminal = Some(AgentOperationTerminal {
                    key: key.clone(),
                    status: AgentOperationTerminalStatus::Failed,
                    result: None,
                    failure: Some(failure),
                });
                output.events.push(AgentEventData::OperationCompleted {
                    key,
                    delivery_attempt,
                    status: AgentOperationTerminalStatus::Failed,
                });
                AgentOperationDeliveryState::Failed
            }
            (
                AgentOperationDeliveryState::Pending
                | AgentOperationDeliveryState::Running
                | AgentOperationDeliveryState::RetryScheduled,
                AgentOperationFsmInput::Cancel {
                    key,
                    delivery_attempt,
                },
            ) => {
                output.terminal = Some(AgentOperationTerminal {
                    key: key.clone(),
                    status: AgentOperationTerminalStatus::Cancelled,
                    result: None,
                    failure: None,
                });
                output.events.push(AgentEventData::OperationCompleted {
                    key,
                    delivery_attempt,
                    status: AgentOperationTerminalStatus::Cancelled,
                });
                AgentOperationDeliveryState::Cancelled
            }
            (
                AgentOperationDeliveryState::Running,
                AgentOperationFsmInput::Recover {
                    key,
                    delivery_attempt,
                },
            ) if self.delivery == DeliveryClass::AtMostOnce
                || self.kind == AgentOperationKind::Tool =>
            {
                output.events.push(AgentEventData::OperationOutcomeUnknown {
                    key,
                    delivery_attempt,
                });
                AgentOperationDeliveryState::OutcomeUnknown
            }
            (
                AgentOperationDeliveryState::Running,
                AgentOperationFsmInput::Recover {
                    key,
                    delivery_attempt,
                },
            ) if self.delivery == DeliveryClass::Retryable => {
                output.events.push(AgentEventData::OperationRetryScheduled {
                    key,
                    delivery_attempt,
                    retry_at_ms: 0,
                });
                AgentOperationDeliveryState::RetryScheduled
            }
            _ => return Err(AgentOperationFsmError::IllegalTransition),
        };
        Ok(Transition {
            state: next,
            output,
        })
    }
}
