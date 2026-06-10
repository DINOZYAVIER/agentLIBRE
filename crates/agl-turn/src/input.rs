#[derive(Clone, Debug, PartialEq)]
pub struct TurnInput {
    pub turn_id: String,
    pub user_input: String,
    pub visible_tools: Vec<VisibleTool>,
    pub max_tool_calls: usize,
}

impl TurnInput {
    pub fn user(user_input: impl Into<String>) -> Self {
        Self {
            turn_id: "turn-1".to_string(),
            user_input: user_input.into(),
            visible_tools: Vec::new(),
            max_tool_calls: 0,
        }
    }

    pub fn with_visible_tool(mut self, tool: VisibleTool) -> Self {
        self.visible_tools.push(tool);
        self
    }

    pub fn with_max_tool_calls(mut self, max_tool_calls: usize) -> Self {
        self.max_tool_calls = max_tool_calls;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleTool {
    pub name: String,
    pub required_arguments: Vec<String>,
}

impl VisibleTool {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            required_arguments: Vec::new(),
        }
    }

    pub fn require_argument(mut self, name: impl Into<String>) -> Self {
        self.required_arguments.push(name.into());
        self
    }
}
