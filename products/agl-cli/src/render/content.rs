use super::*;
use anyhow::ensure;

pub(crate) fn format_worked_for(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds == 0 {
        return "<1s".to_owned();
    }
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{}m {}s", minutes, seconds % 60);
    }
    format!("{}h {}m", minutes / 60, minutes % 60)
}

pub(crate) fn elapsed_between_ms(started_at_ms: i64, completed_at_ms: i64) -> Duration {
    let elapsed_ms = completed_at_ms.saturating_sub(started_at_ms).max(0) as u64;
    Duration::from_millis(elapsed_ms)
}

pub(crate) fn json_to_terminal(value: &serde_json::Value, theme: &TerminalTheme) -> String {
    fn render(value: &serde_json::Value, depth: usize, output: &mut String, theme: &TerminalTheme) {
        let indent = |level: usize| "  ".repeat(level);
        match value {
            serde_json::Value::Object(entries) => {
                output.push_str(&theme.paint("muted", "{"));
                if !entries.is_empty() {
                    output.push('\n');
                }
                for (index, (key, value)) in entries.iter().enumerate() {
                    output.push_str(&indent(depth + 1));
                    let key = serde_json::to_string(key).unwrap_or_else(|_| "\"?\"".into());
                    output.push_str(&theme.paint("json_key", &key));
                    output.push_str(&theme.paint("muted", ": "));
                    render(value, depth + 1, output, theme);
                    if index + 1 != entries.len() {
                        output.push_str(&theme.paint("muted", ","));
                    }
                    output.push('\n');
                }
                if !entries.is_empty() {
                    output.push_str(&indent(depth));
                }
                output.push_str(&theme.paint("muted", "}"));
            }
            serde_json::Value::Array(items) => {
                output.push_str(&theme.paint("muted", "["));
                if !items.is_empty() {
                    output.push('\n');
                }
                for (index, item) in items.iter().enumerate() {
                    output.push_str(&indent(depth + 1));
                    render(item, depth + 1, output, theme);
                    if index + 1 != items.len() {
                        output.push_str(&theme.paint("muted", ","));
                    }
                    output.push('\n');
                }
                if !items.is_empty() {
                    output.push_str(&indent(depth));
                }
                output.push_str(&theme.paint("muted", "]"));
            }
            serde_json::Value::String(value) => {
                let value = serde_json::to_string(value).unwrap_or_else(|_| "\"?\"".into());
                output.push_str(&theme.paint("json_string", &safe_terminal_text(&value)));
            }
            serde_json::Value::Number(value) => {
                output.push_str(&theme.paint("json_number", &value.to_string()))
            }
            serde_json::Value::Bool(value) => {
                output.push_str(&theme.paint("json_boolean", &value.to_string()))
            }
            serde_json::Value::Null => output.push_str(&theme.paint("json_null", "null")),
        }
    }

    let mut output = String::new();
    render(value, 0, &mut output, theme);
    output
}

pub(crate) async fn view_tool(
    client: &AgentClient,
    run: Option<AgentRunId>,
    args: &str,
    theme: &TerminalTheme,
) -> Result<()> {
    let run = run.context("нет активного Run; сначала выполните сообщение")?;
    let mut words = args.split_whitespace();
    let ordinal: u32 = words
        .next()
        .context("использование: /tool N [input|result]")?
        .parse()
        .context("ordinal операции должен быть числом")?;
    let mode = words.next().unwrap_or("result");
    ensure!(
        matches!(mode, "input" | "result"),
        "использование: /tool N [input|result]"
    );
    ensure!(
        words.next().is_none(),
        "использование: /tool N [input|result]"
    );
    let key = agl_core::agent::AgentOperationKey {
        run_id: run,
        ordinal: std::num::NonZeroU32::new(ordinal).context("ordinal должен быть больше нуля")?,
    };
    let operation = client
        .operation_view(key)
        .await
        .context("не удалось получить операцию")?;
    if mode == "input" {
        show_full_text(
            &safe_terminal_text(&serde_json::to_string_pretty(&operation.request)?),
            theme,
        );
        return Ok(());
    }
    if let Some(result) = operation.result {
        if let agl_core::agent::AgentOperationResult::Tool(result) = result {
            let (tool_id, input) = match &operation.request {
                agl_core::agent::AgentOperationRequest::Tool(tool) => {
                    (Some(tool.tool_id.as_str()), Some(&tool.input))
                }
                _ => (None, None),
            };
            show_full_text(
                &format_tool_content_for(tool_id, result.content.as_text(), input, theme),
                theme,
            );
        } else {
            println!(
                "{}",
                safe_terminal_text(&serde_json::to_string_pretty(&result)?)
            );
        }
    } else if let Some(failure) = operation.failure {
        println!(
            "operation failed: {}",
            safe_terminal_text(&serde_json::to_string_pretty(&failure)?)
        );
    } else {
        println!(
            "operation state: {:?} (result not available)",
            operation.state
        );
    }
    Ok(())
}

