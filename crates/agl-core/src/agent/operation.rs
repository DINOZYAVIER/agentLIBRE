use std::num::NonZeroU32;

use crate::Content;
use crate::{AgentRunId, MessageId};
use serde::{Deserialize, Serialize};

use crate::{CanonicalJson, EffectId, ToolId};

use super::{InferenceEngineBuildDigest, InferenceRuntimeProfileDigest, PhysicalResourceDigest};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOperationKey {
    pub run_id: AgentRunId,
    pub ordinal: NonZeroU32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryClass {
    Retryable,
    AtMostOnce,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOperationKind {
    ModelGeneration,
    Compaction,
    Tool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelGenerationRequest {
    pub context: Vec<MessageId>,
    pub max_output_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRequest {
    pub tool_id: ToolId,
    pub input: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AgentOperationRequest {
    ModelGeneration(ModelGenerationRequest),
    Compaction(super::CompactionRequest),
    Tool(ToolRequest),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFinishReason {
    Stop,
    Length,
    ToolCall,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub tool_id: ToolId,
    pub input: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantToolCall {
    pub content: Content,
    pub call: ToolCall,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ModelGenerationOutput {
    Assistant(Content),
    ToolCall(ToolCall),
    ToolCalls(Vec<ToolCall>),
    AssistantToolCall(AssistantToolCall),
    AssistantToolCalls {
        content: Content,
        calls: Vec<ToolCall>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceRealizationRef {
    pub runtime_profile_digest: InferenceRuntimeProfileDigest,
    pub engine_build_digest: InferenceEngineBuildDigest,
    pub physical_resource_digest: PhysicalResourceDigest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelGenerationResult {
    pub output: ModelGenerationOutput,
    pub private_reasoning: Option<Content>,
    pub finish_reason: ModelFinishReason,
    pub usage: ModelUsage,
    pub realization: InferenceRealizationRef,
    pub correction: Option<Box<ModelCorrectionRecord>>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCorrectionRecord {
    pub model: super::ModelDefinitionRef,
    pub attempts: u32,
    pub usage: ModelUsage,
    pub realization: InferenceRealizationRef,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectReceipt {
    pub effect: EffectId,
    pub scope: CanonicalJson,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    pub content: Content,
    pub effect_receipts: Vec<EffectReceipt>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFailureKind {
    /// The rejected input class; `ToolFailure.effect` separately establishes effect state.
    InvalidInput,
    InvalidResult,
    ResultTooLarge,
    /// The authority failure class; this alone does not prove absence of an effect.
    Unauthorized,
    Unavailable,
    Deadline,
    Cancelled,
    Execution,
    OutcomeUnknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolFailure {
    pub kind: ToolFailureKind,
    pub effect: ToolFailureEffect,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolFailureEffect {
    /// The provider proves that this request performed no effect.
    None {
        field: Option<String>,
        next_actions: Vec<String>,
        details: Option<crate::CanonicalJson>,
    },
    Unknown,
}

impl ToolFailure {
    pub fn no_effect(kind: ToolFailureKind, field: Option<&str>, next_actions: &[&str]) -> Self {
        Self {
            kind,
            effect: ToolFailureEffect::None {
                field: field.map(str::to_owned),
                next_actions: next_actions
                    .iter()
                    .map(|value| (*value).to_owned())
                    .collect(),
                details: None,
            },
        }
    }

    pub fn no_effect_with_details(
        kind: ToolFailureKind,
        field: Option<&str>,
        next_actions: &[&str],
        details: crate::CanonicalJson,
    ) -> Self {
        Self {
            kind,
            effect: ToolFailureEffect::None {
                field: field.map(str::to_owned),
                next_actions: next_actions
                    .iter()
                    .map(|value| (*value).to_owned())
                    .collect(),
                details: Some(details),
            },
        }
    }

    pub fn unknown(kind: ToolFailureKind) -> Self {
        Self {
            kind,
            effect: ToolFailureEffect::Unknown,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "result",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AgentOperationResult {
    ModelGeneration(ModelGenerationResult),
    Compaction(Box<super::CompactionResult>),
    Tool(ToolResult),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOperationFailureKind {
    InvalidInput,
    InvalidResult,
    InvalidModelOutput {
        usage: ModelUsage,
    },
    InvalidCompactionOutput {
        usage: ModelUsage,
    },
    CorrectionFailed {
        primary_usage: ModelUsage,
        calls: u64,
        input_tokens: u64,
        output_tokens: u64,
    },
    IdentityMismatch,
    ResultTooLarge,
    ContextExhausted(super::ContextExhaustion),
    CompactionRequired(super::ContextCapacity),
    Unauthorized,
    Unavailable,
    Deadline,
    Cancelled,
    Execution,
    OutcomeUnknown,
    ToolLoopDetected,
}

pub const INVALID_MODEL_OUTPUT_CORRECTION: &str = r#"{"kind":"invalid_model_output","instruction":"The model returned no valid public action. Produce one assistant answer, one admitted Tool call, or one admitted Tool call with adjacent assistant text."}"#;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOperationFailure {
    pub kind: AgentOperationFailureKind,
}

impl From<ToolFailure> for AgentOperationFailure {
    fn from(failure: ToolFailure) -> Self {
        if failure.effect == ToolFailureEffect::Unknown {
            return Self {
                kind: AgentOperationFailureKind::OutcomeUnknown,
            };
        }
        Self {
            kind: match failure.kind {
                ToolFailureKind::InvalidInput => AgentOperationFailureKind::InvalidInput,
                ToolFailureKind::InvalidResult => AgentOperationFailureKind::InvalidResult,
                ToolFailureKind::ResultTooLarge => AgentOperationFailureKind::ResultTooLarge,
                ToolFailureKind::Unauthorized => AgentOperationFailureKind::Unauthorized,
                ToolFailureKind::Unavailable => AgentOperationFailureKind::Unavailable,
                ToolFailureKind::Deadline => AgentOperationFailureKind::Deadline,
                ToolFailureKind::Cancelled => AgentOperationFailureKind::Cancelled,
                ToolFailureKind::Execution => AgentOperationFailureKind::Execution,
                ToolFailureKind::OutcomeUnknown => AgentOperationFailureKind::OutcomeUnknown,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOperation {
    pub key: AgentOperationKey,
    pub request: AgentOperationRequest,
    pub delivery: DeliveryClass,
    pub delivery_attempt: NonZeroU32,
    pub state: AgentOperationDeliveryState,
    pub result: Option<AgentOperationResult>,
    pub failure: Option<AgentOperationFailure>,
    pub revision: u64,
}

/// Client-neutral read-only projection of the authoritative stored operation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentOperationView {
    pub key: AgentOperationKey,
    pub request: AgentOperationRequest,
    pub delivery: DeliveryClass,
    pub delivery_attempt: NonZeroU32,
    pub state: AgentOperationDeliveryState,
    pub result: Option<AgentOperationResult>,
    pub failure: Option<AgentOperationFailure>,
    pub revision: u64,
}

impl From<AgentOperation> for AgentOperationView {
    fn from(operation: AgentOperation) -> Self {
        Self {
            key: operation.key,
            request: operation.request,
            delivery: operation.delivery,
            delivery_attempt: operation.delivery_attempt,
            state: operation.state,
            result: operation.result,
            failure: operation.failure,
            revision: operation.revision,
        }
    }
}

impl AgentOperation {
    pub fn kind(&self) -> AgentOperationKind {
        match self.request {
            AgentOperationRequest::ModelGeneration(_) => AgentOperationKind::ModelGeneration,
            AgentOperationRequest::Compaction(_) => AgentOperationKind::Compaction,
            AgentOperationRequest::Tool(_) => AgentOperationKind::Tool,
        }
    }

    pub fn apply_fsm_transition(
        &self,
        state: AgentOperationDeliveryState,
        output: &super::AgentOperationFsmOutput,
    ) -> Result<Self, &'static str> {
        let mut next = self.clone();
        next.state = state;
        next.revision = self
            .revision
            .checked_add(1)
            .ok_or("operation revision overflow")?;

        match state {
            AgentOperationDeliveryState::Running => {
                let dispatch = output
                    .dispatch
                    .as_ref()
                    .ok_or("Running transition requires dispatch")?;
                if output.terminal.is_some()
                    || dispatch.key != self.key
                    || dispatch.request != self.request
                {
                    return Err("dispatch does not match operation");
                }
                let expected_attempt = match self.state {
                    AgentOperationDeliveryState::Pending => self.delivery_attempt,
                    AgentOperationDeliveryState::RetryScheduled => NonZeroU32::new(
                        self.delivery_attempt
                            .get()
                            .checked_add(1)
                            .ok_or("delivery attempt overflow")?,
                    )
                    .expect("incremented attempt remains nonzero"),
                    _ => return Err("invalid source state for dispatch"),
                };
                if dispatch.delivery_attempt != expected_attempt {
                    return Err("dispatch attempt does not match operation");
                }
                if output.events.as_slice()
                    != [super::AgentEventData::OperationStarted {
                        key: self.key.clone(),
                        delivery_attempt: expected_attempt,
                    }]
                {
                    return Err("started event does not match operation dispatch");
                }
                next.delivery_attempt = dispatch.delivery_attempt;
                next.result = None;
                next.failure = None;
            }
            AgentOperationDeliveryState::Succeeded
            | AgentOperationDeliveryState::Failed
            | AgentOperationDeliveryState::Cancelled => {
                let terminal = output
                    .terminal
                    .as_ref()
                    .ok_or("terminal transition requires terminal output")?;
                if output.dispatch.is_some() || terminal.key != self.key {
                    return Err("terminal output does not match operation");
                }
                let expected = match state {
                    AgentOperationDeliveryState::Succeeded => {
                        AgentOperationTerminalStatus::Succeeded
                    }
                    AgentOperationDeliveryState::Failed => AgentOperationTerminalStatus::Failed,
                    AgentOperationDeliveryState::Cancelled => {
                        AgentOperationTerminalStatus::Cancelled
                    }
                    _ => unreachable!(),
                };
                if terminal.status != expected {
                    return Err("terminal status does not match delivery state");
                }
                let expected_event = super::AgentEventData::OperationCompleted {
                    key: self.key.clone(),
                    delivery_attempt: self.delivery_attempt,
                    status: expected,
                };
                if output.events.as_slice() != [expected_event] {
                    return Err("completed event does not match operation terminal");
                }
                match state {
                    AgentOperationDeliveryState::Succeeded
                        if terminal.result.is_none() || terminal.failure.is_some() =>
                    {
                        return Err("successful terminal requires only a result");
                    }
                    AgentOperationDeliveryState::Failed
                        if terminal.result.is_some() || terminal.failure.is_none() =>
                    {
                        return Err("failed terminal requires only a failure");
                    }
                    AgentOperationDeliveryState::Cancelled
                        if terminal.result.is_some() || terminal.failure.is_some() =>
                    {
                        return Err("cancelled terminal cannot carry result or failure");
                    }
                    _ => {}
                }
                next.result = terminal.result.clone();
                next.failure = terminal.failure.clone();
            }
            AgentOperationDeliveryState::OutcomeUnknown => {
                if output.dispatch.is_some() || output.terminal.is_some() {
                    return Err("OutcomeUnknown cannot dispatch or produce a normal terminal");
                }
                if output.events.as_slice()
                    != [super::AgentEventData::OperationOutcomeUnknown {
                        key: self.key.clone(),
                        delivery_attempt: self.delivery_attempt,
                    }]
                {
                    return Err("outcome-unknown event does not match operation");
                }
                next.result = None;
                next.failure = Some(AgentOperationFailure {
                    kind: AgentOperationFailureKind::OutcomeUnknown,
                });
            }
            AgentOperationDeliveryState::RetryScheduled => {
                if output.dispatch.is_some() || output.terminal.is_some() {
                    return Err("RetryScheduled cannot dispatch or terminate");
                }
                let matches_retry = matches!(
                    output.events.as_slice(),
                    [super::AgentEventData::OperationRetryScheduled {
                        key,
                        delivery_attempt,
                        retry_at_ms,
                    }] if key == &self.key
                        && delivery_attempt == &self.delivery_attempt
                        && *retry_at_ms >= 0
                );
                if !matches_retry {
                    return Err("retry event does not match operation");
                }
                next.result = None;
                next.failure = None;
            }
            AgentOperationDeliveryState::Pending => {
                return Err("FSM cannot transition back to Pending");
            }
        }
        Ok(next)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOperationDeliveryState {
    Pending,
    Running,
    RetryScheduled,
    Succeeded,
    Failed,
    OutcomeUnknown,
    Cancelled,
}

impl AgentOperationDeliveryState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::OutcomeUnknown | Self::Cancelled
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentOperationDispatch {
    pub key: AgentOperationKey,
    pub delivery_attempt: NonZeroU32,
    pub request: AgentOperationRequest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentOperationTerminalStatus {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentOperationTerminal {
    pub key: AgentOperationKey,
    pub status: AgentOperationTerminalStatus,
    pub result: Option<AgentOperationResult>,
    pub failure: Option<AgentOperationFailure>,
}
