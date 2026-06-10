#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnOutput {
    pub answer: Option<String>,
    pub stop_reason: Option<StopReason>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    ToolJsonUnrepairable,
    ToolLimitReached,
    HiddenTool,
    InvalidToolArguments,
}
