use std::fmt;

use agl_capabilities::{HookBatchRequest, HookBatchResult};
use agl_events::{EventDraft, RuntimeEvent};
use agl_ids::TurnId;
use agl_turn::{
    ModelRequest, ModelResponse, ToolDispatchRequest, ToolDispatchResponse, TurnMessage, TurnOutput,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectKey {
    pub turn_id: TurnId,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEffectKind {
    HookBatch,
    ModelGeneration,
    CapabilityDispatch,
    TranscriptAppend,
}

impl TurnEffectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::HookBatch => "hook_batch",
            Self::ModelGeneration => "model_generation",
            Self::CapabilityDispatch => "capability_dispatch",
            Self::TranscriptAppend => "transcript_append",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnEffect {
    HookBatch {
        key: EffectKey,
        request: HookBatchRequest,
    },
    ModelGeneration {
        key: EffectKey,
        request: ModelRequest,
    },
    CapabilityDispatch {
        key: EffectKey,
        request: ToolDispatchRequest,
    },
    TranscriptAppend {
        key: EffectKey,
        messages: Vec<TurnMessage>,
        output: TurnOutput,
    },
}

impl TurnEffect {
    pub fn key(&self) -> &EffectKey {
        match self {
            Self::HookBatch { key, .. }
            | Self::ModelGeneration { key, .. }
            | Self::CapabilityDispatch { key, .. }
            | Self::TranscriptAppend { key, .. } => key,
        }
    }

    pub fn kind(&self) -> TurnEffectKind {
        match self {
            Self::HookBatch { .. } => TurnEffectKind::HookBatch,
            Self::ModelGeneration { .. } => TurnEffectKind::ModelGeneration,
            Self::CapabilityDispatch { .. } => TurnEffectKind::CapabilityDispatch,
            Self::TranscriptAppend { .. } => TurnEffectKind::TranscriptAppend,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectFailureCode {
    Hook,
    Inference,
    Capability,
    Transcript,
    Deadline,
    Invariant,
}

impl EffectFailureCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hook => "effect.hook_failed",
            Self::Inference => "effect.inference_failed",
            Self::Capability => "effect.capability_failed",
            Self::Transcript => "effect.transcript_failed",
            Self::Deadline => "effect.deadline_exceeded",
            Self::Invariant => "effect.invariant_failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectFailure {
    pub code: EffectFailureCode,
    pub message: String,
    pub retryable: bool,
}

impl EffectFailure {
    pub fn new(code: EffectFailureCode, message: impl Into<String>, retryable: bool) -> Self {
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
pub enum EffectOutcome<T> {
    Succeeded(T),
    Failed(EffectFailure),
    Cancelled,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookEffectOutput {
    pub result: HookBatchResult,
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnEffectResult {
    HookBatch {
        key: EffectKey,
        outcome: EffectOutcome<HookEffectOutput>,
    },
    ModelGeneration {
        key: EffectKey,
        outcome: EffectOutcome<ModelResponse>,
    },
    CapabilityDispatch {
        key: EffectKey,
        outcome: EffectOutcome<ToolDispatchResponse>,
    },
    TranscriptAppend {
        key: EffectKey,
        outcome: EffectOutcome<()>,
    },
}

impl TurnEffectResult {
    pub fn key(&self) -> &EffectKey {
        match self {
            Self::HookBatch { key, .. }
            | Self::ModelGeneration { key, .. }
            | Self::CapabilityDispatch { key, .. }
            | Self::TranscriptAppend { key, .. } => key,
        }
    }

    pub fn kind(&self) -> TurnEffectKind {
        match self {
            Self::HookBatch { .. } => TurnEffectKind::HookBatch,
            Self::ModelGeneration { .. } => TurnEffectKind::ModelGeneration,
            Self::CapabilityDispatch { .. } => TurnEffectKind::CapabilityDispatch,
            Self::TranscriptAppend { .. } => TurnEffectKind::TranscriptAppend,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnExecutionFailure {
    pub code: EffectFailureCode,
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
    Pending { effect: TurnEffect },
    Terminal { terminal: TurnTerminal },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnAdvance {
    pub events: Vec<EventDraft<RuntimeEvent>>,
    pub state: TurnAdvanceState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnExecutorError {
    InvalidCheckpoint(String),
    NoPendingEffect,
    DuplicateEffectKey(EffectKey),
    StaleEffectKey {
        expected: EffectKey,
        actual: EffectKey,
    },
    MismatchedEffectResult {
        expected: TurnEffectKind,
        actual: TurnEffectKind,
    },
    AlreadyTerminal,
    Transition(String),
}

impl fmt::Display for TurnExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCheckpoint(message) => {
                write!(formatter, "invalid turn checkpoint: {message}")
            }
            Self::NoPendingEffect => formatter.write_str("turn executor has no pending effect"),
            Self::DuplicateEffectKey(key) => write!(
                formatter,
                "effect {} for turn {} was already consumed",
                key.sequence, key.turn_id
            ),
            Self::StaleEffectKey { expected, actual } => write!(
                formatter,
                "effect key mismatch: expected {}:{}, got {}:{}",
                expected.turn_id, expected.sequence, actual.turn_id, actual.sequence
            ),
            Self::MismatchedEffectResult { expected, actual } => write!(
                formatter,
                "effect result kind mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::AlreadyTerminal => formatter.write_str("turn executor is already terminal"),
            Self::Transition(message) => write!(formatter, "turn transition failed: {message}"),
        }
    }
}

impl std::error::Error for TurnExecutorError {}
