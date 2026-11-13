//! The records of a session (AGL-066): what the transcript draws, compact
//! by default and in full in the inspect mode. The events are grouped into
//! the records; how a record is drawn is a function of the record, the
//! pluggable piece for the summaries of a small model later (D3).

use std::time::Duration;

use crate::action::{Outcome, Value};
use crate::backend::ToolCall;
use crate::i18n::{Lang, m, repl as texts};
use crate::media::{Kind as MediaKind, Media};
use crate::memory::Kind as MemoryKind;
use crate::session::{Event, ForgeKind, TransitionLine};
use crate::theme::Role;
use crate::tools::ToolResult;
use crate::tr;

/// A phase of a step and its time, in the order they were spent (D2).
#[derive(Clone, Debug, PartialEq)]
pub struct PhaseSpan {
    /// The phase, in the language, as the dock names it.
    pub label: String,
    pub time: Duration,
}

/// One record of a session (AGL-066): a block the transcript draws
/// compactly and the inspect mode can expand to its full data (D4).
#[derive(Clone, Debug, PartialEq)]
pub enum Record {
    /// A message of the human (D5: the inspect mode jumps between them).
    User(String),
    /// A visit of a state (D138): `[state]`.
    State(String),
    /// A step: the model's thinking and the text it wrote, then its tool
    /// call with the result (D19). The block's line begins with the
    /// thinking's line, in the tool's style; the compact frame shows the
    /// input's hint and the phases' times (D1, D2, D6).
    Tool {
        call: ToolCall,
        result: Option<ToolResult>,
        /// One line of the input (D6): the command, the file, the query.
        hint: String,
        /// The phases of the step and their times, in the order spent (D2).
        phases: Vec<PhaseSpan>,
        /// The time the call took, when it is known.
        time: Option<Duration>,
        /// The model's thinking of the step, before the call (D3, D19).
        thinking: Option<String>,
        /// The text the model wrote before the call (D9).
        text: Option<String>,
    },
    /// The turn's answer, the main content, in full (D1); its step's
    /// thinking's line goes before it (D19).
    Answer {
        text: String,
        time: Option<Duration>,
        thinking: Option<String>,
    },
    /// A line of the session itself: a stir, a cut off, a write of the
    /// forge or the memory, a medium, a compaction, a command.
    Notice {
        line: String,
        role: Role,
        /// What the inspect mode shows when the record is expanded.
        detail: Option<String>,
    },
}

/// A call's outcome as the transcript and the inspect mode print it;
/// `None` is a call whose result is not known.
pub fn status(outcome: Option<Outcome>) -> (String, Role) {
    match outcome {
        Some(Outcome::Ok) => ("ok".into(), Role::StatusSuccess),
        Some(Outcome::Fail) => ("fail".into(), Role::StatusFailure),
        Some(Outcome::Timeout) => ("timeout".into(), Role::StatusPending),
        Some(Outcome::Interrupted) => ("interrupted".into(), Role::Muted),
        None => ("…".into(), Role::StatusPending),
    }
}

/// The items of the phases' line (D2): `label time`, in the order spent.
pub fn phase_items(phases: &[PhaseSpan]) -> Vec<String> {
    phases
        .iter()
        .map(|span| format!("{} {}", span.label, texts::duration(span.time.as_secs())))
        .collect()
}

/// The line where the agent wrote to the forge (AGL-008).
pub fn forge_line(name: &str, kind: ForgeKind, section: Option<&str>, lang: Lang) -> String {
    match kind {
        ForgeKind::Created => tr!(lang, m::FORGE_EVENT_CREATED, name = name),
        ForgeKind::Edited => tr!(lang, m::FORGE_EVENT_EDITED, name = name),
        ForgeKind::Appended => tr!(
            lang,
            m::FORGE_EVENT_APPENDED,
            name = name,
            section = section.unwrap_or_default()
        ),
        ForgeKind::Removed => tr!(lang, m::FORGE_EVENT_REMOVED, name = name),
        ForgeKind::StatusChanged => tr!(
            lang,
            m::FORGE_EVENT_STATUS,
            name = name,
            to = section.unwrap_or_default()
        ),
        ForgeKind::Renamed => tr!(
            lang,
            m::FORGE_EVENT_RENAMED,
            name = name,
            old = section.unwrap_or_default()
        ),
    }
}

