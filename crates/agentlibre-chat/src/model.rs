//! The conversation as the chat shows it (AGL-073 D9): the rows a session's
//! reports, ends and past make. Plain data: the window draws it.

use agentlibre_proto::{
    Done, Event, PhaseEvent, Report, Sent, StopReason, ToolCall, TurnEnd, Value,
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
    /// A tool call, a dim line.
    Tool,
    /// A stop, a cut-off, an error: a dim line.
    Note,
}

/// A row of the conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub who: Who,
    pub text: String,
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
                    conversation.push(Who::Tool, call_line(call));
                }
                Event::Stop(reason) if *reason != StopReason::Interrupted => {
                    conversation.push(Who::Note, format!("stopped: {}", reason_name(*reason)));
                }
                Event::CutOff(_) => conversation.push(Who::Note, CUT_OFF.into()),
                _ => {}
            }
        }
        conversation
    }

    fn push(&mut self, who: Who, text: String) {
        self.rows.push(Row { who, text });
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
                self.push(Who::Tool, call_line(call));
            }
            Report::Event(Event::Answer(text)) => match self.streaming.take() {
                Some(at) => self.rows[at].text = text.clone(),
                None => self.push(Who::Agent, text.clone()),
            },
            Report::Event(Event::CutOff(_)) => self.note(CUT_OFF),
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

    fn rows(conversation: &Conversation) -> Vec<(Who, &str)> {
        conversation
            .rows
            .iter()
            .map(|row| (row.who, row.text.as_str()))
            .collect()
    }

    #[test]
    fn a_turn_is_its_text_its_calls_and_its_answer_as_they_arrive() {
        let mut conversation = Conversation::default();
        conversation.said("read it");
        conversation.sent(&step());
        conversation.sent(&text("Let me "));
        conversation.sent(&text("look."));
        assert_eq!(rows(&conversation)[1], (Who::Agent, "Let me look."));
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
        assert_eq!(
            rows(&conversation),
            [
                (Who::You, "read it"),
                (Who::Agent, "Let me look."),
                (Who::Tool, "→ read src/main.rs"),
                (Who::Tool, "→ bash ls"),
                (Who::Agent, "It is done."),
            ]
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
        assert_eq!(
            rows(&conversation),
            [
                (Who::Note, "stopped"),
                (Who::Note, "stopped: step_limit"),
                (Who::Note, "no server"),
            ]
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
        assert_eq!(
            rows(&conversation),
            [
                (Who::You, "hi"),
                (Who::Agent, "Looking."),
                (Who::Tool, "→ read a"),
                (Who::Agent, "hello"),
                (Who::You, "again"),
                (Who::Note, "stopped: context_full"),
            ]
        );
    }
}
