use agl_capabilities::{ActionDeclaration, ActionSchema, CapabilityId, SchemaValidationError};
use agl_ids::{RunId, TurnId};
use serde::{Deserialize, Serialize};

use crate::{TurnHookBatch, transcript::TurnMessage};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_policy_hash: Option<String>,
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
            capability_policy_hash: None,
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

    pub fn with_capability_policy_hash(mut self, policy_hash: impl Into<String>) -> Self {
        self.capability_policy_hash = Some(policy_hash.into());
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisibleTool {
    pub id: CapabilityId,
    pub description: String,
    pub input_schema: serde_json::Value,
}

impl VisibleTool {
    pub fn from_declaration(declaration: &ActionDeclaration) -> Self {
        Self {
            id: declaration.id.clone(),
            description: declaration.description.clone(),
            input_schema: declaration.input_schema.clone(),
        }
    }

    pub fn compile_schema(&self) -> Result<ActionSchema, SchemaValidationError> {
        ActionSchema::compile(&self.input_schema)
    }
}
