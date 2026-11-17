//! The conversation as the chat shows it (AGL-073 D9): the rows a session's
//! reports, ends and past make. Plain data: the window draws it.

use agentlibre_proto::{
    Done, Event, Outcome, PhaseEvent, Report, Sent, StopReason, ToolCall, TurnEnd, Value,
};

/// The most of a tool's argument its line shows.
const ARGUMENT_CHARS: usize = 80;

/// Who a row is of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Who {
    /// The human's message.
    You,
    /// The agent's text: its answer, or what it wrote before a call.
    Agent,
    /// The calls between two of the agent's texts, folded (KS-042 D1).
    Steps,
    /// A stop, a cut-off, an error: a dim line.
    Note,
}

/// A row of the conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub who: Who,
    pub text: String,
    /// A row of steps' calls.
    pub steps: Vec<Step>,
}

/// A call of the agent's: its line, and its output once it came.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    id: String,
    pub line: String,
    /// The output's first lines (KS-042 D2), and whether the call failed.
    pub output: Option<String>,
    pub failed: bool,
}

/// The most lines of an output a step shows (KS-042 D2).
const OUTPUT_LINES: usize = 20;

/// An output's first [`OUTPUT_LINES`], then how many more there are.
fn output(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= OUTPUT_LINES {
        return content.trim_end().to_owned();
    }
    format!(
        "{}\n… {} more lines",
        lines[..OUTPUT_LINES].join("\n"),
        lines.len() - OUTPUT_LINES
    )
}

impl Row {
    /// A fold's header: the number of calls and the last one's line.
    pub fn header(&self) -> String {
        let count = self.steps.len();
        let last = self.steps.last().map_or("", |step| step.line.as_str());
        let calls = if count == 1 { "call" } else { "calls" };
        format!("{count} {calls} · {last}")
    }
}

/// The rows, and the agent's text that is arriving.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Conversation {
    pub rows: Vec<Row>,
    /// The row the arriving text goes to.
    streaming: Option<usize>,
}

impl Conversation {
    /// A session's past (D10): its messages, answers, calls and stops.
    pub fn of_history(events: &[Event]) -> Conversation {
        let mut conversation = Conversation::default();
        for event in events {
            match event {
                Event::User(text) => conversation.push(Who::You, text.clone()),
                Event::Answer(text) => conversation.push(Who::Agent, text.clone()),
                Event::Call { text, call } => {
                    if let Some(text) = text.as_ref().filter(|text| !text.trim().is_empty()) {
                        conversation.push(Who::Agent, text.clone());
                    }
                    conversation.call(call);
                }
                Event::Stop(reason) if *reason != StopReason::Interrupted => {
                    conversation.push(Who::Note, format!("stopped: {}", reason_name(*reason)));
                }
                Event::CutOff(_) => conversation.push(Who::Note, CUT_OFF.into()),
                Event::Result {
                    id,
                    outcome,
                    content,
                } => conversation.result(id, *outcome, content),
                _ => {}
            }
        }
        conversation
    }

    fn push(&mut self, who: Who, text: String) {
        self.rows.push(Row {
            who,
            text,
            steps: Vec::new(),
        });
    }

    /// A call: a step of the last row of steps, or of a new one after the
    /// agent's text (D1).
    fn call(&mut self, call: &ToolCall) {
        if self.rows.last().is_none_or(|row| row.who != Who::Steps) {
            self.push(Who::Steps, String::new());
        }
        let row = self.rows.last_mut().expect("a row of steps");
        row.steps.push(Step {
            id: call.id.clone(),
            line: call_line(call),
            output: None,
            failed: false,
        });
    }

    /// A call's output, on its step.
    fn result(&mut self, id: &str, outcome: Outcome, content: &str) {
        let step = self
            .rows
            .iter_mut()
            .rev()
            .filter(|row| row.who == Who::Steps)
            .flat_map(|row| row.steps.iter_mut())
            .find(|step| step.id == id);
        if let Some(step) = step {
            step.output = Some(output(content));
            step.failed = outcome != Outcome::Ok;
        }
    }

    /// The human's message, shown as it is sent.
    pub fn said(&mut self, text: &str) {
        self.streaming = None;
        self.push(Who::You, text.to_owned());
    }

