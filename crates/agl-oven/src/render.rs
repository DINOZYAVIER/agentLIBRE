use agl_config::{ModelConfig, ModelDialect, ToolCallFormat};
use agl_ids::{RunId, TurnId};
use agl_turn::{ModelRequest, TurnMessage};
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RenderedModelRequest {
    pub run_id: RunId,
    pub turn_id: TurnId,
    pub request_index: usize,
    pub dialect: ModelDialect,
    pub tool_call_format: ToolCallFormat,
    pub messages: Vec<RenderedMessage>,
    pub tools: Vec<RenderedTool>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RenderedMessage {
    pub role: RenderedMessageRole,
    pub content: String,
    pub name: Option<String>,
    pub tool_calls: Vec<RenderedToolCall>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderedMessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderedTool {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
            name: tool.id.as_str().to_string(),
            description: tool.description.clone(),
            input_schema: tool.input_schema.clone(),
        })
        .collect();

    Ok(RenderedModelRequest {
        run_id: request.run_id.clone(),
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
        TurnMessage::System { content } => Ok(rendered_text_message(
            RenderedMessageRole::System,
            content.clone(),
            None,
        )),
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
        TurnMessage::ToolObservation { name, result } => Ok(rendered_text_message(
            RenderedMessageRole::Tool,
            result.render_observation(),
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
    match tool_call_format {
        ToolCallFormat::StructuredToolCalls => Ok(RenderedMessage {
            role: RenderedMessageRole::Assistant,
            content: String::new(),
            name: None,
            tool_calls: vec![RenderedToolCall {
                name: name.to_string(),
                arguments: arguments.clone(),
            }],
        }),
        ToolCallFormat::HermesJson => Ok(rendered_text_message(
            RenderedMessageRole::Assistant,
            render_hermes_tool_call(name, arguments)?,
            Some(name.to_string()),
        )),
        ToolCallFormat::GemmaFunctionCall => Ok(rendered_text_message(
            RenderedMessageRole::Assistant,
            render_gemma_function_call(name, arguments)?,
            Some(name.to_string()),
        )),
    }
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
    ensure_gemma_tool_name(name)?;
    let Some(arguments) = arguments.as_object() else {
        bail!("Gemma function calls require object arguments");
    };

    let mut fields = Vec::with_capacity(arguments.len());
    for (key, value) in arguments {
        ensure_gemma_argument_name(key)?;
        fields.push(format!("{key}:{}", render_gemma_value(value)?));
    }

    Ok(format!(
        "<|tool_call>call:{name}{{{}}}<tool_call|>",
        fields.join(",")
    ))
}

fn ensure_gemma_tool_name(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty(),
        "Gemma function-call tool name cannot be empty"
    );
    ensure!(
        value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.' | b':')
        }) && value.matches(':').count() <= 1
            && !value.starts_with(':')
            && !value.ends_with(':'),
        "Gemma function-call tool name contains unsupported characters"
    );
    Ok(())
}

fn ensure_gemma_argument_name(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty(),
        "Gemma function-call argument name cannot be empty"
    );
    ensure!(
        value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_'),
        "Gemma function-call argument name contains unsupported characters"
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
