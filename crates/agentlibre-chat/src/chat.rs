//! What the chat holds of its session (AGL-128): the records of the
//! surface's fold (AGL-127, D3), the REPL's, and what the turn is writing
//! now, its thinking and its text, until its event comes. It keeps too what
//! the row under the field says — the Function, the context, the phase and
//! the speed (AGL-139, D1) — and the row of phases under each answer (D6).

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use agentlibre_common::i18n::{Lang, m};
use agentlibre_common::style::Role;
use agentlibre_common::tr;
use agentlibre_proto::{
    Done, Event, Opened, Phase, PhaseEvent, PhaseTime, Report, Sent, StopReason, Told, Transition,
    TurnEnd,
};
use agentlibre_surface::records::{self, Fold, Record};
use agentlibre_surface::words;

/// What runs the session, for the row under the field (AGL-139, D1): as the
/// REPL's dock, the Function, `used / size (pct)`, the phase with its time
/// and the speed of what is being decoded.
#[derive(Clone, Debug)]
pub struct Dock {
    /// The Function of the session, from `Opened`.
    pub function: String,
    /// The server's context, from `Opened` and `Sent::Context` (AGL-015).
    pub context: Option<usize>,
    /// The tokens of the last prompt, from `Report::ContextUsed`.
    pub used: Option<usize>,
    /// The phase of the turn, from `Report::Transition`.
    pub phase: Phase,
    /// When the phase began: its time is what has gone since.
    since: Option<Instant>,
    /// When the recent pieces of what is being decoded arrived, within the
    /// second of the row's speed (D6), as the REPL's dock counts them.
    pieces: VecDeque<Instant>,
}

impl Default for Dock {
    fn default() -> Dock {
        Dock {
            function: String::new(),
            context: None,
            used: None,
            phase: Phase::Ready,
            since: None,
            pieces: VecDeque::new(),
        }
    }
}

impl Dock {
    /// The phase's label, `agentlibre-surface`'s; the rest is what the row
    /// says while nothing runs (AGL-139, D1).
    pub fn phase_label(&self, lang: Lang) -> String {
        let tool = match &self.phase {
            Phase::Running(tool) => tool.as_str(),
            _ => "",
        };
        match records::phase_label(self.phase.name(), tool, lang) {
            Some(label) => label,
            None => tr!(lang, m::READY).to_string(),
        }
    }

    /// How long the phase has lasted (AGL-139, D1).
    pub fn phase_time(&self) -> Option<Duration> {
        self.since.map(|since| since.elapsed())
    }

    /// `used / size (pct)` as the REPL's dock says it (AGL-139, D1): `—`
    /// while the server has not answered, `— / size` before the first
    /// prompt.
    pub fn usage(&self) -> String {
        let Some(size) = self.context.filter(|size| *size > 0) else {
            return "\u{2014}".into();
        };
        match self.used {
            None => format!("\u{2014} / {}", grouped(size)),
            Some(used) => {
                let percent = used.saturating_mul(100) / size.max(1);
                format!("{} / {} ({percent}%)", grouped(used), grouped(size))
            }
        }
    }

    /// The speed of what is being decoded: the pieces of the last second per
    /// its span, none before two of them have come (AGL-139, D6).
    pub fn speed(&self) -> Option<f64> {
        let (first, last) = (self.pieces.front()?, self.pieces.back()?);
        if self.pieces.len() < 2 {
            return None;
        }
        let span = last.duration_since(*first).as_secs_f64();
        (span > 0.0).then(|| self.pieces.len() as f64 / span)
    }

    /// A piece of the thinking or of the text arrived: the row's speed
    /// counts it, keeping only the last second of them (AGL-139, D6).
    fn piece(&mut self) {
        let now = Instant::now();
        self.pieces.push_back(now);
        while self.pieces.len() > 1 && now.duration_since(self.pieces[0]).as_secs_f64() > 1.0 {
            self.pieces.pop_front();
        }
    }
}

/// `n` with a no-break space between groups of three digits, as the REPL's
/// dock groups them (AGL-139, D1).
pub fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push('\u{a0}');
        }
        out.push(digit);
    }
    out
}

