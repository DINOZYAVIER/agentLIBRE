//! The state machine of one turn: steps, visits, limits and transitions
//! (D11, D15, D33, D42, D44, D82-D84, D93).

use crate::action::Outcome;
use crate::error::Error;
use crate::function::State;
use crate::session::Unfinished;

/// What a machine runs on: its states, where it starts, and the limit of a
/// whole run (AGL-118, D4). A Function's policy gives one today; a flow's
/// declaration will give another, and neither knows about the other.
#[derive(Debug, Clone, Copy)]
pub struct Table<'a> {
    pub states: &'a [State],
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
    /// A turn of a session the flow started ended: `turn.end` (D9).
    TurnEnd,
    /// A flow's timer fired: `timer` (D9).
    Timer,
    /// A human's message: `message.<name>` (D9).
    Message(String),
    /// A check-turn's answer, one of its `events` (D5, D9): `<check>.<answer>`.
    Answer { check: String, answer: String },
}

impl Event {
    /// The name and the detail a transition key is matched against (D43):
    /// the most specific key wins, and `None` is a key's `any`.
    pub fn key(&self) -> (&str, Option<&str>) {
        match self {
            Event::Call { tool, outcome } => (tool, Some(outcome.name())),
            Event::Exit(outcome) => ("exit", Some(outcome.name())),
            Event::TurnEnd => ("turn", Some("end")),
            Event::Timer => ("timer", None),
            Event::Message(name) => ("message", Some(name)),
            Event::Answer { check, answer } => (check, Some(answer)),
        }
    }
}

/// Whether the turn may go on after a step.
#[derive(Debug, PartialEq, Eq)]
pub enum Progress {
    Continue,
    /// `max_steps_total` is reached: the turn stops (D33).
    StepLimit,
}

/// The machine for one user message. A new message starts a new machine
/// in `initial` with all counters at zero (D44, D84).
#[derive(Debug)]
pub struct Machine<'f> {
    table: Table<'f>,
    state: usize,
    /// Steps in the current visit of `state`.
    steps_in_visit: u32,
    /// Visits of each state in this turn.
    visits: Vec<u32>,
    steps_total: u32,
    /// Whether the last call started a visit.
    entered: bool,
}

impl<'f> Machine<'f> {
    pub fn new(table: Table<'f>) -> Machine<'f> {
        let mut machine = Machine {
            table,
            state: table.initial,
            steps_in_visit: 0,
            visits: vec![0; table.states.len()],
            steps_total: 0,
            entered: false,
        };
        machine.enter(table.initial);
        machine
    }

    /// Continues an unfinished turn (D105): the state and visits come from
    /// the session, `max_steps_total` counts anew. `function_name` names
    /// the Function in errors.
    pub fn resume(
        table: Table<'f>,
        function_name: &str,
        turn: &Unfinished,
    ) -> Result<Machine<'f>, Error> {
        let index = |name: &str| {
            table
                .states
                .iter()
                .position(|s| s.name == name)
                .ok_or_else(|| Error::SessionState {
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
    pub fn state(&self) -> &'f State {
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
        let state = self.state();
        let target = event.map_or(self.state, |event| state.next(&event));
        if target != self.state {
            self.enter(target);
        } else if state
            .max_steps
            .is_some_and(|max| self.steps_in_visit >= max)
        {
            // Staying would exceed the visit's steps (D42, D93).
            self.enter(state.on_limit.expect("a limit has on_limit"));
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
            let state = &self.table.states[target];
            if !state
                .max_visits
                .is_some_and(|max| self.visits[target] >= max)
            {
                break;
            }
            target = state.on_limit.expect("a limit has on_limit");
        }
        self.visits[target] += 1;
        self.state = target;
        self.steps_in_visit = 0;
        self.entered = true;
    }
}

#[cfg(test)]
mod tests;
