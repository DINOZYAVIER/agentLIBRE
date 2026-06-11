use agl_config::{ModelConfig, ModelDialect, ToolCallFormat};
use agl_turn::{ModelRequest, TurnMessage};
use anyhow::{bail, ensure, Result};

#[derive(Clone, Debug, PartialEq)]
pub struct RenderedModelRequest {
    pub turn_id: String,
    pub request_index: usize,
    pub dialect: ModelDialect,
    pub tool_call_format: ToolCallFormat,
    pub messages: Vec<RenderedMessage>,
    pub tools: Vec<RenderedTool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderedMessage {
    pub role: RenderedMessageRole,
    pub content: String,
    pub name: Option<String>,
    pub tool_calls: Vec<RenderedToolCall>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderedMessageRole {
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedTool {
    pub name: String,
    pub required_arguments: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderedToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
}

pub fn render_model_request(
    request: &ModelRequest,
    config: &ModelConfig,
) -> Result<RenderedModelRequest> {
    config.validate()?;

    let messages = request
        .messages
        .iter()
        .map(|message| render_message(message, config.tool_call_format))
        .collect::<Result<Vec<_>>>()?;
    let tools = request
        .visible_tools
        .iter()
        .map(|tool| RenderedTool {
            name: tool.name.clone(),
            required_arguments: tool.required_arguments.clone(),
        })
        .collect();

    Ok(RenderedModelRequest {
        turn_id: request.turn_id.clone(),
        request_index: request.request_index,
        dialect: config.dialect,
        tool_call_format: config.tool_call_format,
        messages,
        tools,
    })
}

fn render_message(
    message: &TurnMessage,
    tool_call_format: ToolCallFormat,
) -> Result<RenderedMessage> {
    match message {
        TurnMessage::User { content } => Ok(rendered_text_message(
            RenderedMessageRole::User,
            content.clone(),
            None,
        )),
        TurnMessage::Assistant { content } => Ok(rendered_text_message(
            RenderedMessageRole::Assistant,
            content.clone(),
            None,
        )),
        TurnMessage::AssistantToolCall { name, arguments } => {
            render_assistant_tool_call(name, arguments, tool_call_format)
        }
        TurnMessage::ToolObservation { name, content } => Ok(rendered_text_message(
            RenderedMessageRole::Tool,
            content.clone(),
            Some(name.clone()),
        )),
    }
}

fn rendered_text_message(
    role: RenderedMessageRole,
    content: String,
    name: Option<String>,
) -> RenderedMessage {
    RenderedMessage {
        role,
        content,
        name,
        tool_calls: Vec::new(),
    }
}

fn render_assistant_tool_call(
    name: &str,
    arguments: &serde_json::Value,
    tool_call_format: ToolCallFormat,
) -> Result<RenderedMessage> {
    if tool_call_format == ToolCallFormat::StructuredToolCalls {
        return Ok(RenderedMessage {
            role: RenderedMessageRole::Assistant,
            content: String::new(),
            name: None,
            tool_calls: vec![RenderedToolCall {
                name: name.to_string(),
                arguments: arguments.clone(),
            }],
        });
    }

    let content = match tool_call_format {
        ToolCallFormat::StructuredToolCalls => {
            unreachable!("structured tool calls return before text rendering")
        }
        ToolCallFormat::HermesJson => render_hermes_tool_call(name, arguments)?,
        ToolCallFormat::GemmaFunctionCall => render_gemma_function_call(name, arguments)?,
    };
    Ok(rendered_text_message(
        RenderedMessageRole::Assistant,
        content,
        Some(name.to_string()),
    ))
}

fn render_hermes_tool_call(name: &str, arguments: &serde_json::Value) -> Result<String> {
    let payload = serde_json::json!({
        "name": name,
        "arguments": arguments,
    });
    Ok(format!(
        "<tool_call>{}</tool_call>",
        serde_json::to_string(&payload)?
    ))
}

fn render_gemma_function_call(name: &str, arguments: &serde_json::Value) -> Result<String> {
    ensure_gemma_identifier(name, "tool name")?;
    let Some(arguments) = arguments.as_object() else {
        bail!("Gemma function calls require object arguments");
    };

    let mut fields = Vec::with_capacity(arguments.len());
    for (key, value) in arguments {
        ensure_gemma_identifier(key, "argument name")?;
        fields.push(format!("{key}:{}", render_gemma_value(value)?));
    }

    Ok(format!(
        "<|tool_call>call:{name}{{{}}}<tool_call|>",
        fields.join(",")
    ))
}

fn ensure_gemma_identifier(value: &str, kind: &str) -> Result<()> {
    ensure!(
        !value.is_empty(),
        "Gemma function-call {kind} cannot be empty"
    );
    ensure!(
        value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_'),
        "Gemma function-call {kind} contains unsupported characters"
    );
    Ok(())
}

fn render_gemma_value(value: &serde_json::Value) -> Result<String> {
    match value {
        serde_json::Value::String(value) => {
            ensure!(
                !value.contains("<|\"|>"),
                "Gemma function-call string argument contains the quote delimiter"
            );
            Ok(format!("<|\"|>{value}<|\"|>"))
        }
        serde_json::Value::Number(value) => Ok(value.to_string()),
        serde_json::Value::Bool(value) => Ok(value.to_string()),
        serde_json::Value::Null | serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            bail!("Gemma function calls support scalar argument values only")
        }
    }
}
