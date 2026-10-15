//! The state machine of one turn: steps, visits, limits and transitions
//! (D11, D15, D33, D42, D44, D82-D84, D93).

use crate::action::Outcome;
use crate::error::Error;
use crate::function::{Function, State};
use crate::session::Unfinished;

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
    function: &'f Function,
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
    pub fn new(function: &'f Function) -> Machine<'f> {
        let mut machine = Machine {
            function,
            state: function.initial,
            steps_in_visit: 0,
            visits: vec![0; function.states.len()],
            steps_total: 0,
            entered: false,
        };
        machine.enter(function.initial);
        machine
    }

    /// Continues an unfinished turn (D105): the state and visits come from
    /// the session, `max_steps_total` counts anew. `function_name` names
    /// the Function in errors.
    pub fn resume(
        function: &'f Function,
        function_name: &str,
        turn: &Unfinished,
    ) -> Result<Machine<'f>, Error> {
        let index = |name: &str| {
            function
                .states
                .iter()
                .position(|s| s.name == name)
                .ok_or_else(|| Error::SessionState {
                    state: name.into(),
                    function: function_name.into(),
                })
        };
        let Some(state) = &turn.state else {
            return Ok(Machine::new(function));
        };
        let mut visits = vec![0; function.states.len()];
        for (name, count) in &turn.visits {
            visits[index(name)?] = *count;
        }
        Ok(Machine {
            function,
            state: index(state)?,
            steps_in_visit: turn.steps_in_visit,
            visits,
            steps_total: 0,
            entered: false,
        })
    }

    /// The current state.
    pub fn state(&self) -> &'f State {
        &self.function.states[self.state]
    }

    /// Whether the last call started a visit, to log it (D104).
    pub fn entered(&self) -> bool {
        self.entered
    }

    /// Records a call of `tool` that ended with `outcome` and moves to the
    /// next state. An answer ends the turn instead (D44).
    pub fn after_call(&mut self, tool: &str, outcome: Outcome) -> Progress {
        self.after(Some((tool, outcome)))
    }

    /// Records a discarded action: a step that keeps the state unless the
    /// visit's steps are used up (D154).
    pub fn after_cut_off(&mut self) -> Progress {
        self.after(None)
    }

    fn after(&mut self, call: Option<(&str, Outcome)>) -> Progress {
        self.entered = false;
        self.steps_total += 1;
        self.steps_in_visit += 1;
        let state = self.state();
        let target = call.map_or(self.state, |(tool, outcome)| state.next(tool, outcome));
        if target != self.state {
            self.enter(target);
        } else if state
            .max_steps
            .is_some_and(|max| self.steps_in_visit >= max)
        {
            // Staying would exceed the visit's steps (D42, D93).
            self.enter(state.on_limit.expect("a limit has on_limit"));
        }
        if self.steps_total >= self.function.max_steps_total {
            Progress::StepLimit
        } else {
            Progress::Continue
        }
    }

    /// Starts a visit of `target`, or of its `on_limit` when its visits are
    /// used up (D93). `on_limit` has no cycles (D83), so this ends.
    fn enter(&mut self, mut target: usize) {
        loop {
            let state = &self.function.states[target];
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