/// The line of a medium the session took (AGL-043 F17).
pub fn media_line(medium: &Media, lang: Lang) -> String {
    let head = format!("{} {}", medium.placeholder(), medium.name);
    match (&medium.kind, &medium.transcript) {
        (MediaKind::Audio, Some(transcript)) => format!("{head} — “{transcript}”"),
        (MediaKind::Video, _) => tr!(
            lang,
            m::MEDIA_EVENT_FRAMES,
            head = head,
            frames = medium.frames.len()
        ),
        _ => head,
    }
}

/// The line where the memory was written (AGL-006, M28).
pub fn memory_line(name: &str, kind: MemoryKind, lang: Lang) -> String {
    tr!(lang, m::MEMORY_EVENT, change = format!("{}{name}", kind.sign()))
}

fn notice(line: String, role: Role, detail: Option<String>) -> Record {
    Record::Notice { line, role, detail }
}

/// One line of a call's input (D6): the command, the file of an edit, the
/// query of a search. The `result` is what a forge `list` shows of its
/// output (D18).
pub fn call_hint(call: &ToolCall, result: Option<&ToolResult>) -> String {
    let text = |name: &str| {
        call.args
            .iter()
            .find_map(|(key, value)| match value {
                Value::Text(value) if key == name => Some(value.as_str()),
                _ => None,
            })
            .unwrap_or_default()
    };
    let head: String = match call.name.as_str() {
        "edit" => text("path").to_owned(),
        "search" => format!("{}: {}", text("source"), text("query")),
        "forge" => forge_hint(call, result),
        "memory" => crate::cli::report::call_summary(call),
        _ => text("command").to_owned(),
    };
    let line = head.lines().next().unwrap_or_default();
    if !line.is_empty() {
        return line.to_owned();
    }
    // A call whose input has no line of its own keeps its first text
    // argument (AGL-066, D12).
    call.args
        .iter()
        .find_map(|(_, value)| match value {
            Value::Text(value) => value
                .lines()
                .next()
                .filter(|line| !line.is_empty())
                .map(str::to_owned),
            _ => None,
        })
        .unwrap_or_default()
}

/// The hint of a forge call (D10, D18): its summary, or, for a `list`,
/// the first tasks of its result, chronologically, the newest first.
fn forge_hint(call: &ToolCall, result: Option<&ToolResult>) -> String {
    let action = call
        .args
        .iter()
        .find_map(|(key, value)| match value {
            Value::Text(value) if key == "action" => Some(value.as_str()),
            _ => None,
        })
        .unwrap_or_default();
    if action == "list"
        && let Some(result) = result
        && result.outcome == Outcome::Ok
        && let Some(brief) = forge_list_brief(result)
    {
        return format!("list {brief}");
    }
    crate::cli::report::call_summary(call)
}

/// The first tasks of a forge `list` result, the newest first (D18):
/// `name status, … (+N)`.
fn forge_list_brief(result: &ToolResult) -> Option<String> {
    let mut tasks: Vec<(String, String)> = Vec::new();
    for line in result.content.lines() {
        let fields: Vec<&str> = line.split_whitespace().take(4).collect();
        let [name, _type, status, date] = fields.as_slice() else {
            continue;
        };
        // The date is `YYYY-MM-DD`, so the rows sort by it.
        if date.len() == 10 && date.chars().filter(|ch| *ch == '-').count() == 2 {
            tasks.push((date.to_string(), format!("{name} {status}")));
        }
    }
    if tasks.is_empty() {
        return None;
    }
    tasks.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    let shown: Vec<&str> = tasks.iter().take(3).map(|(_, task)| task.as_str()).collect();
    let rest = tasks.len().saturating_sub(3);
    let mut brief = shown.join(", ");
    if rest > 0 {
        brief.push_str(&format!(" (+{rest})"));
    }
    Some(brief)
}

