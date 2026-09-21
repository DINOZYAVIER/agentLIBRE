use super::*;

pub(crate) fn reasoning_command(
    input: &str,
    selected: &mut Option<agl_core::agent::ReasoningEffort>,
) -> Result<bool, String> {
    let mut words = input.split_whitespace();
    if words.next() != Some("/reasoning") {
        return Ok(false);
    }
    let value = words.next();
    if words.next().is_some() {
        return Err("usage: /reasoning [default|low|medium|xhigh]".into());
    }
    match value {
        None => {}
        Some("default") => *selected = None,
        Some(value) => *selected = Some(parse_reasoning(value)?),
    }
    Ok(true)
}

pub(crate) fn decorations_command(
    input: &str,
    override_value: &mut Option<Decorations>,
) -> Result<bool, String> {
    let mut words = input.split_whitespace();
    if words.next() != Some("/decorations") {
        return Ok(false);
    }
    let value = words.next();
    if words.next().is_some() {
        return Err("usage: /decorations [off|default|full|reset]".into());
    }
    match value {
        None => {}
        Some("off") => *override_value = Some(Decorations::Off),
        Some("default") => *override_value = Some(Decorations::Default),
        Some("full") => *override_value = Some(Decorations::Full),
        Some("reset") => *override_value = None,
        Some(_) => return Err("usage: /decorations [off|default|full|reset]".into()),
    }
    Ok(true)
}

pub(crate) fn tool_output_command(
    input: &str,
    current: &mut agl_core::agent::ToolOutputPresentation,
    defaults: agl_core::agent::ToolOutputPresentation,
) -> Result<bool, String> {
    let mut words = input.split_whitespace();
    if words.next() != Some("/tool-output") {
        return Ok(false);
    }
    let field = words.next();
    let value = words.next();
    if words.next().is_some() {
        return Err("usage: /tool-output [lines N|chars N|reset]".into());
    }
    match (field, value) {
        (None, None) => {}
        (Some("reset"), None) => *current = defaults,
        (Some("lines"), Some(value)) => {
            current.lines = parse_tool_output_limit("lines", value)?;
        }
        (Some("chars"), Some(value)) => {
            current.chars = parse_tool_output_limit("chars", value)?;
        }
        _ => return Err("usage: /tool-output [lines N|chars N|reset]".into()),
    }
    Ok(true)
}

pub(crate) fn tool_frame_command(
    input: &str,
    current: &mut bool,
    default: bool,
) -> Result<bool, String> {
    let mut words = input.split_whitespace();
    if words.next() != Some("/tool-frame") {
        return Ok(false);
    }
    let value = words.next();
    if words.next().is_some() {
        return Err("usage: /tool-frame [on|off|reset]".into());
    }
    match value {
        None => {}
        Some("on") => *current = true,
        Some("off") => *current = false,
        Some("reset") => *current = default,
        Some(_) => return Err("usage: /tool-frame [on|off|reset]".into()),
    }
    Ok(true)
}

pub(crate) fn parse_tool_output_limit(field: &str, value: &str) -> Result<u32, String> {
    let value = value
        .parse::<u32>()
        .map_err(|_| format!("tool-output {field} must be a positive integer"))?;
    if value == 0 {
        return Err(format!("tool-output {field} must be a positive integer"));
    }
    Ok(value)
}

pub(crate) fn model_generation_command(
    input: &str,
    current: &mut bool,
    default: bool,
) -> Result<bool, String> {
    let mut words = input.split_whitespace();
    if words.next() != Some("/model-generation") {
        return Ok(false);
    }
    let field = words.next();
    let value = words.next();
    if words.next().is_some() {
        return Err("usage: /model-generation [details on|off|reset]".into());
    }
    match (field, value) {
        (None, None) => {}
        (Some("details"), Some("on")) => *current = true,
        (Some("details"), Some("off")) => *current = false,
        (Some("details"), Some("reset")) => *current = default,
        _ => return Err("usage: /model-generation [details on|off|reset]".into()),
    }
    Ok(true)
}

pub(crate) fn decorations_label(value: Decorations) -> &'static str {
    match value {
        Decorations::Off => "off",
        Decorations::Default => "default",
        Decorations::Full => "full",
    }
}

pub(crate) fn status_role(status: AgentRunStatus) -> &'static str {
    match status {
        AgentRunStatus::Completed => "status_success",
        AgentRunStatus::Failed | AgentRunStatus::Cancelled => "status_failure",
        AgentRunStatus::Pending | AgentRunStatus::Running => "status_pending",
    }
}

pub(crate) fn operation_status_role(status: &str) -> &'static str {
    match status {
        "Succeeded" => "status_success",
        "Failed" | "Cancelled" => "status_failure",
        _ => "status_pending",
    }
}

pub(crate) fn reasoning_label(
    base: &agl_core::agent::ReasoningSelection,
    selected: Option<agl_core::agent::ReasoningEffort>,
) -> &'static str {
    use agl_core::agent::{ReasoningEffort, ReasoningSelection};
    match selected.or(match base {
        ReasoningSelection::Enabled { effort, .. } => *effort,
        ReasoningSelection::Disabled => None,
    }) {
        Some(ReasoningEffort::Low) => "low",
        Some(ReasoningEffort::Medium) => "medium",
        Some(ReasoningEffort::Xhigh) => "xhigh",
        None if matches!(base, ReasoningSelection::Disabled) => "disabled",
        None => "model default",
    }
}
