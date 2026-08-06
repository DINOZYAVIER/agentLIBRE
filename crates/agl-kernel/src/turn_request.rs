use std::fmt;

use crate::{HookBatchRequest, HookBatchResult};
use crate::{
    ModelRequest, ModelResponse, ToolDispatchRequest, ToolDispatchResponse, TurnMessage, TurnOutput,
};
use agl_events::{EventDraft, RuntimeEvent};
use agl_ids::{MessageId, TurnId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnRequestKey {
    pub turn_id: TurnId,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnRequestKind {
    HookBatch,
    ModelGeneration,
    ToolDispatch,
    TranscriptAppend,
}

impl TurnRequestKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HookBatch => "hook_batch",
            Self::ModelGeneration => "model_generation",
            Self::ToolDispatch => "tool_dispatch",
            Self::TranscriptAppend => "transcript_append",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnRequest {
    HookBatch {
        key: TurnRequestKey,
        request: HookBatchRequest,
    },
    ModelGeneration {
        key: TurnRequestKey,
        provisional_message_id: MessageId,
        request: ModelRequest,
    },
    ToolDispatch {
        key: TurnRequestKey,
        request: ToolDispatchRequest,
    },
    TranscriptAppend {
        key: TurnRequestKey,
        assistant_message_id: Option<MessageId>,
        messages: Vec<TurnMessage>,
        output: TurnOutput,
    },
}

impl TurnRequest {
    pub fn key(&self) -> &TurnRequestKey {
        match self {
            Self::HookBatch { key, .. }
            | Self::ModelGeneration { key, .. }
            | Self::ToolDispatch { key, .. }
            | Self::TranscriptAppend { key, .. } => key,
        }
    }

    pub fn kind(&self) -> TurnRequestKind {
        match self {
            Self::HookBatch { .. } => TurnRequestKind::HookBatch,
            Self::ModelGeneration { .. } => TurnRequestKind::ModelGeneration,
            Self::ToolDispatch { .. } => TurnRequestKind::ToolDispatch,
            Self::TranscriptAppend { .. } => TurnRequestKind::TranscriptAppend,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnRequestFailureCode {
    Hook,
    Inference,
    Tool,
    Transcript,
    Deadline,
    Invariant,
}

impl TurnRequestFailureCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hook => "request.hook_failed",
            Self::Inference => "request.inference_failed",
            Self::Tool => "request.tool_failed",
            Self::Transcript => "request.transcript_failed",
            Self::Deadline => "request.deadline_exceeded",
            Self::Invariant => "request.invariant_failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnRequestFailure {
    pub code: TurnRequestFailureCode,
    pub message: String,
    pub retryable: bool,
}

impl TurnRequestFailure {
    pub fn new(code: TurnRequestFailureCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TurnRequestOutcome<T> {
    Succeeded(T),
    Failed(TurnRequestFailure),
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookRequestOutput {
    pub result: HookBatchResult,
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnRequestResult {
    HookBatch {
        key: TurnRequestKey,
        outcome: TurnRequestOutcome<HookRequestOutput>,
    },
    ModelGeneration {
        key: TurnRequestKey,
        outcome: TurnRequestOutcome<ModelResponse>,
    },
    ToolDispatch {
        key: TurnRequestKey,
        outcome: Box<TurnRequestOutcome<ToolDispatchResponse>>,
    },
    TranscriptAppend {
        key: TurnRequestKey,
        outcome: TurnRequestOutcome<()>,
    },
}

impl TurnRequestResult {
    pub fn key(&self) -> &TurnRequestKey {
        match self {
            Self::HookBatch { key, .. }
            | Self::ModelGeneration { key, .. }
            | Self::ToolDispatch { key, .. }
            | Self::TranscriptAppend { key, .. } => key,
        }
    }

    pub fn kind(&self) -> TurnRequestKind {
        match self {
            Self::HookBatch { .. } => TurnRequestKind::HookBatch,
            Self::ModelGeneration { .. } => TurnRequestKind::ModelGeneration,
            Self::ToolDispatch { .. } => TurnRequestKind::ToolDispatch,
            Self::TranscriptAppend { .. } => TurnRequestKind::TranscriptAppend,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnExecutionFailure {
    pub code: TurnRequestFailureCode,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnTerminal {
    Completed { output: TurnOutput },
    Failed { failure: TurnExecutionFailure },
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnAdvanceState {
    Pending { request: TurnRequest },
    Terminal { terminal: TurnTerminal },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnAdvance {
    pub events: Vec<EventDraft<RuntimeEvent>>,
    pub state: TurnAdvanceState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnMachineError {
    InvalidCheckpoint(String),
    NoPendingRequest,
    DuplicateRequestKey(TurnRequestKey),
    StaleRequestKey {
        expected: TurnRequestKey,
        actual: TurnRequestKey,
    },
    MismatchedRequestResult {
        expected: TurnRequestKind,
        actual: TurnRequestKind,
    },
    AlreadyTerminal,
    Transition(String),
}

impl fmt::Display for TurnMachineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCheckpoint(message) => {
                write!(formatter, "invalid turn checkpoint: {message}")
            }
            Self::NoPendingRequest => formatter.write_str("turn executor has no pending request"),
            Self::DuplicateRequestKey(key) => write!(
                formatter,
                "request {} for turn {} was already consumed",
                key.sequence, key.turn_id
            ),
            Self::StaleRequestKey { expected, actual } => write!(
                formatter,
                "request key mismatch: expected {}:{}, got {}:{}",
                expected.turn_id, expected.sequence, actual.turn_id, actual.sequence
            ),
            Self::MismatchedRequestResult { expected, actual } => write!(
                formatter,
                "request result kind mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::AlreadyTerminal => formatter.write_str("turn executor is already terminal"),
            Self::Transition(message) => write!(formatter, "turn transition failed: {message}"),
        }
    }
}

impl std::error::Error for TurnMachineError {}
