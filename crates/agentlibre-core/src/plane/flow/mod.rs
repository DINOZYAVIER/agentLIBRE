//! The flows of a plane, as its `flow.nix` declares them (AGL-118, D1, D9):
//! a machine above the turn, whose events come from outside it — a script's
//! exit code, a turn's end, a timer, a human's message — and whose actions
//! are mechanical or start a turn.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::action::Outcome;

pub mod act;
pub mod journal;
use crate::error::{Error, FunctionProblem};
use crate::function::State;
use crate::machine::Table;
use crate::nix::{self, Section};

/// The file of a plane's flows, beside `plane.nix` (D1).
pub const FLOW_FILE: &str = "flow.nix";

/// What a state of a flow does (D9). A state may carry any of these; the
/// mechanical ones run in the plane, `Say` starts a turn of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// `action.command = { cmd; timeout; }`: its exit moves the state (D9).
    /// The timeout is not optional: a build and a lint are not the same wait.
    Command { cmd: String, timeout: Duration },
    /// `action.review = { function; events; }`: a turn under another Function
    /// whose answer is one of `events` (D5).
    Review { function: String, events: Vec<String> },
    /// `action.artifact = { dir; keep; }`: what a rollback may reach (D6).
    Artifact { dir: PathBuf, keep: u32 },
    /// `action.say = "<session>"`: a turn started in a session, named as
    /// `Open.resume` names one (D6, D9).
    Say { session: String },
}

/// A flow as declared: its states, and what each of them does (D4, D9).
#[derive(Debug)]
pub struct FlowDecl {
    name: String,
    states: Vec<State>,
    initial: usize,
    max_steps_total: Option<u32>,
    actions: Vec<Vec<Action>>,
}

impl FlowDecl {
    /// The table its machine runs on, the same type a policy's states give
    /// (D4): one reader of transitions for both layers.
    pub fn table(&self) -> Table<'_> {
        Table {
            states: &self.states,
            initial: self.initial,
            max_steps_total: self.max_steps_total,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Where the flow starts, by name.
    pub fn initial(&self) -> &str {
        &self.states[self.initial].name
    }

    /// What the state `at` does (D9).
    pub fn actions(&self, at: usize) -> &[Action] {
        &self.actions[at]
    }

    /// The name of the state `at`.
    pub fn state_name(&self, at: usize) -> &str {
        &self.states[at].name
    }

    /// The states in order, as `agl plane flow status` walks them.
    pub fn states(&self) -> &[State] {
        &self.states
    }
}

/// A plane's flows as its `flow.nix` gives them (D1).
#[derive(Debug, Default)]
pub struct FlowFile {
    pub flows: Vec<FlowDecl>,
}

impl FlowFile {
    /// Reads `<plane_dir>/flow.nix`; no file is no flows (D1).
    pub fn load(plane_dir: &Path) -> Result<FlowFile, Error> {
        let path = plane_dir.join(FLOW_FILE);
        if !path.is_file() {
            return Ok(FlowFile::default());
        }
        let mut top = Section::top(&path, nix::eval_file(&path)?)?;
        top.check_keys(&["flow"])?;
        let mut declared = top.section("flow")?.ok_or_else(|| top.missing("flow"))?;
        let mut flows = Vec::new();
        for (name, at) in declared.sections()? {
            flows.push(read_flow(name, at)?);
        }
        Ok(FlowFile { flows })
    }

