//! What the chat holds of its session (AGL-128): the records of the
//! surface's fold (AGL-127, D3), the REPL's, and what the turn is writing
//! now, its thinking and its text, until its event comes.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use agentlibre_common::i18n::{Lang, m};
use agentlibre_common::tr;
use agentlibre_common::style::Role;
use agentlibre_proto::{Done, Event, PhaseEvent, PhaseTime, Report, Sent, StopReason, Told, TurnEnd};
use agentlibre_surface::records::{self, Fold, Record};

/// The session as the chat shows it.
pub struct Chat {
    fold: Fold,
    /// The thinking the step is writing, until its record comes.
    pub thinking: Option<String>,
    /// When the thinking being written began, and how long the last one took.
    thinking_since: Option<Instant>,
    thought: Option<Duration>,
    /// How long the thinking of a record took, by the record's index, when
    /// it was seen (AGL-128, D4).
    times: HashMap<usize, Duration>,
    /// The text the step is writing, until its record comes.
    pub writing: Option<String>,
    lang: Lang,
}

impl Chat {
    pub fn new(lang: Lang) -> Chat {
        Chat {
            fold: Fold::new(lang),
            thinking: None,
            thinking_since: None,
            thought: None,
            times: HashMap::new(),
            writing: None,
            lang,
        }
    }

    /// How long the thinking of the record `at` took, when it is known.
    pub fn thinking_time(&self, at: usize) -> Option<Duration> {
        self.times.get(&at).copied()
    }

    /// The records so far.
    pub fn records(&self) -> &[Record] {
        self.fold.records()
    }

    /// A session's past, in place of what is held.
    pub fn history(&mut self, events: &[Event], transitions: &[PhaseTime]) {
        let mut fold = Fold::new(self.lang);
        fold.times(transitions);
        for event in events {
            fold.event(event);
        }
        self.fold = fold;
        self.thinking = None;
        self.writing = None;
        self.times.clear();
    }

    /// The human's own message: the daemon tells it to the other clients
    /// only (AGL-126, D11).
    pub fn said(&mut self, text: &str) {
        self.fold.event(&Event::User(text.to_owned()));
    }

    /// A line of the chat's own.
    pub fn note(&mut self, line: &str, role: Role) {
        self.thinking = None;
        self.writing = None;
        self.fold.note(line.to_owned(), role);
    }

    /// What the session's plane sent.
    pub fn sent(&mut self, sent: &Sent) {
        let Sent::Report(report) = sent else {
            return;
        };
        match report {
            Report::Thinking(piece) => {
                self.thinking_since.get_or_insert_with(Instant::now);
                self.thinking.get_or_insert_with(String::new).push_str(piece);
            }
            // The thinking's event is the session file's; what was written
            // goes to the fold, for the record of its step (D19).
            Report::ThinkingDone => {
                self.thought = self.thinking_since.take().map(|since| since.elapsed());
                if let Some(text) = self.thinking.take() {
                    self.fold.event(&Event::Thinking(text));
                }
            }
            Report::ActionText(piece) => self.writing.get_or_insert_with(String::new).push_str(piece),
            Report::Event(event) => {
                if matches!(event, Event::Thinking(_)) {
                    self.thinking = None;
                }
                if matches!(event, Event::Call { .. } | Event::Answer(_)) {
                    self.writing = None;
                }
                self.fold.event(event);
                // The record of a step takes the time of its thinking.
                if matches!(event, Event::Call { .. } | Event::Answer(_))
                    && let Some(time) = self.thought.take()
                {
                    self.times.insert(self.fold.records().len() - 1, time);
                }
            }
            Report::Transition(transition) if matches!(transition.event, PhaseEvent::StepStart) => {
                self.thinking = None;
                self.writing = None;
            }
            _ => {}
        }
    }

    /// A command's end: a turn's stop or its error.
    pub fn done(&mut self, done: &Done) {
        self.thinking = None;
        self.writing = None;
        match &done.turn {
            Some(Ok(TurnEnd::Stopped(StopReason::Interrupted))) => {
                self.note(&tr!(self.lang, m::INTERRUPTED).to_string(), Role::Muted);
            }
            Some(Ok(TurnEnd::Stopped(_))) => {
                if let Some(why) = &done.why {
                    self.note(why, Role::StatusFailure);
                }
            }
            Some(Err(error)) => self.note(error, Role::StatusFailure),
            _ => {}
        }
        match &done.said {
            Some(Err(error)) => self.note(error, Role::StatusFailure),
            // What a slash command said, as the REPL shows it.
            Some(Ok(said)) => {
                for told in &said.lines {
                    let (text, role) = match told {
                        Told::Output(text) | Told::Notice(text) => (text, Role::Muted),
                        Told::Refusal(text) | Told::Fault(text) => (text, Role::StatusFailure),
                        Told::Pending(text) => (text, Role::StatusPending),
                    };
                    self.note(text, role);
                }
                if let Some(write) = &said.memory {
                    self.note(&records::memory_line(&write.name, write.kind, self.lang), Role::Muted);
                }
                if let Some(text) = &said.record_error {
                    self.note(text, Role::StatusFailure);
                }
            }
            None => {}
        }
    }
}

