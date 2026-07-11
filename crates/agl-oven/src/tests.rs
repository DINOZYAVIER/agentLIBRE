use agl_config::{ModelConfig, ModelDialect, ToolCallFormat};
use agl_content::Content;
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

fn text(value: impl Into<String>) -> Content {
    Content::text(value).unwrap()
}

fn rendered_text(message: &RenderedMessage) -> String {
    message
        .content
        .as_ref()
        .and_then(Content::text_only)
        .expect("test message must contain text only")
}

fn qwen_hermes_config() -> ModelConfig {
    ModelConfig {
        dialect: ModelDialect::Qwen3,
        tool_call_format: ToolCallFormat::HermesJson,
    }
}

fn read_file_schema() -> serde_json::Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "options": {
                "type": "object",
                "properties": {
                    "line_limit": {"type": "integer", "minimum": 1}
                },
                "additionalProperties": false
            }
        },
        "required": ["path"],
        "additionalProperties": false
    })
}

fn read_file_tool() -> VisibleTool {
    VisibleTool {
        id: "read_file".parse().unwrap(),
        description: "Read a file".to_string(),
        input_schema: read_file_schema(),
    }
}

#[test]
fn renders_user_messages_and_visible_tools() {
    let request = ModelRequest {
        run_id: run_id(),
        turn_id: turn_id(),
        request_index: 0,
        messages: vec![TurnMessage::User {
            content: text("read README"),
        }],
        visible_tools: vec![read_file_tool()],
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
            content: Some(text("read README")),
            name: None,
            tool_calls: Vec::new(),
        }]
    );
    assert_eq!(
        rendered.tools,
        [RenderedTool {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            input_schema: read_file_schema(),
        }]
    );
}

#[test]
fn hermes_and_gemma_render_equivalent_full_tool_schemas() {
    let request = ModelRequest {
        run_id: run_id(),
        turn_id: turn_id(),
        request_index: 0,
        messages: vec![TurnMessage::User {
            content: text("read README"),
        }],
        visible_tools: vec![read_file_tool()],
    };
    let gemma_config = ModelConfig {
        dialect: ModelDialect::Gemma4,
        tool_call_format: ToolCallFormat::GemmaFunctionCall,
    };

    let hermes = render_model_request(&request, &qwen_hermes_config()).unwrap();
    let gemma = render_model_request(&request, &gemma_config).unwrap();

    assert_eq!(hermes.tools, gemma.tools);
    assert_eq!(hermes.tools[0].input_schema, read_file_schema());
    assert_eq!(
        hermes.tools[0].input_schema["properties"]["options"]["properties"]["line_limit"]["minimum"],
        1
    );
}

#[test]
fn renders_system_message() {
    let request = ModelRequest {
        run_id: run_id(),
        turn_id: turn_id(),
        request_index: 0,
        messages: vec![TurnMessage::System {
            content: text("demo system"),
        }],
        visible_tools: Vec::new(),
    };

    let rendered = render_model_request(&request, &qwen_hermes_config()).unwrap();

    assert_eq!(rendered.messages.len(), 1);
    assert_eq!(rendered.messages[0].role, RenderedMessageRole::System);
    assert_eq!(rendered_text(&rendered.messages[0]), "demo system");
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
    let content = rendered_text(&rendered.messages[0]);
    assert!(content.starts_with("<tool_call>"));
    assert!(content.ends_with("</tool_call>"));

    let raw_json = content
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
            result: agl_capabilities::ActionResult::new(json!({"text": "agentLIBRE readme"})),
        }],
        visible_tools: Vec::new(),
    };

    let rendered = render_model_request(&request, &qwen_hermes_config()).unwrap();

    assert_eq!(
        rendered.messages,
        [RenderedMessage {
            role: RenderedMessageRole::Tool,
            content: Some(text(r#"{"text":"agentLIBRE readme"}"#)),
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
            content: None,
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
            content: Some(text(
                r#"<|tool_call>call:get_current_temperature{location:<|"|>London<|"|>}<tool_call|>"#
                    .to_string(),
            )),
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
        rendered_text(&rendered.messages[0]),
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