    pub fn find(&self, name: &str) -> Option<&FlowDecl> {
        self.flows.iter().find(|f| f.name == name)
    }
}

/// The error of a flow file; `key` locates it in the file (D1).
fn bad(at: &Section, key: &str, problem: FunctionProblem) -> Error {
    Error::Function {
        path: at.path().into(),
        key: at.key(key),
        problem,
    }
}

/// A transition before the states have indices: the event's name and detail,
/// and the target's name (D9).
struct Transition {
    name: String,
    detail: Option<String>,
    target: String,
}

/// One state as declared, with its target names still unresolved.
struct Declared {
    name: String,
    actions: Vec<Action>,
    next: Vec<Transition>,
    otherwise: Option<String>,
    max_steps: Option<u32>,
    max_visits: Option<u32>,
    on_limit: Option<String>,
}

/// Reads one flow of `flow.nix` (D1, D9).
fn read_flow(name: String, mut at: Section) -> Result<FlowDecl, Error> {
    at.check_keys(&["initial", "max_steps_total", "state"])?;
    let initial = at.required_string("initial")?;
    let max_steps_total = match at.integer("max_steps_total")? {
        Some(n) if n >= 1 => Some(n as u32),
        Some(_) => return Err(bad(&at, "max_steps_total", FunctionProblem::NotPositive)),
        None => None,
    };
    let mut states_at = at.required_section("state")?;
    let mut declared = Vec::new();
    for (state, mut body) in states_at.sections()? {
        body.check_keys(&[
            "action",
            "next",
            "max_steps",
            "max_visits",
            "on_limit",
        ])?;
        let limit = |body: &mut Section, key: &str| -> Result<Option<u32>, Error> {
            match body.integer(key)? {
                Some(n) if n >= 1 => Ok(Some(n as u32)),
                Some(_) => Err(bad(body, key, FunctionProblem::NotPositive)),
                None => Ok(None),
            }
        };
        let max_steps = limit(&mut body, "max_steps")?;
        let max_visits = limit(&mut body, "max_visits")?;
        let on_limit = body.string("on_limit")?;
        let (next, otherwise) = read_next(&mut body)?;
        declared.push(Declared {
            name: state,
            actions: read_actions(&mut body)?,
            next,
            otherwise,
            max_steps,
            max_visits,
            on_limit,
        });
    }
    resolve(&at, name, &initial, max_steps_total, declared)
}

/// Reads `action.*` of one state (D9). A state may have none: it then waits
/// for an event from outside, as `halted` waits for the human (D6).
fn read_actions(at: &mut Section) -> Result<Vec<Action>, Error> {
    let Some(mut actions) = at.section("action")? else {
        return Ok(Vec::new());
    };
    actions.check_keys(&["command", "review", "artifact", "say"])?;
    let mut read = Vec::new();
    if let Some(mut one) = actions.section("command")? {
        one.check_keys(&["cmd", "timeout"])?;
        let cmd = one.required_string("cmd")?;
        let timeout = match one.required_integer("timeout")? {
            n if n >= 1 => n as u64,
            _ => return Err(bad(&one, "timeout", FunctionProblem::NotPositive)),
        };
        read.push(Action::Command {
            cmd,
            timeout: Duration::from_secs(timeout),
        });
    }
    if let Some(mut one) = actions.section("review")? {
        one.check_keys(&["function", "events"])?;
        let function = one.required_string("function")?;
        let events = one.required_strings("events")?;
        if events.is_empty() {
            return Err(bad(&one, "events", FunctionProblem::EmptyEnum));
        }
        read.push(Action::Review { function, events });
    }
    if let Some(mut one) = actions.section("artifact")? {
        one.check_keys(&["dir", "keep"])?;
        let dir = PathBuf::from(one.required_string("dir")?);
        let keep = match one.required_integer("keep")? {
            n if n >= 1 => n as u32,
            _ => return Err(bad(&one, "keep", FunctionProblem::NotPositive)),
        };
        read.push(Action::Artifact { dir, keep });
    }
    if let Some(session) = actions.string("say")? {
        read.push(Action::Say { session });
    }
    Ok(read)
}

/// Reads `next.*` of one state: `next.<event>[.<detail>] = <state>` (D9).
/// `next.otherwise` is the fallback; `next.<event>.any` covers the event
/// however it ended, as a tool's `any` does (D43).
fn read_next(at: &mut Section) -> Result<(Vec<Transition>, Option<String>), Error> {
    let Some(mut next) = at.section("next")? else {
        return Ok((Vec::new(), None));
    };
    let otherwise = next.string("otherwise")?;
    let mut read = Vec::new();
    for key in next.keys() {
        if key == "otherwise" {
            continue;
        }
        // An event is either a bare target — `next.timer = "build"` — or a
        // set of details, as a tool's is (D41, D9).
        match next.section(&key)? {
            Some(mut details) => {
                for (name, target) in details.string_entries()? {
                    read.push(Transition {
                        name: key.clone(),
                        detail: match name.as_str() {
                            "any" => None,
                            other => Some(other.to_string()),
                        },
                        target,
                    });
                }
            }
            None => read.push(Transition {
                name: key.clone(),
                detail: None,
                target: next.required_string(&key)?,
            }),
        }
    }
    Ok((read, otherwise))
}

/// Gives the declared states their indices, checks what a declaration must
/// hold, and builds the table (D4, D9). `at` is the flow's section, so an
/// error names the key a human would edit.
fn resolve(
    at: &Section,
    name: String,
    initial: &str,
    max_steps_total: Option<u32>,
    declared: Vec<Declared>,
) -> Result<FlowDecl, Error> {
    let index = |who: &str, target: &str| -> Result<usize, Error> {
        declared
            .iter()
            .position(|d| d.name == target)
            .ok_or_else(|| bad(at, who, FunctionProblem::UnknownState(target.into())))
    };
    let initial = index("initial", initial)?;
    let mut states = Vec::new();
    let mut actions = Vec::new();
    for d in &declared {
        let mut transitions = Vec::new();
        for t in &d.next {
            transitions.push((
                t.name.clone(),
                t.detail.clone(),
                index(&format!("state.{}.next", d.name), &t.target)?,
            ));
        }
        let otherwise = d
            .otherwise
            .as_deref()
            .map(|target| index(&format!("state.{}.next.otherwise", d.name), target))
            .transpose()?;
        let on_limit = d
            .on_limit
            .as_deref()
            .map(|target| index(&format!("state.{}.on_limit", d.name), target))
            .transpose()?;
        let limited = d.max_steps.is_some() || d.max_visits.is_some();
        if limited && on_limit.is_none() {
            return Err(Error::MissingKey {
                path: at.path().into(),
                key: at.key(&format!("state.{}.on_limit", d.name)),
            });
        }
        if !limited && on_limit.is_some() {
            return Err(bad(
                at,
                &format!("state.{}.on_limit", d.name),
                FunctionProblem::OnLimitWithoutLimit,
            ));
        }
        check_events(at, d, &transitions, otherwise.is_some())?;
        states.push(State::of_table(
            d.name.clone(),
            d.max_steps,
            d.max_visits,
            on_limit,
            transitions,
            otherwise,
        ));
        actions.push(d.actions.clone());
    }
    check_on_limit(at, &states)?;
    Ok(FlowDecl {
        name,
        states,
        initial,
        max_steps_total,
        actions,
    })
}

/// Every way an action can end needs a transition or `next.otherwise` (D21's
/// rule, for a flow's own events): a command's four exits, a review's events
/// (D5, D9).
fn check_events(
    at: &Section,
    d: &Declared,
    given: &[(String, Option<String>, usize)],
    any: bool,
) -> Result<(), Error> {
    let covers = |name: &str, detail: &str| {
        any || given.iter().any(|(n, o, _)| {
            n == name && o.as_deref().is_none_or(|o| o == detail)
        })
    };
    let key = format!("state.{}.next", d.name);
    for action in &d.actions {
        match action {
            Action::Command { .. } => {
                for outcome in Outcome::ALL {
                    if !covers("exit", outcome.name()) {
                        return Err(bad(
                            at,
                            &key,
                            FunctionProblem::MissingTransition {
                                tool: "exit".into(),
                                outcome: outcome.name().into(),
                            },
                        ));
                    }
                }
            }
            Action::Review { events, .. } => {
                for event in events {
                    if !covers("review", event) {
                        return Err(bad(
                            at,
                            &key,
                            FunctionProblem::MissingTransition {
                                tool: "review".into(),
                                outcome: event.clone(),
                            },
                        ));
                    }
                }
            }
            // An artifact is put in place, and a `say` starts a turn: neither
            // has outcomes of its own (D6).
            Action::Artifact { .. } | Action::Say { .. } => {}
        }
    }
    Ok(())
}

/// `on_limit` has no cycles (D83), so entering a limit always ends.
fn check_on_limit(at: &Section, states: &[State]) -> Result<(), Error> {
    for from in 0..states.len() {
        let mut seen = vec![false; states.len()];
        let mut at_state = from;
        while let Some(next) = states[at_state].on_limit {
            if seen[next] {
                return Err(bad(at, "state", FunctionProblem::OnLimitCycle));
            }
            seen[next] = true;
            at_state = next;
        }
    }
    Ok(())
}

pub use journal::{Entry, FlowRun};

#[cfg(test)]
mod tests;
