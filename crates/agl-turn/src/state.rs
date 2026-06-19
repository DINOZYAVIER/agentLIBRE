use agl_actions::ToolCall;

use crate::{TurnInput, TurnMessage};

#[derive(Clone, Debug, PartialEq)]
pub struct TurnState {
    pub input: TurnInput,
    pub messages: Vec<TurnMessage>,
    pub request_index: usize,
    pub tool_call_count: usize,
}

impl TurnState {
    pub fn new(input: TurnInput) -> Self {
        let mut messages = input.context_messages.clone();
        messages.push(TurnMessage::User {
            content: input.user_input.clone(),
        });
        let request_index = input.request_index_start;
        Self {
            input,
            messages,
            request_index,
            tool_call_count: 0,
        }
    }

    pub fn append_tool_observation(&mut self, tool_call: ToolCall, observation: String) {
        self.tool_call_count += 1;
        self.messages.push(TurnMessage::AssistantToolCall {
            name: tool_call.name.clone(),
            arguments: tool_call.arguments,
        });
        self.messages.push(TurnMessage::ToolObservation {
            name: tool_call.name,
            content: observation,
        });
    }
}