/// The label of a phase as the observer names it in the file, in
/// `lang`; `None` for a phase the frame does not name.
fn phase_label(name: &str, tool: &str, lang: Lang) -> Option<String> {
    Some(match name {
        "Prefill" => tr!(lang, m::PREFILL).to_string(),
        "Thinking" => tr!(lang, m::THINKING_ACTIVITY).to_string(),
        "Writing" => tr!(lang, m::WRITING).to_string(),
        "Running" => tr!(lang, m::RUNNING, tool = tool).to_string(),
        "Compacting" => tr!(lang, m::COMPACTING).to_string(),
        "StartingServer" => tr!(lang, m::STARTING_SERVER).to_string(),
        "Transcribing" => tr!(lang, m::TRANSCRIBING_ACTIVITY).to_string(),
        "Retry" => "Retry".into(),
        _ => return None,
    })
}

/// The phases of one step from the `transition` lines of its `turn` and
/// `step`, with their times summed, and the time of its run (D2).
fn step_phases(
    turn: usize,
    step: usize,
    transitions: &[TransitionLine],
    tool: &str,
    lang: Lang,
) -> (Vec<PhaseSpan>, Option<Duration>) {
    let mut spans: Vec<PhaseSpan> = Vec::new();
    let mut run: Option<Duration> = None;
    for line in transitions {
        if line.turn != turn || line.step != step || line.from == "Ready" {
            continue;
        }
        let Some(label) = phase_label(&line.from, tool, lang) else {
            continue;
        };
        let time = Duration::from_millis(line.ms);
        if line.from == "Running" {
            run = Some(run.unwrap_or(Duration::ZERO) + time);
        }
        match spans.iter_mut().find(|span| span.label == label) {
            Some(span) => span.time += time,
            None => spans.push(PhaseSpan { label, time }),
        }
    }
    (spans, run)
}

