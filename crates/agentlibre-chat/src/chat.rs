//! What the chat holds of its session (AGL-128): the records of the
//! surface's fold (AGL-127, D3), the REPL's, and what the turn is writing
//! now, its thinking and its text, until its event comes.

use agentlibre_common::i18n::{Lang, m};
use agentlibre_common::tr;
use agentlibre_common::style::Role;
use agentlibre_proto::{Done, Event, PhaseEvent, PhaseTime, Report, Sent, StopReason, TurnEnd};
use agentlibre_surface::records::{Fold, Record};

/// The session as the chat shows it.
pub struct Chat {
    fold: Fold,
    /// The thinking the step is writing, until its record comes.
    pub thinking: Option<String>,
    /// The text the step is writing, until its record comes.
    pub writing: Option<String>,
    lang: Lang,
}

impl Chat {
    pub fn new(lang: Lang) -> Chat {
        Chat {
            fold: Fold::new(lang),
            thinking: None,
            writing: None,
            lang,
        }
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
            Report::Thinking(piece) => self.thinking.get_or_insert_with(String::new).push_str(piece),
            Report::ActionText(piece) => self.writing.get_or_insert_with(String::new).push_str(piece),
            Report::Event(event) => {
                if matches!(event, Event::Thinking(_)) {
                    self.thinking = None;
                }
                if matches!(event, Event::Call { .. } | Event::Answer(_)) {
                    self.writing = None;
                }
                self.fold.event(event);
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
        if let Some(Err(error)) = &done.said {
            self.note(error, Role::StatusFailure);
        }
    }
}

