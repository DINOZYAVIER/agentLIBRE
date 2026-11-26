//! The records of a session (AGL-066): what the transcript draws, compact
//! by default and in full in the inspect mode. The events are grouped into
//! the records; how a record is drawn is a function of the record, the
//! pluggable piece for the summaries of a small model later (D3).

use std::time::Duration;

use agentlibre_proto::{
    Event, ForgeKind, FunctionEntry, FunctionSource, Media, MediaKind, MemoryKind,
    Outcome, PhaseTime, ToolCall, Value,
};

use crate::words as texts;
use agentlibre_common::i18n::{Lang, m};
use agentlibre_common::style::Role;
use agentlibre_common::tr;

/// What a call gave, as the records hold it (AGL-130, D9): the outcome and
/// the content of its `result` event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolResult {
    pub outcome: Outcome,
    pub content: String,
}

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
    /// A flow's wake, which opens a turn as the human's message does
    /// (AGL-121, D55): the inspect mode jumps between them too.
    Wake { flow: String, text: String },
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
        /// What the inspect mode shows when the block is focused
        /// (AGL-098, D2).
        detail: Option<String>,
    },
}

/// The kind of a record's block (AGL-098, D2): the block is the unit the
/// inspect mode's focus moves between. A block with no text of its own
/// keeps its place and shows the placeholder (AGL-095, D4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    /// A message of the human: the record is one block.
    Message,
    /// A line of the session: the record is one block.
    Line,
    /// The thinking of a step or of an answer.
    Thinking,
    /// A tool call: the text written before it and its card.
    Tool,
    /// The tool's result.
    Result,
    /// The answer's text.
    Answer,
}

impl Record {
    /// The record's blocks in the order they stand (AGL-098, D2): a step
    /// is the triple `thinking`, `tool`, `result` — a missing thinking or
    /// result keeps its place as a placeholder; an answer is `thinking`
    /// and `ANSWER`; the rest is one block.
    pub fn blocks(&self) -> Vec<BlockKind> {
        match self {
            Record::User(_) | Record::Wake { .. } => vec![BlockKind::Message],
            Record::State(_) => vec![BlockKind::Line],
            Record::Tool { .. } => vec![BlockKind::Thinking, BlockKind::Tool, BlockKind::Result],
            Record::Answer { .. } => vec![BlockKind::Thinking, BlockKind::Answer],
            Record::Notice { .. } => vec![BlockKind::Line],
        }
    }
}

