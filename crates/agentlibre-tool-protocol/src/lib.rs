use serde::{Deserialize, Serialize};
use serde_json::Value;

const TOOL_CALL_OPEN: &str = "<tool_call>";
const TOOL_CALL_CLOSE: &str = "</tool_call>";

#[derive(Clone, Debug, PartialEq)]
pub enum ModelAction {
    Answer(String),
    ToolCall(ToolCall),
    MalformedToolCall(MalformedToolCall),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MalformedToolCall {
    pub raw_json: String,
    pub classification: MalformedToolJsonKind,
    pub repair: Option<ToolJsonRepair>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MalformedToolJsonKind {
    MissingTerminator,
    Syntax,
    InvalidShape,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ToolJsonRepair {
    Succeeded {
        strategy: RepairStrategy,
        repaired_json: String,
        tool_call: ToolCall,
    },
    Failed {
        strategy: RepairStrategy,
        message: String,
    },
}

impl ToolJsonRepair {
    pub fn strategy(&self) -> RepairStrategy {
        match self {
            ToolJsonRepair::Succeeded { strategy, .. }
            | ToolJsonRepair::Failed { strategy, .. } => *strategy,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairStrategy {
    None,
    AppendMissingBrace,
    AcceptMissingTerminator,
    UnescapeQuotedJson,
}

impl RepairStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            RepairStrategy::None => "none",
            RepairStrategy::AppendMissingBrace => "append_missing_brace",
            RepairStrategy::AcceptMissingTerminator => "accept_missing_terminator",
            RepairStrategy::UnescapeQuotedJson => "unescape_quoted_json",
        }
    }
}

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

pub fn parse_tool_json(raw_json: &str) -> Result<ToolCall, MalformedToolJsonKind> {
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

pub fn repair_tool_json(raw_json: &str, classification: MalformedToolJsonKind) -> ToolJsonRepair {
    if let Some(repaired_json) = unescape_quoted_json(raw_json) {
        return match parse_tool_json(&repaired_json) {
            Ok(tool_call) => ToolJsonRepair::Succeeded {
                strategy: RepairStrategy::UnescapeQuotedJson,
                repaired_json,
                tool_call,
            },
            Err(kind) => ToolJsonRepair::Failed {
                strategy: RepairStrategy::UnescapeQuotedJson,
                message: format!("unescaped JSON remained invalid: {kind:?}"),
            },
        };
    }

    if classification == MalformedToolJsonKind::MissingTerminator {
        if let Ok(tool_call) = parse_tool_json(raw_json) {
            return ToolJsonRepair::Succeeded {
                strategy: RepairStrategy::AcceptMissingTerminator,
                repaired_json: raw_json.to_string(),
                tool_call,
            };
        }
    }

    if classification == MalformedToolJsonKind::InvalidShape {
        return ToolJsonRepair::Failed {
            strategy: RepairStrategy::None,
            message: "tool call JSON has an invalid shape".to_string(),
        };
    }

    if let Some(repaired_json) = append_one_missing_brace(raw_json) {
        return match parse_tool_json(&repaired_json) {
            Ok(tool_call) => ToolJsonRepair::Succeeded {
                strategy: RepairStrategy::AppendMissingBrace,
                repaired_json,
                tool_call,
            },
            Err(kind) => ToolJsonRepair::Failed {
                strategy: RepairStrategy::AppendMissingBrace,
                message: format!("brace repair remained invalid: {kind:?}"),
            },
        };
    }

    ToolJsonRepair::Failed {
        strategy: RepairStrategy::None,
        message: "no safe repair strategy matched".to_string(),
    }
}

fn unescape_quoted_json(raw_json: &str) -> Option<String> {
    let trimmed = raw_json.trim();
    if !(trimmed.starts_with('"') && trimmed.ends_with('"')) {
        return None;
    }

    let unescaped: String = serde_json::from_str(trimmed).ok()?;
    if unescaped.trim_start().starts_with('{') && unescaped.trim_end().ends_with('}') {
        Some(unescaped)
    } else {
        None
    }
}

fn append_one_missing_brace(raw_json: &str) -> Option<String> {
    let trimmed = raw_json.trim();
    if trimmed.is_empty() || !trimmed.starts_with('{') {
        return None;
    }

    let mut balance = 0isize;
    let mut in_string = false;
    let mut escaped = false;
    for ch in trimmed.chars() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => balance += 1,
            '}' => balance -= 1,
            _ => {}
        }
        if balance < 0 {
            return None;
        }
    }

    if !in_string && !escaped && balance == 1 {
        Some(format!("{trimmed}}}"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_plain_answer() {
        assert_eq!(
            parse_model_action("done"),
            ModelAction::Answer("done".to_string())
        );
    }

    #[test]
    fn parses_valid_qwen_hermes_tool_call() {
        assert_eq!(
            parse_model_action(
                r#"<tool_call>{"name":"read_file","arguments":{"path":"README.MD"}}</tool_call>"#
            ),
            ModelAction::ToolCall(ToolCall {
                name: "read_file".to_string(),
                arguments: json!({"path": "README.MD"}),
            })
        );
    }

    #[test]
    fn repairs_quoted_tool_json() {
        let action = parse_model_action(
            r#"<tool_call>"{\"name\":\"read_file\",\"arguments\":{\"path\":\"README.MD\"}}"</tool_call>"#,
        );

        let ModelAction::MalformedToolCall(malformed) = action else {
            panic!("expected malformed tool call with repair");
        };
        let Some(ToolJsonRepair::Succeeded {
            strategy,
            tool_call,
            ..
        }) = malformed.repair
        else {
            panic!("expected successful repair");
        };

        assert_eq!(strategy, RepairStrategy::UnescapeQuotedJson);
        assert_eq!(tool_call.name, "read_file");
        assert_eq!(tool_call.arguments, json!({"path": "README.MD"}));
    }

    #[test]
    fn repairs_one_missing_closing_brace() {
        let action = parse_model_action(
            r#"<tool_call>{"name":"read_file","arguments":{"path":"README.MD"}</tool_call>"#,
        );

        let ModelAction::MalformedToolCall(malformed) = action else {
            panic!("expected malformed tool call with repair");
        };
        let Some(ToolJsonRepair::Succeeded {
            strategy,
            tool_call,
            ..
        }) = malformed.repair
        else {
            panic!("expected successful repair");
        };

        assert_eq!(strategy, RepairStrategy::AppendMissingBrace);
        assert_eq!(tool_call.name, "read_file");
        assert_eq!(tool_call.arguments, json!({"path": "README.MD"}));
    }

    #[test]
    fn repairs_missing_tool_call_terminator_when_json_is_complete() {
        let action = parse_model_action(
            r#"<tool_call>{"name":"read_file","arguments":{"path":"README.MD"}}"#,
        );

        let ModelAction::MalformedToolCall(malformed) = action else {
            panic!("expected malformed tool call with repair");
        };
        let Some(ToolJsonRepair::Succeeded {
            strategy,
            tool_call,
            ..
        }) = malformed.repair
        else {
            panic!("expected successful repair");
        };

        assert_eq!(
            malformed.classification,
            MalformedToolJsonKind::MissingTerminator
        );
        assert_eq!(strategy, RepairStrategy::AcceptMissingTerminator);
        assert_eq!(tool_call.name, "read_file");
        assert_eq!(tool_call.arguments, json!({"path": "README.MD"}));
    }

    #[test]
    fn leaves_unrepairable_json_malformed() {
        let action = parse_model_action(r#"<tool_call>{"name":,"arguments":42</tool_call>"#);

        let ModelAction::MalformedToolCall(malformed) = action else {
            panic!("expected malformed tool call");
        };

        assert_eq!(malformed.classification, MalformedToolJsonKind::Syntax);
        assert!(matches!(
            malformed.repair,
            Some(ToolJsonRepair::Failed { .. })
        ));
    }
}
