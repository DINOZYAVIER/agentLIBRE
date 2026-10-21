//! The progress of a `-p` turn on stderr (D109).

use std::io::{self, Write};

use crate::action::{Outcome, Value};
use crate::agent::Report;
use crate::backend::ToolCall;
use crate::i18n::{self, Lang, m};
use crate::session::ForgeKind;
use crate::tools::ToolResult;
use crate::tr;

/// How much of a command the progress line shows.
const COMMAND_WIDTH: usize = 200;

pub struct StderrReport {
    pub lang: Lang,
    thinking: bool,
}

impl StderrReport {
    pub fn new(lang: Lang) -> StderrReport {
        StderrReport {
            lang,
            thinking: false,
        }
    }
}

impl Report for StderrReport {
    fn state(&mut self, state: &str) {
        eprintln!("[{state}]");
    }

    fn thinking(&mut self, piece: &str) {
        let mut err = io::stderr().lock();
        if !self.thinking {
            self.thinking = true;
            let _ = write!(err, "{}: ", tr!(self.lang, m::THINKING));
        }
        let _ = write!(err, "{piece}");
        let _ = err.flush();
    }

    fn thinking_done(&mut self) {
        if std::mem::take(&mut self.thinking) {
            eprintln!();
        }
    }

    fn text(&mut self, text: &str) {
        eprintln!("{text}");
    }

    fn call(&mut self, call: &ToolCall) {
        eprintln!("> {}: {}", call.name, call_summary(call));
    }

    fn result(&mut self, call: &ToolCall, result: &ToolResult) {
        eprintln!("  {}", result_summary(call, result, self.lang));
    }

    fn memory(&mut self, name: &str, kind: crate::memory::Kind, text: Option<&str>) {
        let change = format!("{}{name}", kind.sign());
        eprintln!("{}", tr!(self.lang, m::MEMORY_EVENT, change = change));
        if let Some(text) = text {
            eprintln!("{text}");
        }
    }

    fn forge(&mut self, name: &str, kind: ForgeKind, section: Option<&str>) {
        let message = match kind {
            ForgeKind::Created => tr!(self.lang, m::FORGE_EVENT_CREATED, name = name),
            ForgeKind::Edited => tr!(self.lang, m::FORGE_EVENT_EDITED, name = name),
            ForgeKind::Appended => tr!(
                self.lang,
                m::FORGE_EVENT_APPENDED,
                name = name,
                section = section.unwrap_or_default()
            ),
            ForgeKind::Removed => tr!(self.lang, m::FORGE_EVENT_REMOVED, name = name),
        };
        eprintln!("{message}");
    }

    fn compacted(&mut self) {
        eprintln!("[compacted]");
    }

    fn cut_off(&mut self) {
        eprintln!("[cut off]");
    }
}

/// `bash`: the command's first line, shortened; `search`: the source and
/// the query; `edit`: the path.
pub fn call_summary(call: &ToolCall) -> String {
    let text = |name: &str| {
        call.args
            .iter()
            .find_map(|(n, value)| match value {
                Value::Text(text) if n == name => Some(text.as_str()),
                _ => None,
            })
            .unwrap_or_default()
    };
    if call.name == "search" {
        let summary = format!("{}: {}", text("source"), text("query"));
        return summary.chars().take(COMMAND_WIDTH).collect();
    }
    if call.name != "bash" {
        return text("path").to_owned();
    }
    let command = text("command");
    let first = command.lines().next().unwrap_or_default();
    let mut summary: String = first.chars().take(COMMAND_WIDTH).collect();
    if summary.len() < command.len() {
        summary.push_str(" …");
    }
    summary
}

/// `edit`: its message; `search`: how many results; `bash`: the outcome,
/// how it ended and the length.
pub fn result_summary(call: &ToolCall, result: &ToolResult, lang: Lang) -> String {
    let outcome = result.outcome.name();
    if call.name == "search" && result.outcome == Outcome::Ok {
        // The numbered lines of the list, as the tool writes them.
        let found = result
            .content
            .lines()
            .filter(|line| {
                line.split_once(". ")
                    .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
            })
            .count();
        return format!("{outcome}: {found} results");
    }
    if call.name != "bash" {
        return format!("{outcome}: {}", result.content);
    }
    let last = result.content.lines().last().unwrap_or_default();
    let ended = ["exit code: ", "killed by signal ", "timed out after "]
        .iter()
        .any(|start| last.starts_with(start));
    let ending = if ended {
        format!(" ({last})")
    } else {
        String::new()
    };
    let lines = if result.content == "(no output)" {
        0
    } else {
        result.content.lines().count()
    };
    format!("{outcome}{ending}, {}", i18n::lines(lines, lang))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Outcome;

    fn bash(command: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: vec![("command".into(), Value::Text(command.into()))],
        }
    }

    #[test]
    fn summaries() {
        assert_eq!(call_summary(&bash("ls -la")), "ls -la");
        assert_eq!(
            call_summary(&bash("cat <<EOF > a\nx\nEOF")),
            "cat <<EOF > a …"
        );
        let failed = ToolResult {
            outcome: Outcome::Fail,
            content: "a\nb\nexit code: 101".into(),
        };
        assert_eq!(
            result_summary(&bash("x"), &failed, Lang::En),
            "fail (exit code: 101), 3 lines"
        );
        let empty = ToolResult {
            outcome: Outcome::Ok,
            content: "(no output)".into(),
        };
        assert_eq!(result_summary(&bash("x"), &empty, Lang::Uk), "ok, 0 рядків");
        let edit = ToolCall {
            id: "c2".into(),
            name: "edit".into(),
            args: vec![("path".into(), Value::Text("src/lib.rs".into()))],
        };
        assert_eq!(call_summary(&edit), "src/lib.rs");
        let edited = ToolResult {
            outcome: Outcome::Ok,
            content: "Replaced 1 occurrence in src/lib.rs.".into(),
        };
        assert_eq!(
            result_summary(&edit, &edited, Lang::En),
            "ok: Replaced 1 occurrence in src/lib.rs."
        );
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;

    fn call(args: &[(&str, &str)]) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: "search".into(),
            args: args
                .iter()
                .map(|(n, v)| (n.to_string(), Value::Text(v.to_string())))
                .collect(),
        }
    }

    #[test]
    fn a_search_call_shows_the_source_and_the_query() {
        assert_eq!(call_summary(&call(&[("query", "rust 1.99"), ("source", "web")])), "web: rust 1.99");
    }

    #[test]
    fn a_search_result_shows_how_many_results() {
        let list = "1. A\n   https://a.example/\n   one. two. 3. not a result\n2. B\n   https://b.example/\nSuggestions: x";
        let ok = ToolResult { outcome: Outcome::Ok, content: list.into() };
        assert_eq!(result_summary(&call(&[]), &ok, Lang::En), "ok: 2 results");
        let none = ToolResult { outcome: Outcome::Ok, content: "No results.".into() };
        assert_eq!(result_summary(&call(&[]), &none, Lang::En), "ok: 0 results");
        // A failure says why.
        let fail = ToolResult { outcome: Outcome::Fail, content: "The search timed out.".into() };
        assert_eq!(result_summary(&call(&[]), &fail, Lang::En), "fail: The search timed out.");
    }
}
