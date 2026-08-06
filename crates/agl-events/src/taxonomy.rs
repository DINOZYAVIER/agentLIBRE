use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParsedActionEvent {
    Answer,
    ToolCall { name: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeParsedActionEvent {
    Answer,
    ToolCall { tool_id: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolJsonMalformedKind {
    MissingTerminator,
    Syntax,
    InvalidShape,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReasonEvent {
    ToolJsonUnrepairable,
    ToolLimitReached,
    HiddenTool,
    InvalidToolArguments,
    RepairRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookBatchOutcomeEvent {
    Pass,
    Warn,
    Fail,
    Repair,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnFinishStatus {
    Answered,
    IncompleteOutput,
    Stopped,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncompleteOutputReasonEvent {
    ModelLength,
    ContentByteLimit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceFinishStatus {
    Succeeded,
    IncompleteOutput,
    Failed,
}
