use crate::ToolResult;
use agl_actions::ToolCall;
use serde::{Deserialize, Serialize};

use crate::{
    TurnInput, TurnMessage, TurnTransition, TurnTransitionError, TurnTransitionRecord,
    TurnTransitionState,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnState {
    pub input: TurnInput,
    pub messages: Vec<TurnMessage>,
    pub request_index: usize,
    pub tool_call_count: usize,
    pub transition_state: TurnTransitionState,
    pub(crate) continuation: crate::turn_executor::TurnContinuation,
}

impl TurnState {
    pub fn new(input: TurnInput) -> Self {
        let mut messages = input.context_messages.clone();
        messages.push(TurnMessage::User {
            content: input.user_input.clone(),
        });
        let request_index = input.request_index_start;
        Self {
            transition_state: TurnTransitionState::new(input.run_id.clone(), input.turn_id.clone()),
            continuation: crate::turn_executor::TurnContinuation::Initialize,
            input,
            messages,
            request_index,
            tool_call_count: 0,
        }
    }

    pub fn apply_transition(
        &mut self,
        transition: TurnTransition,
    ) -> Result<TurnTransitionRecord, TurnTransitionError> {
        self.transition_state.apply(transition)
    }

    pub fn append_tool_result(&mut self, tool_call: ToolCall, result: ToolResult) {
        self.tool_call_count += 1;
        self.messages.push(TurnMessage::AssistantToolCall {
            name: tool_call.name.clone(),
            arguments: tool_call.arguments,
        });
        self.messages.push(TurnMessage::ToolObservation {
            name: tool_call.name,
            result,
        });
    }
}
