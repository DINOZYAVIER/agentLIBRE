use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::HookId;
use crate::{HookDeclaration, ToolSchema};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    ContextPrepare,
    ModelRequest,
    ModelResponse,
    ToolCallBefore,
    ToolCallAfter,
    ArtifactWrite,
    TurnFinish,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContextPrepare => "context.prepare",
            Self::ModelRequest => "model.request",
            Self::ModelResponse => "model.response",
            Self::ToolCallBefore => "tool.call.before",
            Self::ToolCallAfter => "tool.call.after",
            Self::ArtifactWrite => "artifact.write",
            Self::TurnFinish => "turn.finish",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookStatus {
    Pass,
    Warn,
    Fail,
    Repair,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookMessage {
    pub code: String,
    pub message: String,
    pub fix: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookInput {
    pub hook_id: HookId,
    pub event: HookEvent,
    pub payload: Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookResult {
    pub hook_id: HookId,
    pub status: HookStatus,
    pub messages: Vec<HookMessage>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookBatchRequest {
    pub event: HookEvent,
    pub hooks: Vec<HookId>,
    pub payload: Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookBatchResult {
    pub event: HookEvent,
    pub results: Vec<HookResult>,
}

pub trait HookHandler: Send + Sync {
    fn invoke(&self, input: HookInput) -> Result<Value, HookHandlerError>;
}

impl<F> HookHandler for F
where
    F: Fn(HookInput) -> Result<Value, HookHandlerError> + Send + Sync,
{
    fn invoke(&self, input: HookInput) -> Result<Value, HookHandlerError> {
        self(input)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HookHandlerError {
    message: String,
}

impl HookHandlerError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for HookHandlerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for HookHandlerError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HookInvocationError {
    BindingMismatch,
    InvalidInput(String),
    Handler(HookHandlerError),
    InvalidOutput(String),
}

impl std::fmt::Display for HookInvocationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BindingMismatch => formatter.write_str("hook binding does not match declaration"),
            Self::InvalidInput(message) => write!(formatter, "invalid hook input: {message}"),
            Self::Handler(error) => write!(formatter, "hook handler failed: {error}"),
            Self::InvalidOutput(message) => write!(formatter, "invalid hook output: {message}"),
        }
    }
}

impl std::error::Error for HookInvocationError {}

pub(crate) fn invoke_bound_hook(
    declaration: &HookDeclaration,
    handler: &dyn HookHandler,
    payload: Value,
) -> Result<Value, HookInvocationError> {
    ToolSchema::compile(&declaration.input_schema)
        .map_err(|error| HookInvocationError::InvalidInput(error.to_string()))?
        .validate(&payload)
        .map_err(|error| HookInvocationError::InvalidInput(error.to_string()))?;
    let output = handler
        .invoke(HookInput {
            hook_id: declaration.id.clone(),
            event: declaration.event,
            payload,
        })
        .map_err(HookInvocationError::Handler)?;
    ToolSchema::compile(&declaration.output_schema)
        .map_err(|error| HookInvocationError::InvalidOutput(error.to_string()))?
        .validate(&output)
        .map_err(|error| HookInvocationError::InvalidOutput(error.to_string()))?;
    Ok(output)
}

impl HookBatchResult {
    pub fn status(&self) -> HookStatus {
        if self
            .results
            .iter()
            .any(|result| result.status == HookStatus::Fail)
        {
            HookStatus::Fail
        } else if self
            .results
            .iter()
            .any(|result| result.status == HookStatus::Repair)
        {
            HookStatus::Repair
        } else if self
            .results
            .iter()
            .any(|result| result.status == HookStatus::Warn)
        {
            HookStatus::Warn
        } else {
            HookStatus::Pass
        }
    }
}