/// The line that says the server answered and what its context is (AGL-139,
/// D2); the catalog has none of its own.
fn server_answered(size: usize, lang: Lang) -> String {
    let size = grouped(size);
    match lang {
        Lang::Uk => format!("сервер відповів: контекст {size}"),
        Lang::En => format!("the server answered: {size} of context"),
    }
}

/// What a turn keeps for the row under its answer (AGL-139, D6).
#[derive(Clone, Debug, Default)]
struct Turn {
    /// When it began, from the human's message.
    started: Option<Instant>,
    /// Its phases and their times, in the order spent, summed by phase.
    phases: Vec<(String, Duration)>,
    /// Its reading and writing together, for the speed of its tokens.
    decoding: Duration,
    /// The tokens it decoded.
    tokens: usize,
}

impl Turn {
    /// A turn of the past, whose phases the session's file keeps but
    /// nothing of its tokens (AGL-139, D6).
    fn of_past(turn: usize, transitions: &[PhaseTime], lang: Lang) -> Turn {
        let mut phases: Vec<(String, Duration)> = Vec::new();
        for line in transitions {
            if line.turn != turn || line.from == "Ready" {
                continue;
            }
            let tool = line.tool.clone().unwrap_or_default();
            let Some(label) = records::phase_label(&line.from, &tool, lang) else {
                continue;
            };
            match phases.iter_mut().find(|(name, _)| *name == label) {
                Some((_, spent)) => *spent += Duration::from_millis(line.ms),
                None => phases.push((label, Duration::from_millis(line.ms))),
            }
        }
        Turn { phases, ..Turn::default() }
    }

    /// The time of what was decoded in it.
    fn speed(&self) -> Option<f64> {
        let seconds = self.decoding.as_secs_f64();
        (self.tokens > 0 && seconds > 0.0).then(|| self.tokens as f64 / seconds)
    }