/// A call's outcome as the transcript and the inspect mode print it;
/// `None` is a call whose result is not known. Capsof, so the word stands
/// out of the row it ends (AGL-085, D2).
pub fn status(outcome: Option<Outcome>) -> (String, Role) {
    match outcome {
        Some(Outcome::Ok) => ("OK".into(), Role::StatusSuccess),
        // A failed call is not a fatal error: the agent sees it and calls
        // again (AGL-066, D28), so the warn's orange, not the failure's
        // red.
        Some(Outcome::Fail) => ("FAIL".into(), Role::StatusWarn),
        Some(Outcome::Timeout) => ("TIMEOUT".into(), Role::StatusPending),
        Some(Outcome::Interrupted) => ("INTERRUPTED".into(), Role::Muted),
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
    let head = format!("[{}#{}] {}", medium.kind.label(), medium.number, medium.name);
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

/// A Function's source in the language (AGL-130, D10): the word `Source::label`
/// of the core says for it.
pub fn source_label(source: FunctionSource, lang: Lang) -> &'static str {
    match source {
        FunctionSource::Plane => tr!(lang, m::FUNCTION_SOURCE_PLANE),
        FunctionSource::Workspace => tr!(lang, m::FUNCTION_SOURCE_WORKSPACE),
        FunctionSource::User => tr!(lang, m::FUNCTION_SOURCE_USER),
        FunctionSource::BuiltIn => tr!(lang, m::FUNCTION_SOURCE_BUILTIN),
    }
}

/// The Functions of the wire as lines (AGL-036, D3; AGL-130, D10): the
/// current one marked, the columns aligned. It writes what
/// `function::catalog::lines` writes of the same entries; the kind is the
/// text of the wire.
pub fn function_lines(entries: &[FunctionEntry], lang: Lang) -> Vec<String> {
    let name_width = entries.iter().map(|e| e.name.chars().count()).max().unwrap_or(0);
    let source_width = entries
        .iter()
        .map(|e| source_label(e.source, lang).chars().count())
        .max()
        .unwrap_or(0);
    let no_model = tr!(lang, m::FUNCTION_NO_MODEL);
    let model_width = entries
        .iter()
        .filter(|e| e.error.is_none())
        .map(|e| e.model.as_deref().unwrap_or(no_model).chars().count())
        .max()
        .unwrap_or(0);
    entries
        .iter()
        .map(|entry| {
            let mark = if entry.current { '*' } else { ' ' };
            let mut line = format!(
                "{mark} {:name_width$}  {:source_width$}  ",
                entry.name,
                source_label(entry.source, lang),
            );
            match &entry.error {
                Some(error) => line.push_str(error),
                None => {
                    line.push_str(&format!("{:6}  ", entry.kind));
                    let model = entry.model.as_deref().unwrap_or(no_model);
                    match &entry.description {
                        Some(description) => {
                            line.push_str(&format!("{model:model_width$}  {description}"));
                        }
                        None => line.push_str(model),
                    }
                }
            }
            if !entry.shadows.is_empty() {
                let hidden: Vec<&str> = entry.shadows.iter().map(|s| source_label(*s, lang)).collect();
                line.push(' ');
                line.push_str(&tr!(lang, m::FUNCTION_SHADOWS, sources = hidden.join(", ")));
            }
            line.trim_end().to_owned()
        })
        .collect()
}

/// The line where the memory was written (AGL-006, M28).
pub fn memory_line(name: &str, kind: MemoryKind, lang: Lang) -> String {
    tr!(lang, m::MEMORY_EVENT, change = format!("{}{name}", kind.sign()))
}

fn notice(line: String, role: Role, detail: Option<String>) -> Record {
    Record::Notice { line, role, detail }
}

/// What a call sent, in full (AGL-095, D5): the command, the edit with its
/// lines, the search's query, every argument of a forge or a memory call.
/// The compact form of it is the call's `summary` on the wire.
pub fn call_input(call: &ToolCall) -> String {
    let text = |name: &str| call.text(name).unwrap_or_default();
    if call.name == "edit" {
        let mut out = text("path").to_owned();
        for (sign, name) in [('-', "old"), ('+', "new")] {
            for line in text(name).split('\n') {
                out.push_str(&format!("\n{sign} {line}"));
            }
        }
        return out;
    }
    if call.name == "search" {
        return format!(
            "{}: {}",
            text("source"),
            text("query")
        );
    }
    if call.name == "memory" || call.name == "forge" {
        // The whole input, argument by argument (D11); the summary
        // stays the compact form.
        let mut out = String::new();
        for (key, value) in &call.args {
            let Value::Text(value) = value else {
                continue;
            };
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("{key}: {value}"));
        }
        return out;
    }
    text("command").to_owned()
}

