use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroU32;

use crate::{AgentRunId, ConversationId, MessageId};
use serde::{Deserialize, Serialize};

use crate::{CanonicalJson, Fsm, ToolId, Transition};

use super::run::checked_next;
use super::{
    AdmittedTool, AgentCheckpoint, AgentEventData, AgentMessage, AgentOperation,
    AgentOperationDeliveryState, AgentOperationKey, AgentOperationRequest, AgentOperationResult,
    AgentRunFailureKind, AgentRunLimits, AgentRunStatus, AgentRunUsage, DeliveryClass, MessageRole,
    MessageVisibility, ModelFinishReason, ModelGenerationOutput, ModelGenerationRequest,
    ToolRequest,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentFsmState {
    pub status: AgentRunStatus,
    pub checkpoint: AgentCheckpoint,
    pub usage: AgentRunUsage,
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum AgentFsmInput {
    Drive,
    OperationFinished {
        operation: AgentOperation,
        message_id: Option<MessageId>,
    },
    Cancel,
    DeadlineReached,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentFsmOutput {
    pub operation: Option<AgentOperation>,
    pub message: Option<AgentMessage>,
    pub events: Vec<AgentEventData>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentFsmError {
    Terminal,
    NotReady,
    OperationMismatch,
    OperationNotSucceeded,
    MissingMessageId,
    UnexpectedMessageId,
    OrdinalOverflow,
    LimitExceeded,
    InvalidModelResult,
}
impl fmt::Display for AgentFsmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Terminal => "AgentRun is terminal",
                Self::NotReady => "AgentRun is waiting",
                Self::OperationMismatch => "operation does not match checkpoint",
                Self::OperationNotSucceeded => "operation has not succeeded",
                Self::MissingMessageId => "accepted content requires a message ID",
                Self::UnexpectedMessageId => "operation does not produce a message",
                Self::OrdinalOverflow => "operation ordinal overflow",
                Self::LimitExceeded => "AgentRun limit exceeded",
                Self::InvalidModelResult => "model result is structurally inconsistent",
            }
        )
    }
}
impl std::error::Error for AgentFsmError {}

#[derive(Clone, Debug)]
pub struct AgentFsm {
    run_id: AgentRunId,
    conversation_id: Option<ConversationId>,
    limits: AgentRunLimits,
    generation_max_output_tokens: u64,
    tools: BTreeMap<ToolId, DeliveryClass>,
}

impl AgentFsm {
    pub fn new(
        run_id: AgentRunId,
        conversation_id: Option<ConversationId>,
        limits: AgentRunLimits,
        generation_max_output_tokens: u64,
        tools: &[AdmittedTool],
    ) -> Self {
        Self {
            run_id,
            conversation_id,
            limits,
            generation_max_output_tokens,
            tools: tools
                .iter()
                .map(|tool| (tool.definition.id.clone(), tool.definition.delivery))
                .collect(),
        }
    }
    fn output() -> AgentFsmOutput {
        AgentFsmOutput {
            operation: None,
            message: None,
            events: Vec::new(),
        }
    }

    fn queue_compaction(
        &self,
        state: &mut AgentFsmState,
        output: &mut AgentFsmOutput,
        ordinal: NonZeroU32,
        request: super::CompactionRequest,
    ) -> Result<(), AgentFsmError> {
        let context = request.context.clone();
        let operation = self.operation(
            ordinal,
            AgentOperationRequest::Compaction(request),
            DeliveryClass::Retryable,
        );
        state.checkpoint = AgentCheckpoint::Waiting {
            operation: operation.key.clone(),
            context,
            next_operation_ordinal: checked_next(ordinal).ok_or(AgentFsmError::OrdinalOverflow)?,
            pending_tool_calls: Vec::new(),
        };
        output.events.push(AgentEventData::OperationCreated {
            key: operation.key.clone(),
            kind: operation.kind(),
            tool_id: None,
            delivery: operation.delivery,
        });
        output.operation = Some(operation);
        Ok(())
    }

    fn account_model(
        &self,
        usage: &mut AgentRunUsage,
        tokens: Option<super::ModelUsage>,
    ) -> Result<(), AgentFsmError> {
        usage.model_calls = usage
            .model_calls
            .checked_add(1)
            .ok_or(AgentFsmError::LimitExceeded)?;
        if let Some(tokens) = tokens {
            usage.model_input_tokens = usage
                .model_input_tokens
                .checked_add(tokens.input_tokens)
                .ok_or(AgentFsmError::LimitExceeded)?;
            usage.model_output_tokens = usage
                .model_output_tokens
                .checked_add(tokens.output_tokens)
                .ok_or(AgentFsmError::LimitExceeded)?;
        }
        Ok(())
    }