    /// A line of the chat's own: a refusal, an error.
    pub fn note(&mut self, text: &str) {
        self.streaming = None;
        self.push(Who::Note, text.to_owned());
    }

    /// What the session's plane sent.
    pub fn sent(&mut self, sent: &Sent) {
        let Sent::Report(report) = sent else {
            return;
        };
        match report {
            Report::ActionText(piece) => {
                let at = match self.streaming {
                    Some(at) => at,
                    None => {
                        self.push(Who::Agent, String::new());
                        let at = self.rows.len() - 1;
                        self.streaming = Some(at);
                        at
                    }
                };
                self.rows[at].text.push_str(piece);
            }
            Report::Event(Event::Call { text, call }) => {
                // What was written before the call is its text.
                match (self.streaming.take(), text) {
                    (Some(at), Some(text)) => self.rows[at].text = text.clone(),
                    (Some(at), None) if self.rows[at].text.trim().is_empty() => {
                        self.rows.remove(at);
                    }
                    (None, Some(text)) if !text.trim().is_empty() => {
                        self.push(Who::Agent, text.clone());
                    }
                    _ => {}
                }
                self.call(call);
            }
            Report::Event(Event::Answer(text)) => match self.streaming.take() {
                Some(at) => self.rows[at].text = text.clone(),
                None => self.push(Who::Agent, text.clone()),
            },
            Report::Event(Event::CutOff(_)) => self.note(CUT_OFF),
            Report::Event(Event::Result {
                id,
                outcome,
                content,
            }) => {
                self.result(id, *outcome, content);
            }
            // A new step's text is a row of its own.
            Report::Transition(transition) if matches!(transition.event, PhaseEvent::StepStart) => {
                self.streaming = None;
            }
            _ => {}
        }
    }

    /// A command's end: a turn's stop or error.
    pub fn done(&mut self, done: &Done) {
        self.streaming = None;
        match &done.turn {
            Some(Ok(TurnEnd::Stopped(StopReason::Interrupted))) => self.note("stopped"),
            Some(Ok(TurnEnd::Stopped(reason))) => {
                let why = done
                    .why
                    .clone()
                    .unwrap_or_else(|| format!("stopped: {}", reason_name(*reason)));
                self.note(&why);
            }
            Some(Err(error)) => self.note(error),
            _ => {}
        }
        if let Some(Err(error)) = &done.said {
            self.note(error);
        }
    }
}

const CUT_OFF: &str = "a generation was cut off at its token budget";

/// A stop's reason as the session's file names it (`step_limit`…).
fn reason_name(reason: StopReason) -> String {
    match reason {
        StopReason::StepLimit => "step_limit",
        StopReason::ContextFull => "context_full",
        StopReason::ServerError => "server_error",
        StopReason::Interrupted => "interrupted",
        StopReason::StirLimit => "stir_limit",
    }
    .to_owned()
}