    /// The items of the row under the answer (AGL-139, D6): its phases and
    /// their times, the time it took, its tokens and their speed. A turn of
    /// the past says only what its record keeps: the phases and their times.
    fn items(&self, lang: Lang) -> Vec<String> {
        let mut items: Vec<String> = self
            .phases
            .iter()
            .map(|(label, time)| format!("{label} {}", words::duration(time.as_secs())))
            .collect();
        if let Some(started) = self.started {
            let elapsed = started.elapsed();
            items.push(tr!(lang, m::WORKED_FOR, time = words::duration(elapsed.as_secs())).to_string());
            items.push(tr!(lang, m::TOKENS, n = grouped(self.tokens)).to_string());
            if let Some(speed) = self.speed() {
                items.push(words::speed(speed, lang));
            }
        }
        items
    }
}

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
    /// What runs the session, for the row under the field (AGL-139, D1).
    dock: Dock,
    /// The row under each answer, by the record's index (AGL-139, D6).
    rows: HashMap<usize, Vec<String>>,
    /// The turn in progress, for the row of its answer (AGL-139, D6).
    turn: Turn,
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
            dock: Dock::default(),
            rows: HashMap::new(),
            turn: Turn::default(),
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

    /// What runs the session, for the row under the field (AGL-139, D1).
    pub fn dock(&self) -> &Dock {
        &self.dock
    }

    /// The row under the answer of the record `at` (AGL-139, D6): its
    /// phases and their times, and of a turn of this window the time it
    /// took, its tokens and their speed.
    pub fn turn_row(&self, at: usize) -> Option<String> {
        self.rows
            .get(&at)
            .filter(|items| !items.is_empty())
            .map(|items| items.join(" \u{b7} "))
    }

    /// The session as it opened (AGL-139, D1): its Function and, when the
    /// server had answered already, its context.
    pub fn opened(&mut self, opened: &Opened) {
        self.dock.function = opened.function.clone();
        if self.dock.context.is_none() {
            self.dock.context = opened.context;
        }
    }

    /// A session's past, in place of what is held.
    pub fn history(&mut self, events: &[Event], transitions: &[PhaseTime]) {
        let mut fold = Fold::new(self.lang);
        fold.times(transitions);
        self.rows.clear();
        self.turn = Turn::default();
        self.dock.phase = Phase::Ready;
        self.dock.since = None;
        self.dock.pieces.clear();
        let mut turn = 0;
        for event in events {
            match event {
                Event::User(_) | Event::Wake { .. } => turn += 1,
                _ => {}
            }
            fold.event(event);
            // A turn read from the past shows what its record keeps (D6).
            if matches!(event, Event::Answer(_)) {
                let items = Turn::of_past(turn, transitions, self.lang).items(self.lang);
                if !items.is_empty() {
                    self.rows.insert(fold.records().len() - 1, items);
                }
            }
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
        self.turn = Turn { started: Some(Instant::now()), ..Turn::default() };
    }

    /// A line of the chat's own.
    pub fn note(&mut self, line: &str, role: Role) {
        self.thinking = None;
        self.writing = None;
        self.fold.note(line.to_owned(), role);
    }

    /// What the session's plane sent.
    pub fn sent(&mut self, sent: &Sent) {
        match sent {
            // A line of the server's start, as the REPL prints it (D2).
            Sent::Line(line) => self.note(line, Role::Muted),
            Sent::Context(size) => {
                self.dock.context = Some(*size);
                let line = server_answered(*size, self.lang);
                self.note(&line, Role::Muted);
            }
            Sent::Report(report) => match report {
                Report::Thinking(piece) => {
                    self.thinking_since.get_or_insert_with(Instant::now);
                    self.thinking.get_or_insert_with(String::new).push_str(piece);
                    self.dock.piece();
                }
                // The thinking's event is the session file's; what was written
                // goes to the fold, for the record of its step (D19).
                Report::ThinkingDone => {
                    self.thought = self.thinking_since.take().map(|since| since.elapsed());
                    if let Some(text) = self.thinking.take() {
                        self.fold.event(&Event::Thinking(text));
                    }
                }
                Report::ActionText(piece) => {
                    self.writing.get_or_insert_with(String::new).push_str(piece);
                    self.dock.piece();
                }
                Report::ContextUsed(tokens) => self.dock.used = Some(*tokens),
                Report::Decoded(tokens) => self.decoded(*tokens),
                Report::Transition(transition) => self.transition(transition),
                Report::Event(event) => {
                    if matches!(event, Event::Thinking(_)) {
                        self.thinking = None;
                    }
                    if matches!(event, Event::Call { .. } | Event::Answer(_)) {
                        self.writing = None;
                    }
                    if matches!(event, Event::User(_) | Event::Wake { .. }) {
                        self.turn = Turn { started: Some(Instant::now()), ..Turn::default() };
                    }
                    self.fold.event(event);
                    // The record of a step takes the time of its thinking.
                    if matches!(event, Event::Call { .. } | Event::Answer(_))
                        && let Some(time) = self.thought.take()
                    {
                        self.times.insert(self.fold.records().len() - 1, time);
                    }
                    // The answer ends the turn: its row is what the turn kept.
                    if matches!(event, Event::Answer(_)) {
                        let items = self.turn.items(self.lang);
                        if !items.is_empty() {
                            self.rows.insert(self.fold.records().len() - 1, items);
                        }
                        self.turn = Turn::default();
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// A phase's end and the next one's start (AGL-139, D1, D6): the row
    /// says the new phase from now, and the turn keeps the time of the old.
    fn transition(&mut self, transition: &Transition) {
        let lang = self.lang;
        self.dock.phase = transition.to.clone();
        self.dock.since = Some(Instant::now());
        // A generation ends with its phase; its speed goes with it.
        self.dock.pieces.clear();
        if transition.from != Phase::Ready {
            let tool = match &transition.from {
                Phase::Running(tool) => tool.as_str(),
                _ => "",
            };
            if let Some(label) = records::phase_label(transition.from.name(), tool, lang) {
                let time = Duration::from_micros(transition.micros);
                match self.turn.phases.iter_mut().find(|(name, _)| *name == label) {
                    Some((_, spent)) => *spent += time,
                    None => self.turn.phases.push((label, time)),
                }
                if matches!(transition.from, Phase::Thinking | Phase::Writing) {
                    self.turn.decoding += time;
                }
            }
        }
        if matches!(transition.event, PhaseEvent::StepStart) {
            self.thinking = None;
            self.writing = None;
        }
    }

    /// The tokens a step decoded (AGL-139, D6): the row under its answer
    /// divides them by the time of the thinking and of the writing.
    fn decoded(&mut self, tokens: usize) {
        self.turn.tokens += tokens;
    }

    /// A command's end: a turn's stop or its error.
    pub fn done(&mut self, done: &Done) {
        self.thinking = None;
        self.writing = None;
        match &done.turn {
            Some(Ok(TurnEnd::Stopped(StopReason::Interrupted))) => {
                self.note(tr!(self.lang, m::INTERRUPTED), Role::Muted);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn transition(from: Phase, to: Phase, micros: u64) -> Sent {
        Sent::Report(Report::Transition(Transition {
            from,
            to,
            event: PhaseEvent::StepStart,
            micros,
            violation: false,
        }))
    }

    /// The records of the chat that are its own lines.
    fn notes(chat: &Chat) -> Vec<String> {
        chat.records()
            .iter()
            .filter_map(|record| match record {
                Record::Notice { line, .. } => Some(line.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_row_groups_the_context_as_the_dock_does() {
        let mut chat = Chat::new(Lang::En);
        assert_eq!(chat.dock().usage(), "\u{2014}");
        chat.sent(&Sent::Context(131_072));
        assert_eq!(chat.dock().usage(), "\u{2014} / 131\u{a0}072");
        chat.sent(&Sent::Report(Report::ContextUsed(41_213)));
        assert_eq!(chat.dock().usage(), "41\u{a0}213 / 131\u{a0}072 (31%)");
    }

    #[test]
    fn the_server_s_start_comes_as_lines_of_the_conversation() {
        let mut chat = Chat::new(Lang::En);
        chat.sent(&Sent::Line("load: 42%".into()));
        chat.sent(&Sent::Context(4096));
        assert_eq!(notes(&chat), ["load: 42%", "the server answered: 4\u{a0}096 of context"]);
        // The row knows the size the server answered with.
        assert_eq!(chat.dock().context, Some(4096));
    }

    #[test]
    fn the_row_counts_the_pieces_of_the_last_second() {
        let mut chat = Chat::new(Lang::En);
        // One piece says nothing; two say what their span gives.
        chat.sent(&Sent::Report(Report::ActionText("a".into())));
        assert!(chat.dock().speed().is_none());
        chat.sent(&Sent::Report(Report::ActionText("b".into())));
        assert!(chat.dock().speed().is_some());
        // The phase's end clears the speed with the generation.
        chat.sent(&transition(Phase::Writing, Phase::Ready, 0));
        assert!(chat.dock().speed().is_none());
    }

    #[test]
    fn the_answer_keeps_its_phases_its_time_and_speed() {
        let mut chat = Chat::new(Lang::En);
        chat.said("hi");
        chat.sent(&transition(Phase::Ready, Phase::Thinking, 0));
        chat.sent(&Sent::Report(Report::Decoded(20)));
        chat.sent(&transition(Phase::Thinking, Phase::Writing, 2_000_000));
        chat.sent(&transition(Phase::Writing, Phase::Ready, 4_000_000));
        chat.sent(&Sent::Report(Report::Event(Event::Answer("done".into()))));
        // The record of the answer is the second: the message is the first.
        let row = chat.turn_row(1).unwrap_or_default();
        assert!(row.contains("Thinking 2s"), "{row}");
        assert!(row.contains("Writing 4s"), "{row}");
        assert!(row.contains("Worked for"), "{row}");
        assert!(row.contains("20 tokens"), "{row}");
        assert!(row.contains("tok/s"), "{row}");
        // The row under the field says the phase the last transition gave.
        assert_eq!(chat.dock().phase, Phase::Ready);
        assert!(chat.dock().speed().is_none());
    }

    #[test]
    fn a_turn_of_the_past_shows_what_its_record_keeps() {
        let mut chat = Chat::new(Lang::En);
        let events = [Event::User("hi".into()), Event::Answer("done".into())];
        let transitions = [PhaseTime {
            turn: 1,
            step: 1,
            from: "Thinking".into(),
            to: "Writing".into(),
            event: "first_text".into(),
            ms: 2_000,
            tokens: None,
            tool: None,
        }];
        chat.history(&events, &transitions);
        assert_eq!(chat.turn_row(1).unwrap_or_default(), "Thinking 2s");
    }
}
