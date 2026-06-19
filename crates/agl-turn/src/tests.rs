use agl_actions::ToolCall;
use serde_json::json;

use crate::policy::{ToolCallDecision, ToolCallStop, decide_tool_call};
use crate::*;

fn read_file_tool() -> VisibleTool {
    VisibleTool::new("read_file").require_argument("path")
}

fn tool_call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.to_string(),
        arguments,
    }
}

#[test]
fn initializes_turn_state_with_user_message() {
    let state = TurnState::new(TurnInput::user("hello"));

    assert_eq!(state.request_index, 0);
    assert_eq!(state.tool_call_count, 0);
    assert_eq!(
        state.messages,
        [TurnMessage::User {
            content: "hello".to_string(),
        }]
    );
}

#[test]
fn initializes_turn_state_with_context_and_request_index() {
    let state = TurnState::new(
        TurnInput::user("new")
            .with_turn_id("turn-chat")
            .with_context_messages(vec![
                TurnMessage::User {
                    content: "old".to_string(),
                },
                TurnMessage::Assistant {
                    content: "previous".to_string(),
                },
            ])
            .with_request_index_start(7),
    );

    assert_eq!(state.input.turn_id, "turn-chat");
    assert_eq!(state.request_index, 7);
    assert_eq!(
        state.messages,
        [
            TurnMessage::User {
                content: "old".to_string(),
            },
            TurnMessage::Assistant {
                content: "previous".to_string(),
            },
            TurnMessage::User {
                content: "new".to_string(),
            },
        ]
    );
}

#[test]
fn policy_dispatches_visible_tool_with_required_arguments() {
    let state = TurnState::new(
        TurnInput::user("read README")
            .with_visible_tool(read_file_tool())
            .with_max_tool_calls(1),
    );

    let decision = decide_tool_call(
        &state,
        &tool_call("read_file", json!({"path": "README.MD"})),
    );

    assert_eq!(
        decision,
        ToolCallDecision::Dispatch(ToolDispatchRequest {
            turn_id: "turn-1".to_string(),
            name: "read_file".to_string(),
            arguments: json!({"path": "README.MD"}),
        })
    );
}

#[test]
fn policy_stops_at_tool_limit_before_visibility_check() {
    let state = TurnState::new(TurnInput::user("read README").with_max_tool_calls(0));

    let decision = decide_tool_call(&state, &tool_call("hidden_tool", json!({})));

    assert_eq!(
        decision,
        ToolCallDecision::Stop(ToolCallStop::ToolLimitReached { limit: 0 })
    );
}

#[test]
fn policy_stops_hidden_tool_before_dispatch() {
    let state = TurnState::new(
        TurnInput::user("write README")
            .with_visible_tool(read_file_tool())
            .with_max_tool_calls(1),
    );

    let decision = decide_tool_call(
        &state,
        &tool_call("write_file", json!({"path": "README.MD"})),
    );

    assert_eq!(
        decision,
        ToolCallDecision::Stop(ToolCallStop::HiddenTool {
            name: "write_file".to_string(),
        })
    );
}

#[test]
fn policy_stops_invalid_tool_arguments_before_dispatch() {
    let state = TurnState::new(
        TurnInput::user("read README")
            .with_visible_tool(read_file_tool())
            .with_max_tool_calls(1),
    );

    let decision = decide_tool_call(
        &state,
        &tool_call("read_file", json!({"other": "README.MD"})),
    );

    assert_eq!(
        decision,
        ToolCallDecision::Stop(ToolCallStop::InvalidArguments {
            name: "read_file".to_string(),
            message: "missing required argument `path`".to_string(),
        })
    );
}

#[test]
fn append_tool_observation_records_assistant_tool_pair() {
    let mut state = TurnState::new(TurnInput::user("read README"));

    state.append_tool_observation(
        tool_call("read_file", json!({"path": "README.MD"})),
        "agentLIBRE readme".to_string(),
    );

    assert_eq!(state.tool_call_count, 1);
    assert_eq!(state.messages.len(), 3);
    assert_eq!(
        state.messages[1],
        TurnMessage::AssistantToolCall {
            name: "read_file".to_string(),
            arguments: json!({"path": "README.MD"}),
        }
    );
    assert_eq!(
        state.messages[2],
        TurnMessage::ToolObservation {
            name: "read_file".to_string(),
            content: "agentLIBRE readme".to_string(),
        }
    );
}
