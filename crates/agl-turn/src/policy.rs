use agl_actions::ToolCall;
use serde_json::Value;

use crate::{StopDetail, StopReason, ToolDispatchRequest, TurnState, VisibleTool};

#[derive(Clone, Debug, PartialEq)]
pub enum ToolCallDecision {
    Dispatch(ToolDispatchRequest),
    Stop(ToolCallStop),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolCallStop {
    ToolLimitReached { limit: usize },
    HiddenTool { name: String },
    InvalidArguments { name: String, message: String },
}

impl ToolCallStop {
    pub fn reason(&self) -> StopReason {
        match self {
            ToolCallStop::ToolLimitReached { .. } => StopReason::ToolLimitReached,
            ToolCallStop::HiddenTool { .. } => StopReason::HiddenTool,
            ToolCallStop::InvalidArguments { .. } => StopReason::InvalidToolArguments,
        }
    }

    pub fn detail(&self) -> StopDetail {
        match self {
            ToolCallStop::ToolLimitReached { limit } => {
                StopDetail::ToolLimitReached { limit: *limit }
            }
            ToolCallStop::HiddenTool { name } => StopDetail::HiddenTool { name: name.clone() },
            ToolCallStop::InvalidArguments { name, message } => StopDetail::InvalidToolArguments {
                name: name.clone(),
                message: message.clone(),
            },
        }
    }
}

pub fn decide_tool_call(state: &TurnState, tool_call: &ToolCall) -> ToolCallDecision {
    if state.tool_call_count >= state.input.max_tool_calls {
        return ToolCallDecision::Stop(ToolCallStop::ToolLimitReached {
            limit: state.input.max_tool_calls,
        });
    }

    let Some(visible_tool) = state
        .input
        .visible_tools
        .iter()
        .find(|tool| tool.name == tool_call.name)
    else {
        return ToolCallDecision::Stop(ToolCallStop::HiddenTool {
            name: tool_call.name.clone(),
        });
    };

    if let Err(message) = validate_tool_arguments(visible_tool, &tool_call.arguments) {
        return ToolCallDecision::Stop(ToolCallStop::InvalidArguments {
            name: tool_call.name.clone(),
            message,
        });
    }

    ToolCallDecision::Dispatch(ToolDispatchRequest {
        turn_id: state.input.turn_id.clone(),
        name: tool_call.name.clone(),
        arguments: tool_call.arguments.clone(),
    })
}

fn validate_tool_arguments(
    tool: &VisibleTool,
    arguments: &Value,
) -> std::result::Result<(), String> {
    let Some(object) = arguments.as_object() else {
        return Err("tool arguments must be an object".to_string());
    };

    for required in &tool.required_arguments {
        if !object.contains_key(required) {
            return Err(format!("missing required argument `{required}`"));
        }
    }

    Ok(())
}