    fn model_budget_available(&self, usage: &AgentRunUsage) -> bool {
        usage.model_calls < self.limits.model_calls
            && self
                .limits
                .model_input_tokens
                .is_none_or(|limit| usage.model_input_tokens < limit)
            && usage.model_output_tokens < self.limits.model_output_tokens
    }
    fn operation(
        &self,
        ordinal: NonZeroU32,
        request: AgentOperationRequest,
        delivery: DeliveryClass,
    ) -> AgentOperation {
        AgentOperation {
            key: AgentOperationKey {
                run_id: self.run_id,
                ordinal,
            },
            request,
            delivery,
            delivery_attempt: NonZeroU32::MIN,
            state: AgentOperationDeliveryState::Pending,
            result: None,
            failure: None,
            revision: 0,
        }
    }
}

impl Fsm for AgentFsm {
    type State = AgentFsmState;
    type Input = AgentFsmInput;
    type Output = AgentFsmOutput;
    type Error = AgentFsmError;

    fn transition(
        &self,
        state: &AgentFsmState,
        input: Self::Input,
    ) -> Result<Transition<AgentFsmState, AgentFsmOutput>, AgentFsmError> {
        if matches!(
            state.status,
            AgentRunStatus::Completed | AgentRunStatus::Failed | AgentRunStatus::Cancelled
        ) {
            return Err(AgentFsmError::Terminal);
        }
        let mut next = state.clone();
        let mut output = Self::output();
        match input {
            AgentFsmInput::Cancel | AgentFsmInput::DeadlineReached => {
                if !matches!(state.checkpoint, AgentCheckpoint::Ready { .. }) {
                    return Err(AgentFsmError::NotReady);
                }
                let from = next.status;
                next.status = if matches!(input, AgentFsmInput::Cancel) {
                    AgentRunStatus::Cancelled
                } else {
                    AgentRunStatus::Failed
                };
                output.events.push(AgentEventData::RunStatusChanged {
                    from,
                    to: next.status,
                    failure_kind: matches!(input, AgentFsmInput::DeadlineReached)
                        .then_some(AgentRunFailureKind::Deadline),
                });
            }
            AgentFsmInput::Drive => {
                let AgentCheckpoint::Ready {
                    context,
                    next_operation_ordinal,
                } = &state.checkpoint
                else {
                    return Err(AgentFsmError::NotReady);
                };
                if state.usage.model_calls >= self.limits.model_calls
                    || self
                        .limits
                        .model_input_tokens
                        .is_some_and(|limit| state.usage.model_input_tokens >= limit)
                    || state.usage.model_output_tokens >= self.limits.model_output_tokens
                {
                    let from = next.status;
                    next.status = AgentRunStatus::Failed;
                    output.events.push(AgentEventData::RunStatusChanged {
                        from,
                        to: next.status,
                        failure_kind: Some(AgentRunFailureKind::LimitsExceeded),
                    });
                    return Ok(Transition {
                        state: next,
                        output,
                    });
                }
                let operation = self.operation(
                    *next_operation_ordinal,
                    AgentOperationRequest::ModelGeneration(ModelGenerationRequest {
                        context: context.clone(),
                        max_output_tokens: self
                            .limits
                            .model_output_tokens
                            .saturating_sub(state.usage.model_output_tokens)
                            .min(self.generation_max_output_tokens),
                    }),
                    DeliveryClass::Retryable,
                );
                let from = next.status;
                next.status = AgentRunStatus::Running;
                next.checkpoint = AgentCheckpoint::Waiting {
                    operation: operation.key.clone(),
                    context: context.clone(),
                    next_operation_ordinal: checked_next(*next_operation_ordinal)
                        .ok_or(AgentFsmError::OrdinalOverflow)?,
                    pending_tool_calls: Vec::new(),
                };
                if from != next.status {
                    output.events.push(AgentEventData::RunStatusChanged {
                        from,
                        to: next.status,
                        failure_kind: None,
                    });
                }
                output.events.push(AgentEventData::OperationCreated {
                    key: operation.key.clone(),
                    kind: operation.kind(),
                    tool_id: None,
                    delivery: operation.delivery,
                });
                output.operation = Some(operation);
            }
            AgentFsmInput::OperationFinished {
                operation,
                message_id,
            } => {
                let AgentCheckpoint::Waiting {
                    operation: expected,
                    context,
                    next_operation_ordinal,
                    pending_tool_calls,
                } = &state.checkpoint
                else {
                    return Err(AgentFsmError::NotReady);
                };
                if &operation.key != expected {
                    return Err(AgentFsmError::OperationMismatch);
                }
                if operation.state != AgentOperationDeliveryState::Succeeded {
                    if let (
                        AgentOperationRequest::ModelGeneration(_),
                        Some(super::AgentOperationFailure {
                            kind: super::AgentOperationFailureKind::CompactionRequired(before),
                        }),
                    ) = (&operation.request, &operation.failure)
                    {
                        if !before.needs_compaction() || message_id.is_some() {
                            return Err(AgentFsmError::InvalidModelResult);
                        }
                        self.queue_compaction(
                            &mut next,
                            &mut output,
                            *next_operation_ordinal,
                            super::CompactionRequest {
                                context: context.clone(),
                                before: *before,
                                correction_of: None,
                            },
                        )?;
                        return Ok(Transition {
                            state: next,
                            output,
                        });
                    }
                    if let (
                        AgentOperationRequest::Compaction(request),
                        Some(super::AgentOperationFailure {
                            kind:
                                super::AgentOperationFailureKind::ContextExhausted(
                                    super::ContextExhaustion {
                                        compaction: Some(failure),
                                        ..
                                    },
                                ),
                        }),
                    ) = (&operation.request, &operation.failure)
                    {
                        if failure.model_called {
                            self.account_model(&mut next.usage, failure.usage)?;
                            output
                                .events
                                .push(AgentEventData::UsageUpdated { usage: next.usage });
                        }
                        if request.correction_of.is_none()
                            && self.model_budget_available(&next.usage)
                            && matches!(
                                failure.stage,
                                super::CompactionFailureStage::SemanticOutput
                                    | super::CompactionFailureStage::RebuiltInput
                            )
                        {
                            let mut correction = request.clone();
                            correction.correction_of = Some(operation.key.clone());
                            self.queue_compaction(
                                &mut next,
                                &mut output,
                                *next_operation_ordinal,
                                correction,
                            )?;
                            return Ok(Transition {
                                state: next,
                                output,
                            });
                        }
                    }
                    if matches!(
                        (&operation.request, operation.failure.as_ref()),
                        (
                            AgentOperationRequest::ModelGeneration(_),
                            Some(super::AgentOperationFailure {
                                kind: super::AgentOperationFailureKind::InvalidResult
                                    | super::AgentOperationFailureKind::InvalidModelOutput { .. }
                                    | super::AgentOperationFailureKind::CorrectionFailed { .. }
                            })
                        )
                    ) {
                        let id = message_id.ok_or(AgentFsmError::MissingMessageId)?;
                        let primary_usage =
                            operation
                                .failure
                                .as_ref()
                                .and_then(|failure| match &failure.kind {
                                    super::AgentOperationFailureKind::InvalidModelOutput {
                                        usage,
                                    }
                                    | super::AgentOperationFailureKind::CorrectionFailed {
                                        primary_usage: usage,
                                        ..
                                    } => Some(*usage),
                                    _ => None,
                                });
                        self.account_model(&mut next.usage, primary_usage)?;
                        if let Some(super::AgentOperationFailure {
                            kind:
                                super::AgentOperationFailureKind::CorrectionFailed {
                                    calls,
                                    input_tokens,
                                    output_tokens,
                                    ..
                                },
                        }) = operation.failure.as_ref()
                        {
                            next.usage.correction_calls = next
                                .usage
                                .correction_calls
                                .checked_add(*calls)
                                .ok_or(AgentFsmError::LimitExceeded)?;
                            next.usage.correction_input_tokens = next
                                .usage
                                .correction_input_tokens
                                .checked_add(*input_tokens)
                                .ok_or(AgentFsmError::LimitExceeded)?;
                            next.usage.correction_output_tokens = next
                                .usage
                                .correction_output_tokens
                                .checked_add(*output_tokens)
                                .ok_or(AgentFsmError::LimitExceeded)?;
                        }
                        let mut new_context = context.clone();
                        new_context.push(id.clone());
                        output.message = Some(AgentMessage {
                            id: id.clone(),
                            conversation_id: self.conversation_id,
                            run_id: Some(self.run_id),
                            source_operation: Some(operation.key.clone()),
                            role: MessageRole::Assistant,
                            visibility: MessageVisibility::Internal,
                            content: crate::Content::text(super::INVALID_MODEL_OUTPUT_CORRECTION)
                                .expect("static correction content is valid"),
                        });
                        output.events.push(AgentEventData::MessageAppended {
                            message_id: id,
                            role: MessageRole::Assistant,
                            visibility: MessageVisibility::Internal,
                        });
                        next.checkpoint = AgentCheckpoint::Ready {
                            context: new_context,
                            next_operation_ordinal: *next_operation_ordinal,
                        };
                        if next.usage.model_calls >= self.limits.model_calls {
                            let from = next.status;
                            next.status = AgentRunStatus::Failed;
                            output.events.push(AgentEventData::RunStatusChanged {
                                from,
                                to: next.status,
                                failure_kind: Some(AgentRunFailureKind::LimitsExceeded),
                            });
                        }
                        output
                            .events
                            .push(AgentEventData::UsageUpdated { usage: next.usage });
                        return Ok(Transition {
                            state: next,
                            output,
                        });
                    }
                    if let (
                        AgentOperationRequest::Compaction(request),
                        Some(super::AgentOperationFailure {
                            kind:
                                super::AgentOperationFailureKind::InvalidCompactionOutput { usage },
                        }),
                    ) = (&operation.request, &operation.failure)
                    {
                        self.account_model(&mut next.usage, Some(*usage))?;
                        output
                            .events
                            .push(AgentEventData::UsageUpdated { usage: next.usage });
                        if request.correction_of.is_none()
                            && self.model_budget_available(&next.usage)
                        {
                            let mut correction = request.clone();
                            correction.correction_of = Some(operation.key.clone());
                            self.queue_compaction(
                                &mut next,
                                &mut output,
                                *next_operation_ordinal,
                                correction,
                            )?;
                            return Ok(Transition {
                                state: next,
                                output,
                            });
                        }
                    }
                    if operation.state.is_terminal() {
                        let from = next.status;
                        next.status = if operation.state == AgentOperationDeliveryState::Cancelled {
                            AgentRunStatus::Cancelled
                        } else {
                            AgentRunStatus::Failed
                        };
                        output.events.push(AgentEventData::RunStatusChanged {
                            from,
                            to: next.status,
                            failure_kind: (next.status == AgentRunStatus::Failed).then_some(
                                match operation
                                    .failure
                                    .as_ref()
                                    .map(|failure| failure.kind.clone())
                                {
                                    Some(super::AgentOperationFailureKind::Deadline) => {
                                        AgentRunFailureKind::Deadline
                                    }
                                    Some(super::AgentOperationFailureKind::ToolLoopDetected) => {
                                        AgentRunFailureKind::ToolLoopDetected
                                    }
                                    _ => AgentRunFailureKind::Operation,
                                },
                            ),
                        });
                        return Ok(Transition {
                            state: next,
                            output,
                        });
                    }
                    return Err(AgentFsmError::OperationNotSucceeded);
                }
                let result = operation
                    .result
                    .as_ref()
                    .ok_or(AgentFsmError::OperationNotSucceeded)?;
                let mut new_context = context.clone();
                match result {
                    AgentOperationResult::Compaction(result) => {
                        let AgentOperationRequest::Compaction(request) = &operation.request else {
                            return Err(AgentFsmError::InvalidModelResult);
                        };
                        let metadata = &result.metadata;
                        let rebuilt_budget = super::CompactionBudgets::new(
                            request.before.context_capacity_tokens,
                            request.before.reserved_output_tokens,
                            false,
                        )
                        .ok_or(AgentFsmError::InvalidModelResult)?;
                        let reconstructed: Vec<_> = metadata
                            .source
                            .iter()
                            .chain(&metadata.tail)
                            .cloned()
                            .collect();
                        if reconstructed != *context
                            || metadata.source.is_empty()
                            || metadata.retained_checkpoints.len() > 9
                            || metadata
                                .retained_checkpoints
                                .iter()
                                .any(|id| !metadata.source.contains(id))
                            || metadata
                                .retained_checkpoints
                                .windows(2)
                                .any(|ids| ids[0] >= ids[1])
                            || metadata
                                .retained_checkpoints
                                .iter()
                                .any(|id| metadata.tail.contains(id))
                            || metadata.source.first() != Some(&metadata.source_start)
                            || metadata.source.last() != Some(&metadata.source_end)
                            || metadata.tail.first() != metadata.tail_first.as_ref()
                            || metadata.summary_operation != operation.key
                            || metadata.correction_of != request.correction_of
                            || metadata.before != request.before
                            || metadata.after
                                != super::ContextCapacity::new(
                                    metadata.after.prompt_tokens,
                                    request.before.reserved_output_tokens,
                                    request.before.context_capacity_tokens,
                                )
                            || metadata.summary_request
                                != super::ContextCapacity::new(
                                    metadata.summary_request.prompt_tokens,
                                    rebuilt_budget.summary_output_tokens,
                                    request.before.context_capacity_tokens,
                                )
                            || !metadata.summary_request.fits()
                            || metadata.usage.output_tokens > rebuilt_budget.summary_output_tokens
                            || result.content.exact.snapshot_run != self.run_id
                            || metadata.tail_tokens > rebuilt_budget.recent_tail_limit
                            || !metadata.after.fits()
                            || metadata.after.prompt_tokens > rebuilt_budget.rebuilt_input_target
                            || result.content.semantic.validate(&metadata.source).is_err()
                        {
                            return Err(AgentFsmError::InvalidModelResult);
                        }
                        let id = message_id.ok_or(AgentFsmError::MissingMessageId)?;
                        self.account_model(&mut next.usage, Some(metadata.usage))?;
                        output.message = Some(AgentMessage {
                            id: id.clone(),
                            conversation_id: self.conversation_id,
                            run_id: Some(self.run_id),
                            source_operation: Some(operation.key.clone()),
                            role: MessageRole::Assistant,
                            visibility: MessageVisibility::Internal,
                            content: result
                                .content
                                .render()
                                .map_err(|_| AgentFsmError::InvalidModelResult)?,
                        });
                        output.events.push(AgentEventData::MessageAppended {
                            message_id: id.clone(),
                            role: MessageRole::Assistant,
                            visibility: MessageVisibility::Internal,
                        });
                        let mut rebuilt = metadata.retained_checkpoints.clone();
                        rebuilt.push(id);
                        rebuilt.extend(metadata.tail.clone());
                        next.checkpoint = AgentCheckpoint::Ready {
                            context: rebuilt,
                            next_operation_ordinal: *next_operation_ordinal,
                        };
                    }
                    AgentOperationResult::ModelGeneration(result) => {
                        next.usage.model_calls = next
                            .usage
                            .model_calls
                            .checked_add(1)
                            .ok_or(AgentFsmError::LimitExceeded)?;
                        next.usage.model_input_tokens = next
                            .usage
                            .model_input_tokens
                            .checked_add(result.usage.input_tokens)
                            .ok_or(AgentFsmError::LimitExceeded)?;
                        next.usage.model_output_tokens = next
                            .usage
                            .model_output_tokens
                            .checked_add(result.usage.output_tokens)
                            .ok_or(AgentFsmError::LimitExceeded)?;
                        if let Some(correction) = &result.correction {
                            next.usage.correction_calls = next
                                .usage
                                .correction_calls
                                .checked_add(u64::from(correction.attempts))
                                .ok_or(AgentFsmError::LimitExceeded)?;
                            next.usage.correction_input_tokens = next
                                .usage
                                .correction_input_tokens
                                .checked_add(correction.usage.input_tokens)
                                .ok_or(AgentFsmError::LimitExceeded)?;
                            next.usage.correction_output_tokens = next
                                .usage
                                .correction_output_tokens
                                .checked_add(correction.usage.output_tokens)
                                .ok_or(AgentFsmError::LimitExceeded)?;
                        }
                        let limits_exceeded = next.usage.model_calls > self.limits.model_calls
                            || self
                                .limits
                                .model_input_tokens
                                .is_some_and(|limit| next.usage.model_input_tokens > limit)
                            || next.usage.model_output_tokens > self.limits.model_output_tokens;
                        let limits_exceeded = limits_exceeded
                            || next.usage.correction_calls > self.limits.correction_calls
                            || next.usage.correction_input_tokens
                                > self.limits.correction_input_tokens
                            || next.usage.correction_output_tokens
                                > self.limits.correction_output_tokens;
                        match &result.output {
                            ModelGenerationOutput::Assistant(content) => {
                                if result.finish_reason == ModelFinishReason::ToolCall {
                                    return Err(AgentFsmError::InvalidModelResult);
                                }
                                let id = message_id.ok_or(AgentFsmError::MissingMessageId)?;
                                new_context.push(id.clone());
                                output.message = Some(AgentMessage {
                                    id: id.clone(),
                                    conversation_id: self.conversation_id,
                                    run_id: Some(self.run_id),
                                    source_operation: Some(operation.key.clone()),
                                    role: MessageRole::Assistant,
                                    visibility: if self.conversation_id.is_some() {
                                        MessageVisibility::Conversation
                                    } else {
                                        MessageVisibility::Internal
                                    },
                                    content: content.clone(),
                                });
                                output.events.push(AgentEventData::MessageAppended {
                                    message_id: id,
                                    role: MessageRole::Assistant,
                                    visibility: if self.conversation_id.is_some() {
                                        MessageVisibility::Conversation
                                    } else {
                                        MessageVisibility::Internal
                                    },
                                });
                                if limits_exceeded {
                                    let from = next.status;
                                    next.status = AgentRunStatus::Failed;
                                    output.events.push(AgentEventData::RunStatusChanged {
                                        from,
                                        to: next.status,
                                        failure_kind: Some(AgentRunFailureKind::LimitsExceeded),
                                    });
                                } else if result.finish_reason == ModelFinishReason::Stop {
                                    let from = next.status;
                                    next.status = AgentRunStatus::Completed;
                                    output.events.push(AgentEventData::RunStatusChanged {
                                        from,
                                        to: next.status,
                                        failure_kind: None,
                                    });
                                }
                                next.checkpoint = AgentCheckpoint::Ready {
                                    context: new_context,
                                    next_operation_ordinal: *next_operation_ordinal,
                                };
                            }
                            ModelGenerationOutput::ToolCall(_)
                            | ModelGenerationOutput::ToolCalls(_)
                            | ModelGenerationOutput::AssistantToolCall(_)
                            | ModelGenerationOutput::AssistantToolCalls { .. } => {
                                if result.finish_reason != ModelFinishReason::ToolCall {
                                    return Err(AgentFsmError::InvalidModelResult);
                                }
                                let (mut calls, assistant) = match &result.output {
                                    ModelGenerationOutput::ToolCall(call) => {
                                        (vec![call.clone()], None)
                                    }
                                    ModelGenerationOutput::ToolCalls(calls) => {
                                        (calls.clone(), None)
                                    }
                                    ModelGenerationOutput::AssistantToolCall(mixed) => {
                                        (vec![mixed.call.clone()], Some(&mixed.content))
                                    }
                                    ModelGenerationOutput::AssistantToolCalls {
                                        content,
                                        calls,
                                    } => (calls.clone(), Some(content)),
                                    ModelGenerationOutput::Assistant(_) => unreachable!(),
                                };
                                if calls.is_empty() {
                                    return Err(AgentFsmError::InvalidModelResult);
                                }
                                if let Some(content) = assistant {
                                    let id = message_id
                                        .clone()
                                        .ok_or(AgentFsmError::MissingMessageId)?;
                                    new_context.push(id.clone());
                                    let visibility = if self.conversation_id.is_some() {
                                        MessageVisibility::Conversation
                                    } else {
                                        MessageVisibility::Internal
                                    };
                                    output.message = Some(AgentMessage {
                                        id: id.clone(),
                                        conversation_id: self.conversation_id,
                                        run_id: Some(self.run_id),
                                        source_operation: Some(operation.key.clone()),
                                        role: MessageRole::Assistant,
                                        visibility,
                                        content: content.clone(),
                                    });
                                    output.events.push(AgentEventData::MessageAppended {
                                        message_id: id,
                                        role: MessageRole::Assistant,
                                        visibility,
                                    });
                                } else if message_id.is_some() {
                                    return Err(AgentFsmError::UnexpectedMessageId);
                                }
                                if limits_exceeded
                                    || state.usage.tool_calls >= self.limits.tool_calls
                                {
                                    let from = next.status;
                                    next.status = AgentRunStatus::Failed;
                                    next.checkpoint = AgentCheckpoint::Ready {
                                        context: new_context,
                                        next_operation_ordinal: *next_operation_ordinal,
                                    };
                                    output.events.push(AgentEventData::RunStatusChanged {
                                        from,
                                        to: next.status,
                                        failure_kind: Some(AgentRunFailureKind::LimitsExceeded),
                                    });
                                    output
                                        .events
                                        .push(AgentEventData::UsageUpdated { usage: next.usage });
                                    return Ok(Transition {
                                        state: next,
                                        output,
                                    });
                                }
                                let tool = self.operation(
                                    *next_operation_ordinal,
                                    AgentOperationRequest::Tool(ToolRequest {
                                        tool_id: calls[0].tool_id.clone(),
                                        input: CanonicalJson::new(calls[0].input.clone())
                                            .map_err(|_| AgentFsmError::InvalidModelResult)?
                                            .into_value(),
                                    }),
                                    *self
                                        .tools
                                        .get(&calls[0].tool_id)
                                        .ok_or(AgentFsmError::InvalidModelResult)?,
                                );
                                next.checkpoint = AgentCheckpoint::Waiting {
                                    operation: tool.key.clone(),
                                    context: new_context,
                                    next_operation_ordinal: checked_next(*next_operation_ordinal)
                                        .ok_or(
                                        AgentFsmError::OrdinalOverflow,
                                    )?,
                                    pending_tool_calls: calls.split_off(1),
                                };
                                output.events.push(AgentEventData::OperationCreated {
                                    key: tool.key.clone(),
                                    kind: tool.kind(),
                                    tool_id: Some(calls[0].tool_id.clone()),
                                    delivery: tool.delivery,
                                });
                                output.operation = Some(tool);
                            }
                        }
                    }
                    AgentOperationResult::Tool(result) => {
                        let id = message_id.ok_or(AgentFsmError::MissingMessageId)?;
                        next.usage.tool_calls = next
                            .usage
                            .tool_calls
                            .checked_add(1)
                            .ok_or(AgentFsmError::LimitExceeded)?;
                        let limits_exceeded = next.usage.tool_calls > self.limits.tool_calls;
                        new_context.push(id.clone());
                        output.message = Some(AgentMessage {
                            id: id.clone(),
                            conversation_id: self.conversation_id,
                            run_id: Some(self.run_id),
                            source_operation: Some(operation.key.clone()),
                            role: MessageRole::Tool,
                            visibility: MessageVisibility::Internal,
                            content: result.content.clone(),
                        });
                        output.events.push(AgentEventData::MessageAppended {
                            message_id: id,
                            role: MessageRole::Tool,
                            visibility: MessageVisibility::Internal,
                        });
                        if limits_exceeded {
                            let from = next.status;
                            next.status = AgentRunStatus::Failed;
                            output.events.push(AgentEventData::RunStatusChanged {
                                from,
                                to: next.status,
                                failure_kind: Some(AgentRunFailureKind::LimitsExceeded),
                            });
                            next.checkpoint = AgentCheckpoint::Ready {
                                context: new_context,
                                next_operation_ordinal: *next_operation_ordinal,
                            };
                        } else if let Some(call) = pending_tool_calls.first() {
                            let tool = self.operation(
                                *next_operation_ordinal,
                                AgentOperationRequest::Tool(ToolRequest {
                                    tool_id: call.tool_id.clone(),
                                    input: CanonicalJson::new(call.input.clone())
                                        .map_err(|_| AgentFsmError::InvalidModelResult)?
                                        .into_value(),
                                }),
                                *self
                                    .tools
                                    .get(&call.tool_id)
                                    .ok_or(AgentFsmError::InvalidModelResult)?,
                            );
                            next.checkpoint = AgentCheckpoint::Waiting {
                                operation: tool.key.clone(),
                                context: new_context,
                                next_operation_ordinal: checked_next(*next_operation_ordinal)
                                    .ok_or(AgentFsmError::OrdinalOverflow)?,
                                pending_tool_calls: pending_tool_calls[1..].to_vec(),
                            };
                            output.events.push(AgentEventData::OperationCreated {
                                key: tool.key.clone(),
                                kind: tool.kind(),
                                tool_id: Some(call.tool_id.clone()),
                                delivery: tool.delivery,
                            });
                            output.operation = Some(tool);
                        } else {
                            next.checkpoint = AgentCheckpoint::Ready {
                                context: new_context,
                                next_operation_ordinal: *next_operation_ordinal,
                            };
                        }
                    }
                }
                output
                    .events
                    .push(AgentEventData::UsageUpdated { usage: next.usage });
            }
        }
        Ok(Transition {
            state: next,
            output,
        })
    }
}
