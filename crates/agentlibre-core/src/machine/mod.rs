//! The state machine of a run: steps, visits, limits and transitions
//! (D11, D15, D33, D42, D44, D82-D84, D93; AGL-121, D42).

use crate::action::Outcome;
use crate::error::Error;
use error::Error as MachineError;
use crate::session::Unfinished;

/// The transitions and the limits of a state, whatever runs the machine
/// (AGL-118, D4; AGL-121, D56): a policy's state and a flow's state each
/// hold one beside their own fields, and one reader reads this part alone.
#[derive(Debug, Clone)]
pub struct Transitions {
    /// The steps of one visit; `None` is the unlimited (D42, D93).
    pub max_steps: Option<u32>,
    /// The visits of a run; `None` is the unlimited (D93).
    pub max_visits: Option<u32>,
    /// Where a used-up limit goes (D93); a limit has one (D83).
    pub on_limit: Option<usize>,
    /// The transitions other than `otherwise`: an event's name, its detail
    /// (`None` for `any`) and the target state (D43, D9).
    pub next: Vec<(String, Option<String>, usize)>,
    /// `next.otherwise`.
    pub otherwise: Option<usize>,
}

impl Transitions {
    /// The state `event` leads to; the most specific key wins (D43): the
    /// detail, then `any`, then `otherwise`. Loading guarantees an answer
    /// (D21).
    pub fn next(&self, event: &Event) -> usize {
        let (name, detail) = event.key();
        self.next_named(name, detail)
    }

    /// The state the event named `name` with `detail` leads to (D43): what
    /// a journal replays without rebuilding the event itself (AGL-118, D3).
    pub fn next_named(&self, name: &str, detail: Option<&str>) -> usize {
        self.next_option(name, detail).expect("complete transitions")
    }

    /// The state the event leads to, or `None` when the state has no
    /// transition for it and no `otherwise`: a flow ignores an event that
    /// moves nothing, where a turn's state is complete by loading (D21).
    pub fn next_option(&self, name: &str, detail: Option<&str>) -> Option<usize> {
        let find = |wanted: Option<&str>| {
            self.next
                .iter()
                .find(|(t, o, _)| t == name && o.as_deref() == wanted)
                .map(|&(_, _, target)| target)
        };
        find(detail).or_else(|| find(None)).or(self.otherwise)
    }

    /// States the state can lead to.
    pub fn targets(&self) -> impl Iterator<Item = usize> + '_ {
        self.next
            .iter()
            .map(|&(_, _, target)| target)
            .chain(self.otherwise)
            .chain(self.on_limit)
    }
}

/// A state of a machine: its name, and the transitions and limits one reader
/// reads (AGL-118, D4; AGL-121, D56). `function::State` gives a turn's
/// fields beside them; a flow's `FlowState` gives its own.
pub trait State {
    /// Its name: the key of a session's record and a journal's lines.
    fn name(&self) -> &str;
    /// What a machine reads of it (D56).
    fn transitions(&self) -> &Transitions;
}

/// What a machine runs on: its states, where it starts, and the limit of a
/// whole run (AGL-118, D4). A Function's policy gives one and a flow's
/// declaration gives another; neither knows about the other (D56).
#[derive(Debug, Clone)]
pub struct Table<'a, S> {
    pub states: &'a [S],
    pub initial: usize,
    /// The steps of one run; `None` is the unlimited (D40, AGL-021, D2).
    pub max_steps_total: Option<u32>,
}

/// What moves a state machine (AGL-118, D4): a call inside a turn, or an
/// event from outside it. The names and their details are the keys of
/// `next.*` (D43, D9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A tool's call ended with `outcome`: `bash.ok` (D43).
    Call { tool: String, outcome: Outcome },
    /// A command a flow ran ended: `exit.ok` (D9).
    Exit(Outcome),
    /// A launch a flow ran ended: `launch.ok` (D10).
    Launch(Outcome),
    /// A turn of a session the flow started ended: `turn.end` (D9).
    TurnEnd,
    /// A flow's timer fired: `timer` (D9).
    Timer,
    /// A human's message: `message.<name>` (D9).
    Message(String),
    /// A check-turn's verdict: the answer named one of its `events` (D5, D9;
    /// AGL-121, D39), the key `<check>.<answer>`.
    Verdict { check: String, answer: String },
}

impl Event {
    /// The name and the detail a transition key is matched against (D43):
    /// the most specific key wins, and `None` is a key's `any`.
    pub fn key(&self) -> (&str, Option<&str>) {
        match self {
            Event::Call { tool, outcome } => (tool, Some(outcome.name())),
            Event::Exit(outcome) => ("exit", Some(outcome.name())),
            Event::Launch(outcome) => ("launch", Some(outcome.name())),
            Event::TurnEnd => ("turn", Some("end")),
            Event::Timer => ("timer", None),
            Event::Message(name) => ("message", Some(name)),
            Event::Verdict { check, answer } => (check, Some(answer)),
        }
    }
}

