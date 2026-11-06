//! The FSM during a turn (AGL-046): what happens, in which order, and how
//! long it took. It observes and does not steer: nothing here reaches the
//! prompt, and a transition that must not happen does not stop the turn.
//!
//! Every transition goes through [`Observer::on`], which appends a line to
//! the session file. Those lines are not events of the session, so the
//! history the model reads cannot hold them. [`Observed`] puts the observer
//! between the agent and its [`Report`], so all modes share it.

use std::fs::File;
use std::io::Write;
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};

use crate::action::Outcome;
use crate::agent::Report;
use crate::backend::{Prefill, PromptProgress, ToolCall};
use crate::memory::Kind as MemoryKind;
use crate::session::{CutOff, ForgeKind, Session, StopReason, utc_now};
use crate::tools::ToolResult;

/// The version of the lines this module writes.
const SCHEMA: u32 = 1;

/// What is happening during a turn (AGL-046, D10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// No turn is running.
    Ready,
    StartingServer,
    Transcribing,
    /// A step's prompt is built and read by the server, until its first
    /// token.
    Prefill,
    Thinking,
    Writing,
    Running,
    Compacting,
    /// The stirred mode tries the step again.
    Retry,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Phase::Ready => "Ready",
            Phase::StartingServer => "StartingServer",
            Phase::Transcribing => "Transcribing",
            Phase::Prefill => "Prefill",
            Phase::Thinking => "Thinking",
            Phase::Writing => "Writing",
            Phase::Running => "Running",
            Phase::Compacting => "Compacting",
            Phase::Retry => "Retry",
        }
    }
}

/// What moves a turn from one phase to another.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhaseEvent {
    ServerStart,
    Transcribe,
    StepStart,
    FirstThinking,
    FirstText,
    ToolStart {
        tool: String,
    },
    ToolEnd(Outcome),
    CompactStart,
    Compacted,
    CompactFailed,
    CutOff,
    Stir {
        attempt: u32,
        max: u32,
    },
    Answer,
    Stop(StopReason),
    /// The turn ended with an error.
    Failed,
    /// Work outside a turn is over, as after `/compact`.
    Idle,
}

impl PhaseEvent {
    /// The name in the session file.
    pub fn name(&self) -> String {
        match self {
            PhaseEvent::ServerStart => "server_start".into(),
            PhaseEvent::Transcribe => "transcribe".into(),
            PhaseEvent::StepStart => "step_start".into(),
            PhaseEvent::FirstThinking => "first_thinking".into(),
            PhaseEvent::FirstText => "first_text".into(),
            PhaseEvent::ToolStart { .. } => "tool_start".into(),
            PhaseEvent::ToolEnd(outcome) => format!("tool_{}", outcome.name()),
            PhaseEvent::CompactStart => "compact_start".into(),
            PhaseEvent::Compacted => "compacted".into(),
            PhaseEvent::CompactFailed => "compact_failed".into(),
            PhaseEvent::CutOff => "cut_off".into(),
            PhaseEvent::Stir { .. } => "stir".into(),
            PhaseEvent::Answer => "answer".into(),
            PhaseEvent::Stop(reason) => format!("stop_{}", reason.name()),
            PhaseEvent::Failed => "failed".into(),
            PhaseEvent::Idle => "idle".into(),
        }
    }

    /// The phase the event leads to.
    fn target(&self) -> Phase {
        match self {
            PhaseEvent::ServerStart => Phase::StartingServer,
            PhaseEvent::Transcribe => Phase::Transcribing,
            PhaseEvent::StepStart
            | PhaseEvent::ToolEnd(_)
            | PhaseEvent::Compacted
            | PhaseEvent::CompactFailed
            | PhaseEvent::CutOff => Phase::Prefill,
            PhaseEvent::FirstThinking => Phase::Thinking,
            PhaseEvent::FirstText => Phase::Writing,
            PhaseEvent::ToolStart { .. } => Phase::Running,
            PhaseEvent::CompactStart => Phase::Compacting,
            PhaseEvent::Stir { .. } => Phase::Retry,
            PhaseEvent::Answer | PhaseEvent::Stop(_) | PhaseEvent::Failed | PhaseEvent::Idle => {
                Phase::Ready
            }
        }
    }
}