/// The events of a session into the records (AGL-066). `transitions` are
/// the phases' times of the steps, by their turn and step; the steps are
/// the calls, 1-based within their turn, and the turns the user messages.
pub fn group(events: &[Event], transitions: &[TransitionLine], lang: Lang) -> Vec<Record> {
    let mut records = Vec::new();
    let mut turn = 0;
    let mut step = 0;
    // Only a change of state is a record (AGL-046, D13), as in the
    // transcript: the first visit of a turn is not a change.
    let mut state_entered = false;
    // The thinking of a step goes with its record: the call of the step,
    // or the answer (D19).
    let mut pending_thinking: Option<String> = None;
    for event in events {
        match event {
            Event::User(text) => {
                turn += 1;
                step = 0;
                state_entered = false;
                records.push(Record::User(text.clone()));
            }
            Event::State(state) => {
                if std::mem::replace(&mut state_entered, true) {
                    records.push(Record::State(state.clone()));
                }
            }
            Event::Thinking(text) => {
                // The thinking goes with the record of its step (D19).
                pending_thinking = Some(text.clone());
            }
            Event::Call { text, call } => {
                step += 1;
                let (phases, time) = step_phases(turn, step, transitions, &call.name, lang);
                records.push(Record::Tool {
                    call: call.clone(),
                    result: None,
                    hint: call_hint(call, None),
                    phases,
                    time,
                    thinking: pending_thinking.take(),
                    text: text.clone(),
                });
            }
            Event::Result {
                id,
                outcome,
                content,
            } => {
                // The last call of the id without a result (D108).
                let Some(offset) = records.iter().rev().position(|record| {
                    matches!(
                        record,
                        Record::Tool { call, result, .. } if call.id == *id && result.is_none()
                    )
                }) else {
                    continue;
                };
                let index = records.len() - 1 - offset;
                if let Record::Tool { call, result, hint, .. } = &mut records[index] {
                    *result = Some(ToolResult {
                        outcome: *outcome,
                        content: content.clone(),
                    });
                    // The hint of a forge `list` is its result (D18).
                    *hint = call_hint(call, result.as_ref());
                }
            }
            Event::Answer(text) => records.push(Record::Answer {
                text: text.clone(),
                time: None,
                thinking: pending_thinking.take(),
            }),
            Event::CutOff(cut) => records.push(notice(
                crate::status::cut_off_line(cut, lang),
                Role::Muted,
                None,
            )),
            Event::Stir { attempt, error } => records.push(notice(
                crate::status::stir_line(*attempt, error, lang),
                Role::Muted,
                Some(error.clone()),
            )),
            Event::Stop(_) => {}
            Event::Forge {
                name,
                kind,
                section,
            } => records.push(notice(
                forge_line(name, *kind, section.as_deref(), lang),
                Role::Muted,
                None,
            )),
            Event::Memory { name, kind, text } => records.push(notice(
                memory_line(name, *kind, lang),
                Role::Muted,
                text.clone(),
            )),
            Event::Media(medium) => records.push(notice(media_line(medium, lang), Role::Muted, None)),
            Event::Compact { summary, .. } => records.push(notice(
                tr!(lang, m::COMPACTING).to_string(),
                Role::Muted,
                Some(summary.clone()),
            )),
            Event::Command(run) => records.push(notice(
                run.line.clone(),
                if run.failed {
                    Role::StatusFailure
                } else {
                    Role::Muted
                },
                run.output.clone(),
            )),
        }
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Value;

    fn call(id: &str, command: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "bash".into(),
            args: vec![
                ("command".into(), Value::Text(command.into())),
                ("timeout".into(), Value::Int(30)),
            ],
        }
    }

    fn line(turn: usize, step: usize, from: &str, ms: u64) -> TransitionLine {
        TransitionLine {
            turn,
            step,
            from: from.into(),
            to: "Prefill".into(),
            event: "tool_end".into(),
            ms,
            tokens: None,
            tool: None,
        }
    }

    #[test]
    fn a_turn_groups_into_its_records() {
        let events = [
            Event::User("Fix it".into()),
            Event::State("work".into()),
            Event::Thinking("Let me look.\nMore thoughts.".into()),
            Event::Call {
                text: Some("I will run it.".into()),
                call: call("c1", "cargo test"),
            },
            Event::Result {
                id: "c1".into(),
                outcome: Outcome::Ok,
                content: "ok\n".into(),
            },
            Event::Call {
                text: None,
                call: call("c2", "ls"),
            },
            Event::Result {
                id: "c2".into(),
                outcome: Outcome::Fail,
                content: "no\n".into(),
            },
            Event::Answer("Done.".into()),
            Event::User("Thanks".into()),
            Event::State("work".into()),
            Event::State("done".into()),
        ];
        let records = group(&events, &[], Lang::En);
        assert_eq!(
            records,
            vec![
                Record::User("Fix it".into()),
                // The thinking and the text go with the record of their
                // step (D19); the first visit of the turn is not a change
                // (AGL-046, D13).
                Record::Tool {
                    call: call("c1", "cargo test"),
                    result: Some(ToolResult {
                        outcome: Outcome::Ok,
                        content: "ok\n".into(),
                    }),
                    hint: "cargo test".into(),
                    phases: Vec::new(),
                    time: None,
                    thinking: Some("Let me look.\nMore thoughts.".into()),
                    text: Some("I will run it.".into()),
                },
                Record::Tool {
                    call: call("c2", "ls"),
                    result: Some(ToolResult {
                        outcome: Outcome::Fail,
                        content: "no\n".into(),
                    }),
                    hint: "ls".into(),
                    phases: Vec::new(),
                    time: None,
                    thinking: None,
                    text: None,
                },
                Record::Answer {
                    text: "Done.".into(),
                    time: None,
                    thinking: None,
                },
                Record::User("Thanks".into()),
                // The first visit of the turn is not a change.
                Record::State("done".into()),
            ]
        );
    }

    #[test]
    fn a_call_without_its_result_stays_open() {
        let events = [
            Event::User("Fix it".into()),
            Event::Call {
                text: None,
                call: call("c1", "ls"),
            },
        ];
        let records = group(&events, &[], Lang::En);
        assert_eq!(
            records[1],
            Record::Tool {
                call: call("c1", "ls"),
                result: None,
                hint: "ls".into(),
                phases: Vec::new(),
                time: None,
                thinking: None,
                text: None,
            }
        );
    }

    #[test]
    fn the_phases_bind_to_their_step() {
        let events = [
            Event::User("Fix it".into()),
            Event::Call {
                text: None,
                call: call("c1", "ls"),
            },
            Event::Result {
                id: "c1".into(),
                outcome: Outcome::Ok,
                content: "ok".into(),
            },
            Event::Call {
                text: None,
                call: call("c2", "ls"),
            },
            Event::Result {
                id: "c2".into(),
                outcome: Outcome::Ok,
                content: "ok".into(),
            },
            Event::User("More".into()),
            Event::Call {
                text: None,
                call: call("c3", "ls"),
            },
            Event::Result {
                id: "c3".into(),
                outcome: Outcome::Ok,
                content: "ok".into(),
            },
        ];
        // Step 1 of turn 1 reads 100 ms and thinks 200 ms; the line of the
        // ready's gap is left out; step 2 runs 50 ms; turn 2 has no lines.
        let transitions = [
            line(1, 1, "Ready", 9000),
            line(1, 1, "Prefill", 100),
            line(1, 1, "Thinking", 200),
            line(1, 1, "Running", 50),
            line(1, 2, "Running", 50),
        ];
        let records = group(&events, &transitions, Lang::En);
        if let (Record::Tool { phases, time, .. }, Record::Tool { phases: p2, time: t2, .. }) =
            (&records[1], &records[2])
        {
            assert_eq!(
                (phases, time),
                (
                    &vec![
                        PhaseSpan {
                            label: "Reading the prompt".into(),
                            time: Duration::from_millis(100),
                        },
                        PhaseSpan {
                            label: "Thinking".into(),
                            time: Duration::from_millis(200),
                        },
                        PhaseSpan {
                            label: "Running bash".into(),
                            time: Duration::from_millis(50),
                        },
                    ],
                    &Some(Duration::from_millis(50)),
                )
            );
            assert_eq!(p2, &vec![PhaseSpan {
                label: "Running bash".into(),
                time: Duration::from_millis(50),
            }]);
            assert_eq!(*t2, Some(Duration::from_millis(50)));
        } else {
            panic!("the calls are records");
        }
        // The first step of turn 2 holds no phases of turn 1.
        if let Record::Tool { phases, time, .. } = &records[4] {
            assert!(phases.is_empty());
            assert_eq!(time, &None);
        } else {
            panic!("the call is a record");
        }
    }

    #[test]
    fn the_hint_is_one_line_of_the_input() {
        assert_eq!(call_hint(&call("c1", "ls\npwd"), None), "ls");
        let edit = ToolCall {
            id: "c1".into(),
            name: "edit".into(),
            args: vec![
                ("path".into(), Value::Text("a.rs".into())),
                ("old".into(), Value::Text("x".into())),
                ("new".into(), Value::Text("y".into())),
            ],
        };
        assert_eq!(call_hint(&edit, None), "a.rs");
        let search = ToolCall {
            id: "c1".into(),
            name: "search".into(),
            args: vec![
                ("source".into(), Value::Text("web".into())),
                ("query".into(), Value::Text("agentLIBRE".into())),
            ],
        };
        assert_eq!(call_hint(&search, None), "web: agentLIBRE");
        let forge = ToolCall {
            id: "c1".into(),
            name: "forge".into(),
            args: vec![
                ("action".into(), Value::Text("append".into())),
                ("name".into(), Value::Text("agl-066".into())),
                ("section".into(), Value::Text("Progress".into())),
                ("text".into(), Value::Text("the bullet".into())),
            ],
        };
        assert_eq!(call_hint(&forge, None), "append agl-066 Progress");
        let memory = ToolCall {
            id: "c1".into(),
            name: "memory".into(),
            args: vec![
                ("action".into(), Value::Text("add".into())),
                ("title".into(), Value::Text("Build".into())),
            ],
        };
        assert_eq!(call_hint(&memory, None), "add Build");
        // A call whose input has no known line keeps its first text
        // argument (D12).
        let unknown = ToolCall {
            id: "c1".into(),
            name: "view".into(),
            args: vec![("path".into(), Value::Text("/tmp/a.png".into()))],
        };
        assert_eq!(call_hint(&unknown, None), "/tmp/a.png");
    }

    #[test]
    fn the_forge_list_hint_shows_the_newest_first() {
        // AGL-066, D18: the first tasks of the result, chronologically,
        // the newest first; the rows it does not name stay out.
        let result = ToolResult {
            outcome: Outcome::Ok,
            content: "agl-001-a spec done 2026-11-01\nglitch\nagl-003-c spec open 2026-11-09\nagl-002-b spec in-progress 2026-11-05\nagl-004-d spec backlog 2026-11-11".into(),
        };
        let call = ToolCall {
            id: "c1".into(),
            name: "forge".into(),
            args: vec![("action".into(), Value::Text("list".into()))],
        };
        assert_eq!(
            call_hint(&call, Some(&result)),
            "list agl-004-d backlog, agl-003-c open, agl-002-b in-progress (+1)"
        );
        // Without its result, the hint is the summary; a failed list keeps
        // it too.
        assert_eq!(call_hint(&call, None), "list");
        let failed = ToolResult {
            outcome: Outcome::Fail,
            content: "no such store".into(),
        };
        assert_eq!(call_hint(&call, Some(&failed)), "list");
    }

    #[test]
    fn the_forge_edit_hint_shows_what_changed() {
        // AGL-066, D18: the edit's first lines of old and new.
        let edit = ToolCall {
            id: "c1".into(),
            name: "forge".into(),
            args: vec![
                ("action".into(), Value::Text("edit".into())),
                ("name".into(), Value::Text("agl-066".into())),
                ("old".into(), Value::Text("the old words\nand more".into())),
                ("new".into(), Value::Text("the new words".into())),
            ],
        };
        assert_eq!(
            call_hint(&edit, None),
            "edit agl-066 “the old words” → “the new words”"
        );
    }

    #[test]
    fn the_thinking_of_a_step_goes_with_its_record() {
        // AGL-066, D19: the thinking belongs to the record of its step;
        // the time of its Thinking phase stays in the step's phases.
        let events = [
            Event::User("go".into()),
            Event::Thinking("hm".into()),
            Event::Call {
                text: None,
                call: call("c1", "ls"),
            },
            Event::Result {
                id: "c1".into(),
                outcome: Outcome::Ok,
                content: "ok".into(),
            },
            Event::Thinking("again".into()),
            Event::Call {
                text: None,
                call: call("c2", "pwd"),
            },
            Event::Result {
                id: "c2".into(),
                outcome: Outcome::Ok,
                content: "here".into(),
            },
            Event::Answer("Done.".into()),
        ];
        let transitions = [
            line(1, 1, "Thinking", 2000),
            line(1, 2, "Thinking", 500),
        ];
        let records = group(&events, &transitions, Lang::En);
        let (first, second, answer) = match &records[..] {
            [_, first, second, answer] => (first, second, answer),
            other => panic!("unexpected records: {other:?}"),
        };
        let Record::Tool { thinking, phases, .. } = first else {
            panic!("the first step is a tool: {first:?}")
        };
        assert_eq!(thinking.as_deref(), Some("hm"));
        assert!(phases
            .iter()
            .any(|span| span.time == Duration::from_millis(2000)));
        let Record::Tool { thinking, .. } = second else {
            panic!("the second step is a tool: {second:?}")
        };
        assert_eq!(thinking.as_deref(), Some("again"));
        let Record::Answer { thinking, .. } = answer else {
            panic!("the last record is the answer: {answer:?}")
        };
        assert_eq!(thinking.as_deref(), None);
    }

    #[test]
    fn the_status_of_an_outcome() {
        assert_eq!(status(Some(Outcome::Ok)), ("ok".into(), Role::StatusSuccess));
        assert_eq!(status(None), ("…".into(), Role::StatusPending));
    }
}
