use crate::{MessageRole, ModelMessage, TurnInput};

pub(crate) struct TurnState {
    pub(crate) input: TurnInput,
    pub(crate) messages: Vec<ModelMessage>,
    pub(crate) request_index: usize,
    pub(crate) tool_call_count: usize,
}

impl TurnState {
    pub(crate) fn new(input: TurnInput) -> Self {
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
}