/// A transition that must not happen (AGL-046, D11).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    pub from: Phase,
    pub event: PhaseEvent,
}

/// The table of the FSM: the phase `event` leads to from `from`.
pub fn next(from: Phase, event: &PhaseEvent) -> Result<Phase, Violation> {
    use Phase::*;
    let generating = matches!(from, Prefill | Thinking | Writing | Retry);
    let legal = match event {
        PhaseEvent::ServerStart => matches!(from, Ready | StartingServer | Transcribing),
        PhaseEvent::Transcribe => matches!(from, Ready | StartingServer | Transcribing),
        PhaseEvent::StepStart => matches!(from, Ready | StartingServer | Transcribing | Prefill),
        PhaseEvent::FirstThinking => matches!(from, Prefill | Retry | Thinking),
        PhaseEvent::FirstText => matches!(from, Prefill | Thinking | Retry | Writing),
        PhaseEvent::ToolStart { .. }
        | PhaseEvent::CutOff
        | PhaseEvent::Stir { .. }
        | PhaseEvent::Answer => generating,
        PhaseEvent::ToolEnd(_) => from == Running,
        PhaseEvent::CompactStart => matches!(from, Ready | Prefill),
        PhaseEvent::Compacted | PhaseEvent::CompactFailed => from == Compacting,
        PhaseEvent::Stop(_) | PhaseEvent::Failed | PhaseEvent::Idle => true,
    };
    if legal {
        Ok(event.target())
    } else {
        Err(Violation {
            from,
            event: event.clone(),
        })
    }
}

/// One transition, as the session file and the report get it.
#[derive(Clone, Debug, PartialEq)]
pub struct Transition {
    pub from: Phase,
    pub to: Phase,
    pub event: PhaseEvent,
    /// The time spent in `from`.
    pub time: Duration,
    /// Whether the table does not allow it (AGL-046, D11).
    pub violation: bool,
}

/// What a step's prompt was made of (AGL-046, D15).
#[derive(Clone, Debug)]
pub struct StepRecord<'a> {
    pub state: &'a str,
    pub prompt_sha256: String,
    pub prompt_tokens: usize,
    pub think: bool,
    pub think_budget: usize,
    pub action_budget: u32,
    pub context: usize,
    pub stirred: bool,
    pub reasoning: String,
}

/// The FSM of one turn and where its lines go.
pub struct Observer {
    /// The session file; the lines wait in `pending` until it is known, as
    /// `-p` starts the server before it has a session.
    file: Option<File>,
    pending: Vec<Json>,
    turn: usize,
    step: usize,
    phase: Phase,
    since: Instant,
    /// The tokens decoded and the prompt's size told since the last
    /// transition.
    tokens: usize,
    prompt_tokens: Option<usize>,
    /// A violation fails the tests (AGL-046, D11).
    strict: bool,
}

impl Default for Observer {
    fn default() -> Observer {
        Observer {
            file: None,
            pending: Vec::new(),
            turn: 0,
            step: 0,
            phase: Phase::Ready,
            since: Instant::now(),
            tokens: 0,
            prompt_tokens: None,
            strict: cfg!(test),
        }
    }
}

impl Observer {
    pub fn new() -> Observer {
        Observer::default()
    }

    /// The lines go to `session`, for the turn that is about to start.
    pub fn attach(&mut self, session: &Session) {
        self.turn = session.turns() + 1;
        self.attach_file(session);
    }

    /// The lines go to `session`, for work between its turns.
    pub fn attach_between_turns(&mut self, session: &Session) {
        self.turn = session.turns();
        self.attach_file(session);
    }

    fn attach_file(&mut self, session: &Session) {
        self.file = session.log_file();
        for mut line in std::mem::take(&mut self.pending) {
            line["turn"] = self.turn.into();
            self.write(line);
        }
    }

    /// A failed write is not reported here: the session file is the one the
    /// agent appends its events to, and that write stops the turn.
    fn write(&mut self, line: Json) {
        match &mut self.file {
            // One write: `Display` of a value alone would make one per piece.
            Some(file) => {
                let text = format!("{line}\n");
                let _ = file.write_all(text.as_bytes()).and_then(|()| file.flush());
            }
            None => self.pending.push(line),
        }
    }

