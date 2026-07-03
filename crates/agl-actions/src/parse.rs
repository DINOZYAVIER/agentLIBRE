use crate::{
    MalformedToolCall, MalformedToolJsonKind, ModelAction, ToolCall, repair::repair_tool_json,
};
use serde_json::{Map, Value};

const TOOL_CALL_OPEN: &str = "<tool_call>";
const TOOL_CALL_CLOSE: &str = "</tool_call>";
const GEMMA_TOOL_CALL_OPEN: &str = "<|tool_call>";
const GEMMA_TOOL_CALL_CLOSE: &str = "<tool_call|>";
const GEMMA_CALL_PREFIX: &str = "call:";
const GEMMA_STRING_DELIMITER: &str = "<|\"|>";

pub fn parse_model_action(content: &str) -> ModelAction {
    match first_tool_call_format(content) {
        Some((ToolCallParser::Hermes, open_at)) => parse_hermes_model_action(content, open_at),
        Some((ToolCallParser::Gemma, open_at)) => parse_gemma_model_action(content, open_at),
        None => ModelAction::Answer(content.to_string()),
    }
}

#[derive(Clone, Copy)]
enum ToolCallParser {
    Hermes,
    Gemma,
}

fn first_tool_call_format(content: &str) -> Option<(ToolCallParser, usize)> {
    match (
        content.find(TOOL_CALL_OPEN),
        content.find(GEMMA_TOOL_CALL_OPEN),
    ) {
        (Some(hermes), Some(gemma)) if hermes <= gemma => Some((ToolCallParser::Hermes, hermes)),
        (Some(_), Some(gemma)) => Some((ToolCallParser::Gemma, gemma)),
        (Some(hermes), None) => Some((ToolCallParser::Hermes, hermes)),
        (None, Some(gemma)) => Some((ToolCallParser::Gemma, gemma)),
        (None, None) => None,
    }
}

fn parse_hermes_model_action(content: &str, open_at: usize) -> ModelAction {
    let json_start = open_at + TOOL_CALL_OPEN.len();
    let Some(close_rel) = content[json_start..].find(TOOL_CALL_CLOSE) else {
        let raw_json = content[json_start..].trim().to_string();
        let repair = repair_tool_json(&raw_json, MalformedToolJsonKind::MissingTerminator);
        return ModelAction::MalformedToolCall(MalformedToolCall {
            raw_json,
            classification: MalformedToolJsonKind::MissingTerminator,
            repair: Some(repair),
        });
    };

    let raw_json = content[json_start..json_start + close_rel]
        .trim()
        .to_string();
    parse_tool_json(&raw_json).map_or_else(
        |classification| {
            let repair = repair_tool_json(&raw_json, classification.clone());
            ModelAction::MalformedToolCall(MalformedToolCall {
                raw_json,
                classification,
                repair: Some(repair),
            })
        },
        ModelAction::ToolCall,
    )
}

fn parse_gemma_model_action(content: &str, open_at: usize) -> ModelAction {
    let call_start = open_at + GEMMA_TOOL_CALL_OPEN.len();
    let Some(close_rel) = content[call_start..].find(GEMMA_TOOL_CALL_CLOSE) else {
        return ModelAction::MalformedToolCall(MalformedToolCall {
            raw_json: content[call_start..].trim().to_string(),
            classification: MalformedToolJsonKind::MissingTerminator,
            repair: None,
        });
    };

    let raw_call = content[call_start..call_start + close_rel]
        .trim()
        .to_string();
    parse_gemma_tool_call(&raw_call).map_or_else(
        |classification| {
            ModelAction::MalformedToolCall(MalformedToolCall {
                raw_json: raw_call,
                classification,
                repair: None,
            })
        },
        ModelAction::ToolCall,
    )
}

pub(crate) fn parse_tool_json(raw_json: &str) -> Result<ToolCall, MalformedToolJsonKind> {
    let value: Value = serde_json::from_str(raw_json).map_err(|_| MalformedToolJsonKind::Syntax)?;
    tool_call_from_value(value)
}

fn tool_call_from_value(value: Value) -> Result<ToolCall, MalformedToolJsonKind> {
    let Value::Object(mut object) = value else {
        return Err(MalformedToolJsonKind::InvalidShape);
    };

    let Some(Value::String(name)) = object.remove("name") else {
        return Err(MalformedToolJsonKind::InvalidShape);
    };

    if name.trim().is_empty() {
        return Err(MalformedToolJsonKind::InvalidShape);
    }

    let Some(arguments) = object.remove("arguments") else {
        return Err(MalformedToolJsonKind::InvalidShape);
    };

    if !arguments.is_object() {
        return Err(MalformedToolJsonKind::InvalidShape);
    }

    Ok(ToolCall { name, arguments })
}

