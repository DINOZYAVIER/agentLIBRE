use crate::{parse_tool_json, MalformedToolJsonKind, RepairStrategy, ToolJsonRepair};

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
