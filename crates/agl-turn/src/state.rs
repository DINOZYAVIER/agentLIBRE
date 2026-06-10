use agl_actions::ToolCall;

use crate::{MessageRole, ModelMessage, TurnInput};

#[derive(Clone, Debug, PartialEq)]
pub struct TurnState {
    pub input: TurnInput,
    pub messages: Vec<ModelMessage>,
    pub request_index: usize,
    pub tool_call_count: usize,
}

impl TurnState {
    pub fn new(input: TurnInput) -> Self {
        let messages = vec![ModelMessage {
            role: MessageRole::User,
            content: input.user_input.clone(),
        }];
        Self {
            input,
            messages,
            request_index: 0,
            tool_call_count: 0,
        }
    }

    pub fn append_tool_observation(&mut self, tool_call: ToolCall, observation: String) {
        self.tool_call_count += 1;
        self.messages.push(ModelMessage {
            role: MessageRole::Assistant,
            content: format!(
                "<tool_call>{}</tool_call>",
                serde_json::json!({
                    "name": tool_call.name,
                    "arguments": tool_call.arguments,
                })
            ),
        });
        self.messages.push(ModelMessage {
            role: MessageRole::Tool,
            content: observation,
        });
    }
}
