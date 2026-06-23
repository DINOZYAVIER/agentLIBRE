use crate::{TurnHookBatch, transcript::TurnMessage};

#[derive(Clone, Debug, PartialEq)]
pub struct TurnInput {
    pub turn_id: String,
    pub user_input: String,
    pub context_messages: Vec<TurnMessage>,
    pub visible_tools: Vec<VisibleTool>,
    pub hook_batches: Vec<TurnHookBatch>,
    pub request_index_start: usize,
    pub max_tool_calls: usize,
}

impl TurnInput {
    pub fn user(user_input: impl Into<String>) -> Self {
        Self {
            turn_id: "turn-1".to_string(),
            user_input: user_input.into(),
            context_messages: Vec::new(),
            visible_tools: Vec::new(),
            hook_batches: Vec::new(),
            request_index_start: 0,
            max_tool_calls: 0,
        }
    }

    pub fn with_turn_id(mut self, turn_id: impl Into<String>) -> Self {
        self.turn_id = turn_id.into();
        self
    }

    pub fn with_context_messages(mut self, messages: Vec<TurnMessage>) -> Self {
        self.context_messages = messages;
        self
    }

    pub fn with_request_index_start(mut self, request_index_start: usize) -> Self {
        self.request_index_start = request_index_start;
        self
    }

    pub fn with_visible_tool(mut self, tool: VisibleTool) -> Self {
        self.visible_tools.push(tool);
        self
    }

    pub fn with_hook_batch(mut self, hook_batch: TurnHookBatch) -> Self {
        self.hook_batches.push(hook_batch);
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