/// A call's line: its tool and the first line of its first text argument.
fn call_line(call: &ToolCall) -> String {
    let argument = call
        .args
        .iter()
        .find_map(|(_, value)| match value {
            Value::Text(text) => Some(text.lines().next().unwrap_or_default().trim().to_owned()),
            Value::Int(_) => None,
        })
        .unwrap_or_default();
    let argument = if argument.chars().count() > ARGUMENT_CHARS {
        let kept: String = argument.chars().take(ARGUMENT_CHARS - 1).collect();
        format!("{}…", kept.trim_end())
    } else {
        argument
    };
    format!("→ {} {argument}", call.name).trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use agentlibre_proto::{Phase, SessionView, Transition};

    use super::*;

    fn call(name: &str, path: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            args: vec![("path".into(), Value::Text(path.into()))],
        }
    }

    fn step() -> Sent {
        Sent::Report(Report::Transition(Transition {
            from: Phase::Ready,
            to: Phase::Prefill,
            event: PhaseEvent::StepStart,
            micros: 0,
            violation: false,
        }))
    }

    fn text(piece: &str) -> Sent {
        Sent::Report(Report::ActionText(piece.into()))
    }

    fn rows(conversation: &Conversation) -> Vec<(Who, String)> {
        conversation
            .rows
            .iter()
            .map(|row| match row.who {
                Who::Steps => (row.who, row.header()),
                _ => (row.who, row.text.clone()),
            })
            .collect()
    }

    fn check(conversation: &Conversation, expected: &[(Who, &str)]) {
        let rows = rows(conversation);
        let rows: Vec<(Who, &str)> = rows
            .iter()
            .map(|(who, text)| (*who, text.as_str()))
            .collect();
        assert_eq!(rows, expected);
    }

    #[test]
    fn a_calls_output_is_on_its_step_cut_to_twenty_lines() {
        let mut conversation = Conversation::default();
        conversation.sent(&Sent::Report(Report::Event(Event::Call {
            text: None,
            call: call("read", "big"),
        })));
        let long: Vec<String> = (1..=25).map(|line| line.to_string()).collect();
        conversation.sent(&Sent::Report(Report::Event(Event::Result {
            id: "c1".into(),
            outcome: Outcome::Ok,
            content: long.join("\n"),
        })));
        let step = &conversation.rows[0].steps[0];
        let output = step.output.as_deref().unwrap();
        assert!(
            output.starts_with("1\n2\n") && output.ends_with("20\n… 5 more lines"),
            "{output}"
        );
        assert!(!step.failed);
        conversation.sent(&Sent::Report(Report::Event(Event::Result {
            id: "c1".into(),
            outcome: Outcome::Fail,
            content: "no such file".into(),
        })));
        let step = &conversation.rows[0].steps[0];
        assert_eq!(
            (step.output.as_deref(), step.failed),
            (Some("no such file"), true)
        );
    }

    #[test]
    fn a_turn_is_its_text_its_calls_and_its_answer_as_they_arrive() {
        let mut conversation = Conversation::default();
        conversation.said("read it");
        conversation.sent(&step());
        conversation.sent(&text("Let me "));
        conversation.sent(&text("look."));
        assert_eq!(
            rows(&conversation)[1],
            (Who::Agent, "Let me look.".to_owned())
        );
        conversation.sent(&Sent::Report(Report::Event(Event::Call {
            text: Some("Let me look.".into()),
            call: call("read", "src/main.rs\nmore"),
        })));
        conversation.sent(&step());
        // A call with no text before it leaves no empty row.
        conversation.sent(&Sent::Report(Report::Event(Event::Call {
            text: None,
            call: call("bash", "ls"),
        })));
        conversation.sent(&step());
        conversation.sent(&text("It is"));
        conversation.sent(&Sent::Report(Report::Event(Event::Answer(
            "It is done.".into(),
        ))));
        check(
            &conversation,
            &[
                (Who::You, "read it"),
                (Who::Agent, "Let me look."),
                (Who::Steps, "2 calls · → bash ls"),
                (Who::Agent, "It is done."),
            ],
        );
    }

    #[test]
    fn a_stop_and_an_error_are_notes() {
        let mut conversation = Conversation::default();
        let done = |turn| Done {
            turn: Some(turn),
            why: None,
            said: None,
            session: SessionView::default(),
        };
        conversation.done(&done(Ok(TurnEnd::Stopped(StopReason::Interrupted))));
        conversation.done(&done(Ok(TurnEnd::Stopped(StopReason::StepLimit))));
        conversation.done(&done(Err("no server".into())));
        conversation.done(&done(Ok(TurnEnd::Answer("x".into()))));
        check(
            &conversation,
            &[
                (Who::Note, "stopped"),
                (Who::Note, "stopped: step_limit"),
                (Who::Note, "no server"),
            ],
        );
    }

    #[test]
    fn the_past_is_the_messages_the_answers_and_the_calls() {
        let conversation = Conversation::of_history(&[
            Event::State("start".into()),
            Event::User("hi".into()),
            Event::Call {
                text: Some("Looking.".into()),
                call: call("read", "a"),
            },
            Event::Answer("hello".into()),
            Event::Stop(StopReason::Interrupted),
            Event::User("again".into()),
            Event::Stop(StopReason::ContextFull),
        ]);
        check(
            &conversation,
            &[
                (Who::You, "hi"),
                (Who::Agent, "Looking."),
                (Who::Steps, "1 call · → read a"),
                (Who::Agent, "hello"),
                (Who::You, "again"),
                (Who::Note, "stopped: context_full"),
            ],
        );
    }
}
