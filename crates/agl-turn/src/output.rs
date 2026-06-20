#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnOutput {
    Answered { answer: String },
    Stopped { reason: StopReason },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
