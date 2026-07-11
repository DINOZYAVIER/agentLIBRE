use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum TurnOutput {
    Answered {
        answer: String,
    },
    Stopped {
        reason: StopReason,
        detail: Option<StopDetail>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    ToolJsonUnrepairable,
    ToolLimitReached,
    HiddenTool,
    InvalidToolArguments,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ToolJsonUnrepairable => "tool_json_unrepairable",
            Self::ToolLimitReached => "tool_limit_reached",
            Self::HiddenTool => "hidden_tool",
            Self::InvalidToolArguments => "invalid_tool_arguments",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StopDetail {
    HiddenTool { name: String },
    ToolLimitReached { limit: usize },
    InvalidToolArguments { name: String, message: String },
}
