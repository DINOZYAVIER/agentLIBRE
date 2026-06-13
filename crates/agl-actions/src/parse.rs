use crate::{
    MalformedToolCall, MalformedToolJsonKind, ModelAction, ToolCall, repair::repair_tool_json,
};
use serde_json::Value;

const TOOL_CALL_OPEN: &str = "<tool_call>";
const TOOL_CALL_CLOSE: &str = "</tool_call>";

pub fn parse_model_action(content: &str) -> ModelAction {
    let Some(open_at) = content.find(TOOL_CALL_OPEN) else {
        return ModelAction::Answer(content.to_string());
    };

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