/// Whether the machine may go on after a step.
#[derive(Debug, PartialEq, Eq)]
pub enum Progress {
    Continue,
    /// `max_steps_total` is reached: the machine stops (D33).
    StepLimit,
}

/// The machine of one run. A new run starts a new machine in `initial`
/// with all counters at zero (D44, D84).
#[derive(Debug)]
pub struct Machine<'f, S> {
    table: Table<'f, S>,
    state: usize,
    /// Steps in the current visit of `state`.
    steps_in_visit: u32,
    /// Visits of each state in this run.
    visits: Vec<u32>,
    steps_total: u32,
    /// Whether the last call started a visit.
    entered: bool,
}

impl<'f, S: State> Machine<'f, S> {
    pub fn new(table: Table<'f, S>) -> Machine<'f, S> {
        let initial = table.initial;
        let visits = vec![0; table.states.len()];
        let mut machine = Machine {
            table,
            state: initial,
            steps_in_visit: 0,
            visits,
            steps_total: 0,
            entered: false,
        };
        machine.enter(initial);
        machine
    }

    /// Continues an unfinished turn (D105): the state and visits come from
    /// the session, `max_steps_total` counts anew. `function_name` names
    /// the Function in errors.
    pub fn resume(
        table: Table<'f, S>,
        function_name: &str,
        turn: &Unfinished,
    ) -> Result<Machine<'f, S>, Error> {
        let index = |name: &str| {
            table
                .states
                .iter()
                .position(|s| s.name() == name)
                .ok_or_else(|| MachineError::SessionState {
                    state: name.into(),
                    function: function_name.into(),
                })
        };
        let Some(state) = &turn.state else {
            return Ok(Machine::new(table));
        };
        let mut visits = vec![0; table.states.len()];
        for (name, count) in &turn.visits {
            visits[index(name)?] = *count;
        }
        Ok(Machine {
            table,
            state: index(state)?,
            steps_in_visit: turn.steps_in_visit,
            visits,
            steps_total: 0,
            entered: false,
        })
    }

    /// The current state.
    pub fn state(&self) -> &'f S {
        &self.table.states[self.state]
    }

    /// Whether the last call started a visit, to log it (D104).
    pub fn entered(&self) -> bool {
        self.entered
    }

    /// Records a call of `tool` that ended with `outcome` and moves to the
    /// next state. An answer ends the turn instead (D44).
    /// Records an event from outside the machine and moves to the next state
    /// (D4): what a flow's runner calls, as `after_call` does for a turn.
    pub fn after_event(&mut self, event: Event) -> Progress {
        self.after(Some(event))
    }

    pub fn after_call(&mut self, tool: &str, outcome: Outcome) -> Progress {
        self.after(Some(Event::Call {
            tool: tool.into(),
            outcome,
        }))
    }

    /// Records a discarded action: a step that keeps the state unless the
    /// visit's steps are used up (D154).
    pub fn after_cut_off(&mut self) -> Progress {
        self.after(None)
    }

    fn after(&mut self, event: Option<Event>) -> Progress {
        self.entered = false;
        self.steps_total += 1;
        self.steps_in_visit += 1;
        let moves = self.state().transitions();
        let target = event.map_or(self.state, |event| moves.next(&event));
        if target != self.state {
            self.enter(target);
        } else if moves
            .max_steps
            .is_some_and(|max| self.steps_in_visit >= max)
        {
            // Staying would exceed the visit's steps (D42, D93).
            self.enter(moves.on_limit.expect("a limit has on_limit"));
        }
        // A `None` limit is the unlimited (AGL-021, D2); it never stops.
        if self
            .table
            .max_steps_total
            .is_some_and(|max| self.steps_total >= max)
        {
            Progress::StepLimit
        } else {
            Progress::Continue
        }
    }

    /// Starts a visit of `target`, or of its `on_limit` when its visits are
    /// used up (D93). `on_limit` has no cycles (D83), so this ends.
    fn enter(&mut self, mut target: usize) {
        loop {
            let moves = self.table.states[target].transitions();
            if !moves
                .max_visits
                .is_some_and(|max| self.visits[target] >= max)
            {
                break;
            }
            target = moves.on_limit.expect("a limit has on_limit");
        }
        self.visits[target] += 1;
        self.state = target;
        self.steps_in_visit = 0;
        self.entered = true;
    }
}

pub mod error;

#[cfg(test)]
mod tests;
