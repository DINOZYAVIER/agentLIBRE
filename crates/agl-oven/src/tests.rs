use agl_config::{ModelConfig, ModelDialect, ToolCallFormat};
use agl_ids::{RunId, TurnId};
use agl_turn::{ModelRequest, TurnMessage, VisibleTool};
use serde_json::json;

use crate::{
    RenderedMessage, RenderedMessageRole, RenderedTool, RenderedToolCall, render_model_request,
};

const TEST_RUN_ID: &str = "run_01890f17-4a00-7000-8000-000000000001";
const TEST_TURN_ID: &str = "turn_01890f17-4a00-7000-8000-000000000002";

fn run_id() -> RunId {
    RunId::parse(TEST_RUN_ID).unwrap()
}

fn turn_id() -> TurnId {
    TurnId::parse(TEST_TURN_ID).unwrap()
}

fn qwen_hermes_config() -> ModelConfig {
    ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
    }
}

#[test]
fn renders_user_messages_and_visible_tools() {
    let request = ModelRequest {
        run_id: run_id(),
        turn_id: turn_id(),
        request_index: 0,
        messages: vec![TurnMessage::User {
            content: "read README".to_string(),
        }],
        visible_tools: vec![VisibleTool::new("read_file").require_argument("path")],
    };

    let rendered = render_model_request(&request, &qwen_hermes_config()).unwrap();

    assert_eq!(rendered.run_id, run_id());
    assert_eq!(rendered.turn_id, turn_id());
    assert_eq!(rendered.request_index, 0);
    assert_eq!(rendered.dialect, ModelDialect::Qwen3);
    assert_eq!(rendered.tool_call_format, ToolCallFormat::HermesJson);
    assert_eq!(
        rendered.messages,
        [RenderedMessage {
            role: RenderedMessageRole::User,
            content: "read README".to_string(),
            name: None,
            tool_calls: Vec::new(),
        }]
    );
    assert_eq!(
        rendered.tools,
        [RenderedTool {
            name: "read_file".to_string(),
            description: String::new(),
            required_arguments: vec!["path".to_string()],
        }]
    );
}

#[test]
fn renders_system_message() {
    let request = ModelRequest {
        run_id: run_id(),
        turn_id: turn_id(),
        request_index: 0,
        messages: vec![TurnMessage::System {
            content: "demo system".to_string(),
        }],
        visible_tools: Vec::new(),
    };

    let rendered = render_model_request(&request, &qwen_hermes_config()).unwrap();

    assert_eq!(rendered.messages.len(), 1);
    assert_eq!(rendered.messages[0].role, RenderedMessageRole::System);
    assert_eq!(rendered.messages[0].content, "demo system");
    assert_eq!(rendered.messages[0].name, None);
    assert!(rendered.messages[0].tool_calls.is_empty());
}

#[test]
fn renders_hermes_assistant_tool_call_transcript() {
    let request = ModelRequest {
        run_id: run_id(),
        turn_id: turn_id(),
        request_index: 1,
        messages: vec![TurnMessage::AssistantToolCall {
            name: "read_file".to_string(),
            arguments: json!({"path": "README.MD"}),
        }],
        visible_tools: Vec::new(),
    };

    let rendered = render_model_request(&request, &qwen_hermes_config()).unwrap();

    assert_eq!(rendered.messages.len(), 1);
    assert_eq!(rendered.messages[0].role, RenderedMessageRole::Assistant);
    assert_eq!(rendered.messages[0].name, Some("read_file".to_string()));
    assert!(rendered.messages[0].tool_calls.is_empty());
    assert!(rendered.messages[0].content.starts_with("<tool_call>"));
    assert!(rendered.messages[0].content.ends_with("</tool_call>"));

    let raw_json = rendered.messages[0]
        .content
        .strip_prefix("<tool_call>")
        .and_then(|value| value.strip_suffix("</tool_call>"))
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(raw_json).unwrap();
    assert_eq!(
        payload,
        json!({
            "name": "read_file",
            "arguments": {"path": "README.MD"},
        })
    );
}