fn parse_gemma_tool_call(raw_call: &str) -> Result<ToolCall, MalformedToolJsonKind> {
    let Some(call) = raw_call.strip_prefix(GEMMA_CALL_PREFIX) else {
        return Err(MalformedToolJsonKind::InvalidShape);
    };
    let Some(arguments_start) = call.find('{') else {
        return Err(MalformedToolJsonKind::InvalidShape);
    };
    let name = call[..arguments_start].trim();
    if !is_gemma_tool_name(name) {
        return Err(MalformedToolJsonKind::InvalidShape);
    }

    let arguments = parse_gemma_arguments(&call[arguments_start..])?;
    Ok(ToolCall {
        name: name.to_string(),
        arguments,
    })
}

fn parse_gemma_arguments(raw_arguments: &str) -> Result<Value, MalformedToolJsonKind> {
    let raw_arguments = raw_arguments.trim();
    if !raw_arguments.starts_with('{') || !raw_arguments.ends_with('}') {
        return Err(MalformedToolJsonKind::InvalidShape);
    }

    let mut arguments = Map::new();
    let inner = raw_arguments[1..raw_arguments.len() - 1].trim();
    if inner.is_empty() {
        return Ok(Value::Object(arguments));
    }

    let mut offset = 0;
    while offset < inner.len() {
        let key_end = inner[offset..]
            .find(':')
            .map(|index| offset + index)
            .ok_or(MalformedToolJsonKind::InvalidShape)?;
        let key = inner[offset..key_end].trim();
        if !is_gemma_argument_name(key) {
            return Err(MalformedToolJsonKind::InvalidShape);
        }

        offset = skip_ascii_whitespace(inner, key_end + 1);
        let (value, next_offset) = parse_gemma_value(inner, offset)?;
        arguments.insert(key.to_string(), value);

        offset = skip_ascii_whitespace(inner, next_offset);
        if offset == inner.len() {
            break;
        }
        if inner[offset..].starts_with(',') {
            offset = skip_ascii_whitespace(inner, offset + 1);
            if offset == inner.len() {
                return Err(MalformedToolJsonKind::InvalidShape);
            }
        } else {
            return Err(MalformedToolJsonKind::InvalidShape);
        }
    }

    Ok(Value::Object(arguments))
}

fn parse_gemma_value(input: &str, offset: usize) -> Result<(Value, usize), MalformedToolJsonKind> {
    if offset >= input.len() {
        return Err(MalformedToolJsonKind::InvalidShape);
    }
    if input[offset..].starts_with(GEMMA_STRING_DELIMITER) {
        let value_start = offset + GEMMA_STRING_DELIMITER.len();
        let value_end_rel = input[value_start..]
            .find(GEMMA_STRING_DELIMITER)
            .ok_or(MalformedToolJsonKind::InvalidShape)?;
        let value_end = value_start + value_end_rel;
        return Ok((
            Value::String(input[value_start..value_end].to_string()),
            value_end + GEMMA_STRING_DELIMITER.len(),
        ));
    }

    let value_end = input[offset..]
        .find(',')
        .map(|index| offset + index)
        .unwrap_or(input.len());
    let raw_value = input[offset..value_end].trim();
    if raw_value.is_empty() {
        return Err(MalformedToolJsonKind::InvalidShape);
    }
    let value = match raw_value {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "null" => Value::Null,
        _ => serde_json::from_str(raw_value).map_err(|_| MalformedToolJsonKind::Syntax)?,
    };
    match value {
        Value::Number(_) | Value::Bool(_) | Value::Null => Ok((value, value_end)),
        _ => Err(MalformedToolJsonKind::InvalidShape),
    }
}

fn skip_ascii_whitespace(input: &str, mut offset: usize) -> usize {
    while offset < input.len() && input.as_bytes()[offset].is_ascii_whitespace() {
        offset += 1;
    }
    offset
}

fn is_gemma_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.' | b':')
        })
        && name.matches(':').count() <= 1
        && !name.starts_with(':')
        && !name.ends_with(':')
}

fn is_gemma_argument_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}
