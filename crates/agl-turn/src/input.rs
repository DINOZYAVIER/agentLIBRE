use agl_ids::{RunId, TurnId};

use crate::{TurnHookBatch, transcript::TurnMessage};

#[derive(Clone, Debug, PartialEq)]
pub struct TurnInput {
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub user_input: String,
    pub context_messages: Vec<TurnMessage>,
    pub visible_tools: Vec<VisibleTool>,
    pub hook_batches: Vec<TurnHookBatch>,
    pub hook_payload: serde_json::Value,
    pub request_index_start: usize,
    pub max_tool_calls: usize,
    pub max_hook_repair_attempts: usize,
}

impl TurnInput {
    pub fn user(run_id: RunId, turn_id: TurnId, user_input: impl Into<String>) -> Self {
        Self {
            run_id,
            turn_id,
            user_input: user_input.into(),
            context_messages: Vec::new(),
            visible_tools: Vec::new(),
            hook_batches: Vec::new(),
            hook_payload: serde_json::Value::Object(serde_json::Map::new()),
            request_index_start: 0,
            max_tool_calls: 0,
            max_hook_repair_attempts: 0,
        }
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

    pub fn with_hook_payload(mut self, payload: serde_json::Value) -> Self {
        self.hook_payload = payload;
        self
    }

    pub fn with_max_tool_calls(mut self, max_tool_calls: usize) -> Self {
        self.max_tool_calls = max_tool_calls;
        self
    }

    pub fn with_max_hook_repair_attempts(mut self, max_hook_repair_attempts: usize) -> Self {
        self.max_hook_repair_attempts = max_hook_repair_attempts;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleTool {
    pub name: String,
    pub description: String,
    pub required_arguments: Vec<String>,
}

impl VisibleTool {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            required_arguments: Vec::new(),
        }
    }

    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn require_argument(mut self, name: impl Into<String>) -> Self {
        self.required_arguments.push(name.into());
        self
    }
}