#[test]
fn renders_tool_observation_with_tool_name() {
    let request = ModelRequest {
        run_id: run_id(),
        turn_id: turn_id(),
        request_index: 1,
        messages: vec![TurnMessage::ToolObservation {
            name: "read_file".to_string(),
            content: "agentLIBRE readme".to_string(),
        }],
        visible_tools: Vec::new(),
    };

    let rendered = render_model_request(&request, &qwen_hermes_config()).unwrap();

    assert_eq!(
        rendered.messages,
        [RenderedMessage {
            role: RenderedMessageRole::Tool,
            content: "agentLIBRE readme".to_string(),
            name: Some("read_file".to_string()),
            tool_calls: Vec::new(),
        }]
    );
}

#[test]
fn renders_structured_assistant_tool_call_without_text_wrapper() {
    let request = ModelRequest {
        run_id: run_id(),
        turn_id: turn_id(),
        request_index: 1,
        messages: vec![TurnMessage::AssistantToolCall {
            name: "read_file".to_string(),
            arguments: json!({"path": "README.MD"}),
        }],
        visible_tools: Vec::new(),
    };
    let config = ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::StructuredToolCalls,
    };

    let rendered = render_model_request(&request, &config).unwrap();

    assert_eq!(
        rendered.messages,
        [RenderedMessage {
            role: RenderedMessageRole::Assistant,
            content: String::new(),
            name: None,
            tool_calls: vec![RenderedToolCall {
                name: "read_file".to_string(),
                arguments: json!({"path": "README.MD"}),
            }],
        }]
    );
}

#[test]
fn renders_gemma_function_call_transcript() {
    let request = ModelRequest {
        run_id: run_id(),
        turn_id: turn_id(),
        request_index: 1,
        messages: vec![TurnMessage::AssistantToolCall {
            name: "get_current_temperature".to_string(),
            arguments: json!({"location": "London"}),
        }],
        visible_tools: Vec::new(),
    };
    let config = ModelConfig {
        dialect: ModelDialect::Gemma4,
        tool_call_format: ToolCallFormat::GemmaFunctionCall,
    };

    let rendered = render_model_request(&request, &config).unwrap();

    assert_eq!(
        rendered.messages,
        [RenderedMessage {
            role: RenderedMessageRole::Assistant,
            content:
                r#"<|tool_call>call:get_current_temperature{location:<|"|>London<|"|>}<tool_call|>"#
                    .to_string(),
            name: Some("get_current_temperature".to_string()),
            tool_calls: Vec::new(),
        }]
    );
}

#[test]
fn renders_gemma_function_call_transcript_with_dotted_tool_name() {
    let request = ModelRequest {
        run_id: run_id(),
        turn_id: turn_id(),
        request_index: 1,
        messages: vec![TurnMessage::AssistantToolCall {
            name: "fs.read".to_string(),
            arguments: json!({"path": "README.MD"}),
        }],
        visible_tools: Vec::new(),
    };
    let config = ModelConfig {
        dialect: ModelDialect::Gemma4,
        tool_call_format: ToolCallFormat::GemmaFunctionCall,
    };

    let rendered = render_model_request(&request, &config).unwrap();

    assert_eq!(
        rendered.messages[0].content,
        r#"<|tool_call>call:fs.read{path:<|"|>README.MD<|"|>}<tool_call|>"#
    );
}

#[test]
fn rejects_gemma_function_call_nested_arguments_explicitly() {
    let request = ModelRequest {
        run_id: run_id(),
        turn_id: turn_id(),
        request_index: 1,
        messages: vec![TurnMessage::AssistantToolCall {
            name: "read_file".to_string(),
            arguments: json!({"path": {"nested": "README.MD"}}),
        }],
        visible_tools: Vec::new(),
    };
    let config = ModelConfig {
        dialect: ModelDialect::Gemma4,
        tool_call_format: ToolCallFormat::GemmaFunctionCall,
    };

    let error = render_model_request(&request, &config).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("Gemma function calls support scalar argument values only")
    );
}