/// The action of a forge or a memory call, on the tool's line (D28).
pub fn call_action(call: &ToolCall) -> Option<String> {
    call.text("action")
        .filter(|action| !action.is_empty())
        .map(str::to_owned)
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
    transitions: &[PhaseTime],
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

/// The fold of a session's events into records (AGL-066; AGL-127, D3): one
/// event at a time, so that a surface that is told what a session does can
/// keep its records as they come, and `group` feeds it a whole past. The
/// number of the turn and of the step, whether the first state of a turn was
/// seen, and the thinking that waits for its step are its state.
pub struct Fold {
    lang: Lang,
    records: Vec<Record>,
    /// The turn and the step of each record that is a step's call, by the
    /// index of the record: where its phases' times are looked up.
    steps: Vec<Option<(usize, usize)>>,
    transitions: Vec<PhaseTime>,
    turn: usize,
    step: usize,
    /// Only a change of state is a record (AGL-046, D13), as in the
    /// transcript: the first visit of a turn is not a change.
    state_entered: bool,
    /// The thinking of a step goes with its record: the call of the step,
    /// or the answer (D19).
    pending_thinking: Option<String>,
}

impl Fold {
    pub fn new(lang: Lang) -> Fold {
        Fold {
            lang,
            records: Vec::new(),
            steps: Vec::new(),
            transitions: Vec::new(),
            turn: 0,
            step: 0,
            state_entered: false,
            pending_thinking: None,
        }
    }

    /// The records so far.
    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// The records, the fold done.
    pub fn into_records(self) -> Vec<Record> {
        self.records
    }

    /// The phases' times of the steps, by their turn and step: the phases of
    /// the steps already folded are made anew from them, so the times may come
    /// before the events or after them.
    pub fn times(&mut self, transitions: &[PhaseTime]) {
        self.transitions = transitions.to_vec();
        for (record, key) in self.records.iter_mut().zip(&self.steps) {
            if let (Record::Tool { call, phases, time, .. }, Some((turn, step))) = (record, key) {
                (*phases, *time) = step_phases(*turn, *step, &self.transitions, &call.name, self.lang);
            }
        }
    }

    fn push(&mut self, record: Record) {
        self.records.push(record);
        self.steps.push(None);
    }

    /// A line of the surface's own, not of the session: a refusal, an
    /// error, a stop (AGL-128).
    pub fn note(&mut self, line: String, role: Role) {
        self.push(notice(line, role, None));
    }

    /// Folds the next event of the session.
    pub fn event(&mut self, event: &Event) {
        let lang = self.lang;
        match event {
            Event::User(text) => {
                self.turn += 1;
                self.step = 0;
                self.state_entered = false;
                self.push(Record::User(text.clone()));
            }
            Event::Wake { flow, text } => {
                self.turn += 1;
                self.step = 0;
                self.state_entered = false;
                self.push(Record::Wake { flow: flow.clone(), text: text.clone() });
            }
            Event::State(state) => {
                if std::mem::replace(&mut self.state_entered, true) {
                    self.push(Record::State(state.clone()));
                }
            }
            Event::Thinking(text) => {
                // The thinking goes with the record of its step (D19).
                self.pending_thinking = Some(text.clone());
            }
            Event::Call { text, call, summary } => {
                self.step += 1;
                let (phases, time) =
                    step_phases(self.turn, self.step, &self.transitions, &call.name, lang);
                let thinking = self.pending_thinking.take();
                self.push(Record::Tool {
                    call: call.clone(),
                    result: None,
                    // The daemon's table of tools sums the call up (AGL-121,
                    // D52); an event without it shows an empty hint (AGL-130, D9).
                    hint: summary.clone().unwrap_or_default(),
                    phases,
                    time,
                    thinking,
                    text: text.clone(),
                });
                if let Some(key) = self.steps.last_mut() {
                    *key = Some((self.turn, self.step));
                }
            }
            Event::Result {
                id,
                outcome,
                content,
            } => {
                // The last call of the id without a result (D108).
                let Some(offset) = self.records.iter().rev().position(|record| {
                    matches!(
                        record,
                        Record::Tool { call, result, .. } if call.id == *id && result.is_none()
                    )
                }) else {
                    return;
                };
                let index = self.records.len() - 1 - offset;
                if let Record::Tool { result, .. } = &mut self.records[index] {
                    *result = Some(ToolResult {
                        outcome: *outcome,
                        content: content.clone(),
                    });
                }
            }
            Event::Answer(text) => {
                let thinking = self.pending_thinking.take();
                self.push(Record::Answer {
                    text: text.clone(),
                    time: None,
                    thinking,
                });
            }
            Event::CutOff(cut) => self.push(notice(
                crate::words::cut_off_line(cut, lang),
                Role::Muted,
                None,
            )),
            Event::Stir { attempt, error } => self.push(notice(
                crate::words::stir_line(*attempt, error, lang),
                Role::Muted,
                Some(error.clone()),
            )),
            // AGL-068: a forge or a memory write is in the record of its
            // call (the hint row says what it did); the events keep the
            // writes in the session's file.
            Event::Forge { .. } | Event::Memory { .. } | Event::Stop(_) => {}
            Event::Media(medium) => self.push(notice(media_line(medium, lang), Role::Muted, None)),
            Event::Compact { summary, .. } => self.push(notice(
                tr!(lang, m::COMPACTING).to_string(),
                Role::Muted,
                Some(summary.clone()),
            )),
            Event::Command(run) => self.push(notice(
                run.line.clone(),
                if run.failed {
                    Role::StatusFailure
                } else {
                    Role::Muted
                },
                run.output.clone(),
            )),
            Event::System { text, failed } => self.push(notice(
                text.clone(),
                if *failed {
                    Role::StatusFailure
                } else {
                    Role::Muted
                },
                None,
            )),
        }
    }
}

/// The events of a session into the records (AGL-066). `transitions` are
/// the phases' times of the steps, by their turn and step; the steps are
/// the calls, 1-based within their turn, and the turns the user messages.
pub fn group(events: &[Event], transitions: &[PhaseTime], lang: Lang) -> Vec<Record> {
    let mut fold = Fold::new(lang);
    fold.times(transitions);
    for event in events {
        fold.event(event);
    }
    fold.into_records()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn line(turn: usize, step: usize, from: &str, ms: u64) -> PhaseTime {
        PhaseTime {
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
                summary: Some("cargo test".into()),
            },
            Event::Result {
                id: "c1".into(),
                outcome: Outcome::Ok,
                content: "ok\n".into(),
            },
            Event::Call {
                text: None,
                call: call("c2", "ls"),
                summary: Some("ls".into()),
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
                summary: Some("ls".into()),
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
                summary: Some("ls".into()),
            },
            Event::Result {
                id: "c1".into(),
                outcome: Outcome::Ok,
                content: "ok".into(),
            },
            Event::Call {
                text: None,
                call: call("c2", "ls"),
                summary: Some("ls".into()),
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
                summary: Some("ls".into()),
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
    fn a_forge_or_a_memory_call_names_its_action() {
        // AGL-068: the action of the call is on the tool's line.
        let call_of = |name: &str, action: &str| ToolCall {
            id: "c1".into(),
            name: name.into(),
            args: vec![("action".into(), Value::Text(action.into()))],
        };
        assert_eq!(call_action(&call_of("forge", "append")), Some("append".to_owned()));
        assert_eq!(call_action(&call_of("memory", "add")), Some("add".to_owned()));
        assert_eq!(call_action(&call("c1", "ls")), None);
    }

    #[test]
    fn the_hint_of_a_step_is_its_summary_on_the_wire() {
        // AGL-130, D9: a surface draws the summary the daemon sent; an
        // event without one shows an empty hint.
        let events = [
            Event::Call {
                text: None,
                call: call("c1", "ls\npwd"),
                summary: Some("ls".into()),
            },
            Event::Call {
                text: None,
                call: call("c2", "pwd"),
                summary: None,
            },
        ];
        let hints: Vec<String> = group(&events, &[], Lang::En)
            .into_iter()
            .filter_map(|record| match record {
                Record::Tool { hint, .. } => Some(hint),
                _ => None,
            })
            .collect();
        assert_eq!(hints, ["ls", ""]);
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
                summary: Some("ls".into()),
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
                summary: Some("pwd".into()),
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
        // AGL-085, D2: capsof, so the word stands out of the row it ends.
        assert_eq!(status(Some(Outcome::Ok)), ("OK".into(), Role::StatusSuccess));
        assert_eq!(status(Some(Outcome::Fail)), ("FAIL".into(), Role::StatusWarn));
        assert_eq!(status(None), ("…".into(), Role::StatusPending));
    }

    #[test]
    fn a_system_line_is_a_notice_red_when_failed() {
        // AGL-103, D5.
        let records = group(
            &[
                Event::System { text: "the server is ready at http://x".into(), failed: false },
                Event::System { text: "the server refused to start".into(), failed: true },
            ],
            &[],
            Lang::En,
        );
        assert_eq!(
            records,
            vec![
                notice("the server is ready at http://x".into(), Role::Muted, None),
                notice("the server refused to start".into(), Role::StatusFailure, None),
            ]
        );
    }

    #[test]
    fn a_function_of_the_wire_is_a_line() {
        let entry = |name: &str, current: bool| FunctionEntry {
            name: name.into(),
            source: FunctionSource::User,
            shadows: vec![FunctionSource::BuiltIn],
            kind: "llama".into(),
            model: Some("m.gguf".into()),
            description: None,
            error: None,
            current,
        };
        let lines = function_lines(&[entry("a", true), entry("long-name", false)], Lang::En);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("* a          user  llama   m.gguf"), "{}", lines[0]);
        assert!(lines[1].starts_with("  long-name  user  llama   m.gguf"), "{}", lines[1]);
        assert!(lines[1].contains("built-in"), "{}", lines[1]);
    }

    #[test]
    fn the_same_events_at_once_and_one_by_one_make_the_same_records() {
        // AGL-127, D3: `group` is a fold fed a whole past; the times may come
        // before the events or after them.
        let events = [
            Event::User("go".into()),
            Event::State("work".into()),
            Event::Thinking("hm".into()),
            Event::Call {
                text: Some("I will run it.".into()),
                call: call("c1", "ls"),
                summary: Some("ls".into()),
            },
            Event::Result {
                id: "c1".into(),
                outcome: Outcome::Ok,
                content: "a".into(),
            },
            Event::Call {
                text: None,
                call: call("c2", "pwd"),
                summary: Some("pwd".into()),
            },
            Event::State("done".into()),
            Event::Answer("Done.".into()),
            Event::User("again".into()),
            Event::Call {
                text: None,
                call: call("c3", "ls"),
                summary: Some("ls".into()),
            },
            Event::Result {
                id: "c3".into(),
                outcome: Outcome::Fail,
                content: "no".into(),
            },
            Event::Result {
                id: "c2".into(),
                outcome: Outcome::Ok,
                content: "here".into(),
            },
        ];
        let transitions = [
            line(1, 1, "Prefill", 100),
            line(1, 1, "Running", 50),
            line(1, 2, "Thinking", 30),
            line(2, 1, "Running", 7),
        ];
        let at_once = group(&events, &transitions, Lang::En);
        // The times first, the events one at a time.
        let mut fold = Fold::new(Lang::En);
        fold.times(&transitions);
        for event in &events {
            fold.event(event);
        }
        assert_eq!(fold.records(), at_once);
        // The events first: the times make the phases of what was folded.
        let mut late = Fold::new(Lang::En);
        for (n, event) in events.iter().enumerate() {
            late.event(event);
            // What is folded so far is a prefix of the whole.
            assert!(late.records().len() <= at_once.len(), "{n}");
        }
        late.times(&transitions);
        assert_eq!(late.records(), at_once);
        // The phases are there: the first step spent 100 ms reading.
        let Record::Tool { phases, time, .. } = &at_once[1] else {
            panic!("{:?}", at_once[1]);
        };
        assert_eq!(phases.len(), 2);
        assert_eq!(*time, Some(Duration::from_millis(50)));
    }
}
