//! A run's session as what the run sent tells it (AGL-126, D10): its events,
//! each once, and what the turn that runs is writing. Who holds it needs no
//! command of the run, which takes them one after another.

use agentlibre_proto::Writing;

use super::Sent;
use crate::agent::Report;
use crate::session::Event;

#[derive(Clone, Debug, Default)]
pub struct Past {
    pub events: Vec<Event>,
    pub writing: Writing,
}

impl Past {
    /// The past of a run whose session holds `events`, before it is started.
    pub fn of(events: &[Event]) -> Past {
        Past {
            events: events.to_vec(),
            writing: Writing::default(),
        }
    }

    /// Takes what the run sent, in its order.
    pub fn take(&mut self, sent: &Sent) {
        match sent {
            Sent::Kept(event) => {
                match event {
                    // The step's thinking is in the record now.
                    Event::Thinking(_) => self.writing.thinking.clear(),
                    Event::Stop(_) | Event::Stir { .. } => self.writing = Writing::default(),
                    _ => {}
                }
                self.events.push(event.clone());
            }
            Sent::Report(report) => self.told(report),
            // The session as its run holds it.
            Sent::Done(done) => *self = Past::of(&done.session.events),
            _ => {}
        }
    }

    fn told(&mut self, report: &Report) {
        match report {
            Report::Event(event) => {
                if matches!(event, Event::Call { .. } | Event::Answer(_) | Event::CutOff(_)) {
                    self.writing = Writing::default();
                }
                self.events.push(event.clone());
            }
            Report::Thinking(piece) => self.writing.thinking.push_str(piece),
            Report::ActionText(piece) => self.writing.text.push_str(piece),
            // A step, or the summary of a compaction, begins anew.
            Report::Prefill | Report::Compacting => self.writing = Writing::default(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_turn_writes_is_held_until_an_event_holds_it() {
        let mut past = Past::default();
        let report = |past: &mut Past, report| past.take(&Sent::Report(report));
        report(&mut past, Report::Prefill);
        report(&mut past, Report::Thinking("let me ".into()));
        report(&mut past, Report::Thinking("see".into()));
        report(&mut past, Report::ActionText("An ans".into()));
        assert_eq!((past.writing.thinking.as_str(), past.writing.text.as_str()), ("let me see", "An ans"));
        // The step's thinking goes to the record before its action.
        past.take(&Sent::Kept(Event::Thinking("let me see".into())));
        assert_eq!((past.writing.thinking.as_str(), past.writing.text.as_str()), ("", "An ans"));
        report(&mut past, Report::Event(Event::Answer("An answer".into())));
        assert!(past.writing.is_empty());
        assert_eq!(past.events, [Event::Thinking("let me see".into()), Event::Answer("An answer".into())]);
        // A stop drops what was not finished.
        report(&mut past, Report::ActionText("Ano".into()));
        past.take(&Sent::Kept(Event::Stop(crate::session::StopReason::Interrupted)));
        assert!(past.writing.is_empty());
    }
}
