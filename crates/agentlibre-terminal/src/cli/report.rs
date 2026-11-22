//! The progress of a `-p` turn on stderr (D109).

use std::io::{self, Write};

use crate::action::Outcome;
use crate::agent::{Report, Sink};
use crate::backend::ToolCall;
use crate::i18n::{self, Lang, m};
use crate::session::{CutOff, Event, ForgeKind};
use crate::tools::{spec::call_summary, ToolResult};
use crate::tr;

/// How much of a command the progress line shows.
const COMMAND_WIDTH: usize = 200;

pub struct StderrReport {
    pub lang: Lang,
    thinking: bool,
    /// The call whose result is next.
    call: Option<ToolCall>,
}

impl crate::plane::Turn for StderrReport {
    fn line(&mut self, line: &str) {
        eprintln!("{line}");
    }
}

impl Sink for StderrReport {
    /// Takes what the turn tells (AGL-067, D3).
    fn send(&mut self, report: Report) {
        self.take(report);
    }
}

impl StderrReport {
    pub fn new(lang: Lang) -> StderrReport {
        StderrReport {
            lang,
            thinking: false,
            call: None,
        }
    }

    fn take(&mut self, report: Report) {
        match report {
            Report::Event(Event::State(state)) => self.state(&state),
            Report::Thinking(piece) => self.thinking(&piece),
            Report::ThinkingDone => self.thinking_done(),
            Report::Event(Event::Call { text, call }) => {
                if let Some(text) = text {
                    self.text(&text);
                }
                self.call(&call);
                self.call = Some(call);
            }
            Report::Event(Event::Result {
                outcome, content, ..
            }) => {
                if let Some(call) = self.call.take() {
                    self.result(&call, &ToolResult { outcome, content });
                }
            }
            Report::Event(Event::Memory { name, kind, text }) => {
                self.memory(&name, kind, text.as_deref())
            }
            Report::Event(Event::Forge {
                name,
                kind,
                section,
            }) => self.forge(&name, kind, section.as_deref()),
            Report::Event(Event::CutOff(cut)) => self.cut_off(&cut),
            Report::Stirred {
                attempt,
                max,
                error,
            } => self.stirred(attempt, max, &error),
            Report::Compacted => self.compacted(),
            _ => {}
        }
    }

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
        eprintln!("> {}: {}", call.name, cut(&call_summary(call), COMMAND_WIDTH));
    }

    fn result(&mut self, call: &ToolCall, result: &ToolResult) {
        eprintln!("  {}", result_summary(call, result, self.lang));
    }

    pub fn memory(&mut self, name: &str, kind: crate::memory::Kind, text: Option<&str>) {
        eprintln!("{}", crate::records::memory_line(name, kind, self.lang));
        if let Some(text) = text {
            eprintln!("{text}");
        }
    }

    fn forge(&mut self, name: &str, kind: ForgeKind, section: Option<&str>) {
        eprintln!("{}", crate::records::forge_line(name, kind, section, self.lang));
    }

    fn compacted(&mut self) {
        eprintln!("[compacted]");
    }

    fn cut_off(&mut self, cut: &CutOff) {
        eprintln!("{}", crate::status::cut_off_line(cut, self.lang));
    }

    fn stirred(&mut self, attempt: u32, _max: u32, error: &str) {
        eprintln!("{}", crate::status::stir_line(attempt, error, self.lang));
    }
}

/// A summary cut to `chars` characters, an ellipsis ending what was cut.
fn cut(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_owned();
    }
    let kept: String = text.chars().take(chars.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
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
    use crate::action::{Outcome, Value};

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
        assert_eq!(call_summary(&bash("cat <<EOF > a\nx\nEOF")), "cat <<EOF > a");
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
    use crate::action::Value;

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

#[cfg(test)]
mod memory_tests {
    use super::*;
    use crate::action::Value;

    #[test]
    fn a_memory_call_shows_the_action_and_what_it_is_about() {
        let call = |args: &[(&str, &str)]| ToolCall {
            id: "c".into(),
            name: "memory".into(),
            args: args.iter().map(|(n, v)| (n.to_string(), Value::Text(v.to_string()))).collect(),
        };
        assert_eq!(call_summary(&call(&[("action", "add"), ("title", "Build"), ("text", "x")])), "add Build");
        assert_eq!(call_summary(&call(&[("action", "show"), ("name", "build")])), "show build");
        assert_eq!(call_summary(&call(&[("action", "show")])), "show");
    }
}

#[cfg(test)]
mod forge_tests {
    use super::*;
    use crate::action::Value;

    fn call(args: &[(&str, &str)]) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: "forge".into(),
            args: args
                .iter()
                .map(|(n, v)| (n.to_string(), Value::Text(v.to_string())))
                .collect(),
        }
    }

    #[test]
    fn a_forge_call_shows_the_action_what_it_is_about_and_what_it_does() {
        assert_eq!(
            call_summary(&call(&[
                ("action", "append"),
                ("name", "agl-066"),
                ("section", "Progress"),
                ("text", "the bullet"),
            ])),
            "append agl-066 Progress"
        );
        assert_eq!(
            call_summary(&call(&[("action", "status"), ("name", "agl-066"), ("state", "done")])),
            "status agl-066 done"
        );
        assert_eq!(
            call_summary(&call(&[
                ("action", "rename"),
                ("name", "agl-066"),
                ("new", "agl-067"),
            ])),
            "rename agl-066 → agl-067"
        );
        assert_eq!(
            call_summary(&call(&[("action", "new"), ("title", "The task")])),
            "new The task"
        );
        assert_eq!(
            call_summary(&call(&[("action", "show"), ("name", "agl-066")])),
            "show agl-066"
        );
        assert_eq!(call_summary(&call(&[("action", "list")])), "list");
        // The edit shows the first lines of its texts (AGL-066, D18).
        assert_eq!(
            call_summary(&call(&[
                ("action", "edit"),
                ("name", "agl-066"),
                ("old", "the old words"),
                ("new", "the new words"),
            ])),
            "edit agl-066 “the old words” → “the new words”"
        );
    }
}