    /// Takes `event`. `None` when the phase goes on: a phase entered again
    /// is the same phase.
    pub fn on(&mut self, event: PhaseEvent) -> Option<Transition> {
        let from = self.phase;
        let (to, violation) = match next(from, &event) {
            Ok(to) => (to, false),
            Err(violation) => {
                assert!(!self.strict, "{violation:?}");
                (event.target(), true)
            }
        };
        if to == from && !violation && !matches!(event, PhaseEvent::Stir { .. }) {
            return None;
        }
        let transition = Transition {
            from,
            to,
            event,
            time: self.since.elapsed(),
            violation,
        };
        self.since = Instant::now();
        self.phase = to;
        self.write_transition(&transition);
        Some(transition)
    }

    fn write_transition(&mut self, transition: &Transition) {
        let mut line = json!({
            "type": "transition",
            "schema": SCHEMA,
            "turn": self.turn,
            "step": self.step,
            "from": transition.from.name(),
            "to": transition.to.name(),
            "event": transition.event.name(),
            "ms": transition.time.as_millis() as u64,
            "time": utc_now(),
        });
        if self.tokens > 0 {
            line["tokens"] = std::mem::take(&mut self.tokens).into();
        }
        if let Some(tokens) = self.prompt_tokens.take() {
            line["prompt_tokens"] = tokens.into();
        }
        match &transition.event {
            PhaseEvent::ToolStart { tool } => line["tool"] = tool.as_str().into(),
            PhaseEvent::Stir { attempt, max } => {
                line["attempt"] = (*attempt).into();
                line["max"] = (*max).into();
            }
            _ => {}
        }
        if transition.violation {
            line["violation"] = true.into();
        }
        self.write(line);
    }

    fn write_step(&mut self, record: &StepRecord) {
        let line = json!({
            "type": "step",
            "schema": SCHEMA,
            "turn": self.turn,
            "step": self.step,
            "state": record.state,
            "prompt_sha256": record.prompt_sha256,
            "prompt_tokens": record.prompt_tokens,
            "think": record.think,
            "think_budget": record.think_budget,
            "action_budget": record.action_budget,
            "context": record.context,
            "stirred": record.stirred,
            "reasoning": record.reasoning,
            "time": utc_now(),
        });
        self.write(line);
    }

    fn write_progress(&mut self, progress: PromptProgress) {
        let line = json!({
            "type": "prompt_progress",
            "schema": SCHEMA,
            "turn": self.turn,
            "step": self.step,
            "total": progress.total,
            "cache": progress.cache,
            "processed": progress.processed,
            "time_ms": progress.time_ms,
            "time": utc_now(),
        });
        self.write(line);
    }

    fn write_prefill(&mut self, prefill: Prefill) {
        let line = json!({
            "type": "prefill",
            "schema": SCHEMA,
            "turn": self.turn,
            "step": self.step,
            "tokens": prefill.tokens,
            "ms": prefill.ms,
            "tokens_per_second": (prefill.tokens as f64 * 1000.0 / prefill.ms).round(),
            "time": utc_now(),
        });
        self.write(line);
    }

    /// Takes `event` and tells `report` of the transition.
    pub fn enter(&mut self, event: PhaseEvent, report: &mut dyn Report) {
        if let Some(transition) = self.on(event) {
            report.transition(&transition);
        }
    }

    /// Back to `Ready` unless the turn already is: every way out of a turn
    /// ends in the file.
    pub fn end(&mut self, event: PhaseEvent, report: &mut dyn Report) {
        if self.phase != Phase::Ready {
            self.enter(event, report);
        }
    }
}

/// A [`Report`] that takes the transitions from what the agent reports and
/// passes everything on.
pub struct Observed<'a> {
    pub observer: &'a mut Observer,
    pub report: &'a mut dyn Report,
}