pub(crate) fn format_tool_content_for(
    tool_id: Option<&str>,
    text: &str,
    input: Option<&serde_json::Value>,
    theme: &TerminalTheme,
) -> String {
    let value = serde_json::from_str::<serde_json::Value>(text);
    if let Ok(value) = &value {
        match tool_id.unwrap_or("") {
            "agentlibre.execution:command.exec" => {
                let state = value
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let outcome = value
                    .get("outcome")
                    .map(format_execution_outcome)
                    .unwrap_or_else(|| "pending".to_owned());
                let mut result = format!("state: {state}\noutcome: {outcome}");
                for name in ["stdout", "stderr"] {
                    if let Some(text) = value
                        .get(name)
                        .and_then(|v| v.as_str())
                        .filter(|v| !v.is_empty())
                    {
                        result.push_str(&format!("\n{name}:\n{}", safe_terminal_text(text)));
                    }
                }
                if value
                    .get("truncated")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false)
                {
                    result.push_str("\noutput truncated: true");
                }
                return result;
            }
            "agentlibre.builtins:fs_read" => {
                let path = value.get("path").and_then(|v| v.as_str()).unwrap_or("-");
                let mut result = format!("path: {path}\n");
                if let Some(lines) = value.get("lines").and_then(|v| v.as_array()) {
                    for line in lines {
                        let number = line.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
                        let text = line.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        result.push_str(&format!("{number:>6}  {}\n", safe_terminal_text(text)));
                    }
                }
                return result.trim_end().to_owned();
            }
            "agentlibre.builtins:fs_apply_patch" => {
                let status = value
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let mut result = format!("status: {status}");
                if let Some(input) = input {
                    result.push_str(&format!(
                        "\nrequest: {}",
                        safe_terminal_text(&input.to_string())
                    ));
                }
                return result;
            }
            "agentlibre.searxng:search" => {
                if let Some(results) = value.get("results").and_then(|v| v.as_array()) {
                    return results
                        .iter()
                        .map(|item| {
                            format!(
                                "{}\n{}",
                                item.get("title").and_then(|v| v.as_str()).unwrap_or("-"),
                                item.get("url").and_then(|v| v.as_str()).unwrap_or("-")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n");
                }
            }
            _ => {}
        }
    }
    match value {
        Ok(value) => markdown_to_terminal(
            &serde_json::to_string_pretty(&value).unwrap_or_else(|_| safe_terminal_text(text)),
            theme,
        ),
        Err(_) => markdown_to_terminal(text, theme),
    }
}

pub(crate) fn format_execution_outcome(value: &serde_json::Value) -> String {
    if value.is_null() {
        return "pending".to_owned();
    }
    let Some(kind) = value.get("type").and_then(|value| value.as_str()) else {
        return safe_terminal_text(&value.to_string());
    };
    match kind {
        "exit" => value
            .get("code")
            .and_then(|value| value.as_i64())
            .map(|code| format!("exit {code}"))
            .unwrap_or_else(|| "exit".to_owned()),
        "signal" => value
            .get("signal")
            .and_then(|value| value.as_i64())
            .map(|signal| format!("signal {signal}"))
            .unwrap_or_else(|| "signal".to_owned()),
        "timed_out" => "timed out".to_owned(),
        "terminated" => "terminated".to_owned(),
        "unknown_after_service_restart" => "unknown after service restart".to_owned(),
        other => safe_terminal_text(other),
    }
}

pub(crate) fn preview_tool_output(
    text: &str,
    limits: agl_core::agent::ToolOutputPresentation,
    ordinal: u32,
    mode: &str,
    theme: &TerminalTheme,
) -> String {
    let mut output = String::new();
    let mut chars = 0_u32;
    let mut lines = 1_u32;
    let mut truncated = false;
    let mut iter = text.chars().peekable();
    while let Some(ch) = iter.next() {
        if ch == '\x1b' && iter.peek() == Some(&'[') {
            output.push(ch);
            for control in iter.by_ref() {
                output.push(control);
                if control.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        if chars == limits.chars || (ch == '\n' && lines == limits.lines) {
            truncated = true;
            break;
        }
        output.push(ch);
        chars += 1;
        if ch == '\n' {
            lines += 1;
        }
    }
    if iter.next().is_some() {
        truncated = true;
    }
    if truncated {
        output.push_str(theme.reset());
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&theme.paint(
            "muted",
            &format!(
                "… limited to {} lines / {} chars; /tool {ordinal} {mode}",
                limits.lines, limits.chars
            ),
        ));
    }
    output
}

pub(crate) fn frame_tool_output(text: &str, theme: &TerminalTheme) -> String {
    let gutter = theme.paint("input_rule", "  │ ");
    text.split('\n')
        .map(|line| format!("{gutter}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn show_full_text(text: &str, theme: &TerminalTheme) {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        println!("{text}");
        return;
    }
    use crossterm::{event, execute, terminal};
    let lines: Vec<_> = text.lines().collect();
    let page = terminal::size()
        .map(|(_, height)| height.saturating_sub(2) as usize)
        .unwrap_or(20)
        .max(1);
    let _ = terminal::enable_raw_mode();
    let mut offset = 0;
    while offset < lines.len() {
        print!("\x1b[2J\x1b[H");
        for line in lines.iter().skip(offset).take(page) {
            println!("{line}");
        }
        println!(
            "{}",
            theme.paint(
                "muted",
                &format!(
                    "-- {}/{} lines; Space/PageDown next, PageUp previous, q/Esc exit --",
                    (offset + page).min(lines.len()),
                    lines.len()
                )
            )
        );
        let Ok(event::Event::Key(key)) = event::read() else {
            break;
        };
        match key.code {
            event::KeyCode::Char('q') | event::KeyCode::Esc => break,
            event::KeyCode::Char(' ') | event::KeyCode::PageDown | event::KeyCode::Down => {
                offset = (offset + page).min(lines.len())
            }
            event::KeyCode::PageUp | event::KeyCode::Up => offset = offset.saturating_sub(page),
            _ => {}
        }
    }
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        std::io::stdout(),
        terminal::Clear(terminal::ClearType::FromCursorDown)
    );
}

pub(crate) fn markdown_to_terminal(markdown: &str, theme: &TerminalTheme) -> String {
    let mut output = String::new();
    let mut line_start = true;
    let mut code = false;
    let mut link_destinations = Vec::new();
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    for event in MarkdownParser::new_ext(markdown, options) {
        match event {
            Event::Start(Tag::Heading { .. }) => {
                if !line_start {
                    output.push('\n');
                }
                output.push_str(&theme.start("markdown_heading"));
            }
            Event::End(TagEnd::Heading(_)) => {
                output.push_str(theme.reset());
                output.push_str("\n\n");
                line_start = true;
            }
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                output.push_str("\n\n");
                line_start = true;
            }
            Event::Start(Tag::Item) => {
                if !line_start {
                    output.push('\n');
                }
                output.push_str(&theme.paint("markdown_bullet", "• "));
                line_start = false;
            }
            Event::End(TagEnd::Item) => {
                output.push('\n');
                line_start = true;
            }
            Event::Start(Tag::BlockQuote(_)) => {
                if !line_start {
                    output.push('\n');
                }
                output.push_str(&theme.paint("markdown_quote", "│ "));
                line_start = false;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                output.push('\n');
                line_start = true;
            }
            Event::Start(Tag::CodeBlock(_)) => {
                if !line_start {
                    output.push('\n');
                }
                output.push_str(&theme.start("markdown_code"));
                code = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                output.push_str(theme.reset());
                output.push('\n');
                line_start = true;
                code = false;
            }
            Event::Code(text) => {
                output.push_str(&theme.paint("markdown_inline_code", &safe_terminal_text(&text)));
            }
            Event::Text(text) => {
                output.push_str(&safe_terminal_text(&text));
                line_start = false;
            }
            Event::SoftBreak | Event::HardBreak => {
                output.push('\n');
                line_start = true;
            }
            Event::Rule => {
                output.push_str(&theme.paint("input_rule", "────────────────\n"));
                line_start = true;
            }
            Event::Start(Tag::Strong) => {
                output.push_str(&theme.start("markdown_strong"));
            }
            Event::End(TagEnd::Strong) => {
                output.push_str(theme.reset());
            }
            Event::Start(Tag::Emphasis) => {
                output.push_str(&theme.start("markdown_emphasis"));
            }
            Event::End(TagEnd::Emphasis) => {
                output.push_str(theme.reset());
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                link_destinations.push(safe_terminal_text(&dest_url));
                output.push_str(&theme.start("markdown_link"));
                output.push('[');
            }
            Event::End(TagEnd::Link) => {
                output.push(']');
                if let Some(destination) = link_destinations.pop()
                    && !destination.is_empty()
                {
                    output.push_str(" (");
                    output.push_str(&destination);
                    output.push(')');
                }
                output.push_str(theme.reset());
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                output.push_str(&safe_terminal_text(&text))
            }
            _ => {}
        }
        if code && output.ends_with('\n') {
            line_start = true;
        }
    }
    while output.ends_with("\n\n") {
        output.pop();
    }
    output
}