impl Observed<'_> {
    fn enter(&mut self, event: PhaseEvent) {
        self.observer.enter(event, &mut *self.report);
    }

    /// See [`Observer::end`].
    pub fn end(&mut self, event: PhaseEvent) {
        self.observer.end(event, &mut *self.report);
    }
}

impl Report for Observed<'_> {
    fn state(&mut self, state: &str) {
        self.report.state(state);
    }
    fn thinking(&mut self, piece: &str) {
        self.enter(PhaseEvent::FirstThinking);
        self.report.thinking(piece);
    }
    fn thinking_done(&mut self) {
        self.report.thinking_done();
    }
    fn action_started(&mut self) {
        self.enter(PhaseEvent::FirstText);
        self.report.action_started();
    }
    fn action_text(&mut self, piece: &str) {
        self.enter(PhaseEvent::FirstText);
        self.report.action_text(piece);
    }
    fn text(&mut self, text: &str) {
        self.report.text(text);
    }
    fn answer(&mut self, text: &str) {
        self.enter(PhaseEvent::Answer);
        self.report.answer(text);
    }
    fn call(&mut self, call: &ToolCall) {
        self.enter(PhaseEvent::ToolStart {
            tool: call.name.clone(),
        });
        self.report.call(call);
    }
    fn result(&mut self, call: &ToolCall, result: &ToolResult) {
        self.enter(PhaseEvent::ToolEnd(result.outcome));
        self.report.result(call, result);
    }
    fn forge(&mut self, name: &str, kind: ForgeKind, section: Option<&str>) {
        self.report.forge(name, kind, section);
    }
    fn memory(&mut self, name: &str, kind: MemoryKind, text: Option<&str>) {
        self.report.memory(name, kind, text);
    }
    fn compacting(&mut self) {
        self.enter(PhaseEvent::CompactStart);
        self.report.compacting();
    }
    fn compacted(&mut self) {
        self.enter(PhaseEvent::Compacted);
        self.report.compacted();
    }
    fn compact_failed(&mut self) {
        self.enter(PhaseEvent::CompactFailed);
    }
    fn cut_off(&mut self, cut: &CutOff) {
        self.enter(PhaseEvent::CutOff);
        self.report.cut_off(cut);
    }
    fn prefill(&mut self) {
        self.observer.step += 1;
        self.enter(PhaseEvent::StepStart);
        self.report.prefill();
    }
    fn context_used(&mut self, tokens: usize) {
        self.observer.prompt_tokens = Some(tokens);
        self.report.context_used(tokens);
    }
    fn decoded(&mut self, tokens: usize) {
        self.observer.tokens += tokens;
        self.report.decoded(tokens);
    }
    fn stirred(&mut self, attempt: u32, max: u32, error: &str) {
        self.enter(PhaseEvent::Stir { attempt, max });
        self.report.stirred(attempt, max, error);
    }
    fn prompt_progress(&mut self, progress: PromptProgress) {
        self.observer.write_progress(progress);
        self.report.prompt_progress(progress);
    }
    fn prompt_read(&mut self, prefill: Prefill) {
        self.observer.write_prefill(prefill);
        self.report.prompt_read(prefill);
    }
    fn step_started(&mut self, record: &StepRecord) {
        self.observer.write_step(record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Phase; 9] = [
        Phase::Ready,
        Phase::StartingServer,
        Phase::Transcribing,
        Phase::Prefill,
        Phase::Thinking,
        Phase::Writing,
        Phase::Running,
        Phase::Compacting,
        Phase::Retry,
    ];

    fn legal_from(event: &PhaseEvent) -> Vec<Phase> {
        ALL.into_iter()
            .filter(|phase| next(*phase, event).is_ok())
            .collect()
    }

    #[test]
    fn the_table_allows_what_a_turn_does() {
        use Phase::*;
        let walk = [
            (Ready, PhaseEvent::ServerStart, StartingServer),
            (StartingServer, PhaseEvent::Transcribe, Transcribing),
            (Transcribing, PhaseEvent::StepStart, Prefill),
            (Prefill, PhaseEvent::FirstThinking, Thinking),
            (Thinking, PhaseEvent::FirstText, Writing),
            (
                Writing,
                PhaseEvent::ToolStart {
                    tool: "bash".into(),
                },
                Running,
            ),
            (Running, PhaseEvent::ToolEnd(Outcome::Fail), Prefill),
            (Prefill, PhaseEvent::CompactStart, Compacting),
            (Compacting, PhaseEvent::Compacted, Prefill),
            (Prefill, PhaseEvent::Stir { attempt: 1, max: 2 }, Retry),
            (Retry, PhaseEvent::FirstText, Writing),
            (Writing, PhaseEvent::CutOff, Prefill),
            (Prefill, PhaseEvent::FirstText, Writing),
            (Writing, PhaseEvent::Answer, Ready),
        ];
        for (from, event, to) in walk {
            assert_eq!(next(from, &event), Ok(to), "{from:?} {event:?}");
        }
    }

    #[test]
    fn the_table_refuses_what_must_not_happen() {
        use Phase::*;
        assert_eq!(
            legal_from(&PhaseEvent::ToolEnd(Outcome::Ok)),
            [Running],
            "a result needs a call"
        );
        assert_eq!(
            legal_from(&PhaseEvent::FirstThinking),
            [Prefill, Thinking, Retry],
            "no thinking after the text"
        );
        assert_eq!(legal_from(&PhaseEvent::Compacted), [Compacting]);
        assert_eq!(legal_from(&PhaseEvent::CompactFailed), [Compacting]);
        for phase in [Ready, StartingServer, Transcribing, Running, Compacting] {
            assert!(next(phase, &PhaseEvent::Answer).is_err(), "{phase:?}");
            assert!(next(phase, &PhaseEvent::CutOff).is_err(), "{phase:?}");
        }
        assert!(next(Prefill, &PhaseEvent::ServerStart).is_err());
        assert!(next(Running, &PhaseEvent::StepStart).is_err());
    }

    #[test]
    fn every_phase_can_end() {
        for event in [
            PhaseEvent::Stop(StopReason::Interrupted),
            PhaseEvent::Failed,
            PhaseEvent::Idle,
        ] {
            assert_eq!(legal_from(&event), ALL);
        }
    }

    #[test]
    fn a_phase_entered_again_is_no_transition_but_a_second_stir_is() {
        let mut observer = Observer::new();
        assert!(observer.on(PhaseEvent::StepStart).is_some());
        assert!(observer.on(PhaseEvent::StepStart).is_none());
        let stir = |attempt| PhaseEvent::Stir { attempt, max: 2 };
        assert!(observer.on(stir(1)).is_some());
        assert!(observer.on(stir(2)).is_some());
    }

    #[test]
    fn outside_the_tests_a_violation_is_recorded_and_the_turn_goes_on() {
        let mut observer = Observer {
            strict: false,
            ..Observer::new()
        };
        let transition = observer.on(PhaseEvent::ToolEnd(Outcome::Ok)).unwrap();
        assert!(transition.violation);
        assert_eq!(transition.to, Phase::Prefill);
        assert_eq!(observer.phase, Phase::Prefill);
        assert_eq!(observer.pending[0]["violation"], true);
    }

    #[test]
    #[should_panic(expected = "Violation")]
    fn in_the_tests_a_violation_fails() {
        Observer::new().on(PhaseEvent::ToolEnd(Outcome::Ok));
    }

    #[test]
    fn the_lines_wait_for_the_file_and_carry_the_turn() {
        let dir = std::env::temp_dir().join(format!("agentLIBRE-phase-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut session = Session::create(&dir, "f", std::path::Path::new("/w")).unwrap();
        session
            .append(crate::session::Event::User("first".into()))
            .unwrap();
        let mut observer = Observer::new();
        observer.on(PhaseEvent::ServerStart);
        observer.attach(&session);
        observer.on(PhaseEvent::StepStart);
        let text = std::fs::read_to_string(dir.join(format!("{}.jsonl", session.id))).unwrap();
        let lines: Vec<Json> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|line: &Json| line["type"] == "transition")
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["to"], "StartingServer");
        assert_eq!(lines[1]["from"], "StartingServer");
        assert!(lines.iter().all(|line| line["turn"] == 2));
        assert!(lines.iter().all(|line| line["schema"] == SCHEMA));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
