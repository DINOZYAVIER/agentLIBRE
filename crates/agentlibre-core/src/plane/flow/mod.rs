//! The flows of a plane, as its `flow.nix` declares them (AGL-118, D1, D9):
//! a machine above the turn, whose events come from outside it — a script's
//! exit code, a turn's end, a timer, a human's message — and whose actions
//! are mechanical or start a turn.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use crate::action::Outcome;

pub mod act;
pub mod holder;
pub mod drive;
pub mod journal;
use crate::error::Error;
use crate::function::error::{Error as FunctionError, FunctionProblem};
use crate::machine::{Table, Transitions};
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
    /// `action.wake = "<session>"`, or `{ session; text? }` (D14): a wake
    /// delivered to a session named as `Open.resume` names one (D6, D9),
    /// with the flow's own line when the declaration gives one.
    Wake {
        session: String,
        text: Option<String>,
    },
    /// `action.launch = { cmd; timeout; }`: what a rollback runs to bring the
    /// artifact back; its event is `launch.ok`/`launch.fail` (D10).
    Launch { cmd: String, timeout: Duration },
}

/// A state of a flow as declared (AGL-118, D9; AGL-121, D42): what the flow
/// does here, what it waits for, and the transitions and limits one reader
/// reads (D56) — a state of its own, not a policy's with vectors beside it.
#[derive(Debug)]
pub struct FlowState {
    name: String,
    /// `action.*`: what the state does, in the order it runs (D9).
    actions: Vec<Action>,
    /// `timer = <seconds>`: while the flow stands in this state, `timer`
    /// fires once the seconds run out (D9, D16).
    timer: Option<Duration>,
    /// `nudges = [ "go" ]`: the `message` events this agent may deliver
    /// in this state, empty by default (D15, D21).
    nudges: Vec<String>,
    /// The transitions and the limits (AGL-118, D4; AGL-121, D56).
    transitions: Transitions,
}

impl FlowState {
    /// The state the event named `name` with `detail` leads to, or `None`
    /// when nothing moves (D21): what a journal replays (AGL-118, D3).
    pub fn next_option(&self, name: &str, detail: Option<&str>) -> Option<usize> {
        self.transitions.next_option(name, detail)
    }

    /// The state the named event leads to (D43).
    pub fn next_named(&self, name: &str, detail: Option<&str>) -> usize {
        self.transitions.next_named(name, detail)
    }
}

impl crate::machine::State for FlowState {
    fn name(&self) -> &str {
        &self.name
    }

    fn transitions(&self) -> &Transitions {
        &self.transitions
    }
}

/// A flow as declared: its states, each with what it does (D4, D9; D42).
#[derive(Debug)]
pub struct FlowDecl {
    name: String,
    states: Vec<FlowState>,
    initial: usize,
    max_steps_total: Option<u32>,
}

impl FlowDecl {
    /// The table its machine runs on: the same reader a policy's states
    /// give (D4, D56), over this flow's own states (D42).
    pub fn table(&self) -> Table<'_, FlowState> {
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
        &self.states[at].actions
    }

    /// The state's `timer`, seconds since the flow came to rest in it before
    /// `timer` fires (D9, D16).
    pub fn timer(&self, at: usize) -> Option<Duration> {
        self.states[at].timer
    }

    /// The state's `nudges`: the `message` events this agent may deliver
    /// here, empty by default (D15, D21).
    pub fn nudges(&self, at: usize) -> &[String] {
        &self.states[at].nudges
    }

    /// The name of the state `at`.
    pub fn state_name(&self, at: usize) -> &str {
        &self.states[at].name
    }

    /// The states in order, as `agl plane flow status` walks them.
    pub fn states(&self) -> &[FlowState] {
        &self.states
    }
}

/// A plane's flows as its `flow.nix` gives them (D1).
#[derive(Debug, Default)]
pub struct FlowFile {
    pub flows: Vec<FlowDecl>,
}

/// What `load` keeps by the file's mtime (AGL-121, D36): a `flow.nix` is
/// evaluated once and read from here until the file changes — not per
/// letter, not per timer retry. A change in a file it imports goes unseen;
/// the real ones import none. A failed read is kept too: a broken file
/// says the same thing to every look until the human mends it.
type Reading = (SystemTime, Result<Arc<FlowFile>, Arc<Error>>);

static CACHE: OnceLock<Mutex<HashMap<PathBuf, Reading>>> = OnceLock::new();

impl FlowFile {
    /// Reads `<plane_dir>/flow.nix` through the mtime cache (D36); no file
    /// is no flows (D1).
    pub fn load(plane_dir: &Path) -> Result<Arc<FlowFile>, Arc<Error>> {
        let path = plane_dir.join(FLOW_FILE);
        // No file, or no mtime to key on: no flows, and no cache.
        let Ok(mtime) = path.metadata().and_then(|meta| meta.modified()) else {
            return Ok(Arc::new(FlowFile::default()));
        };
        let cache = CACHE.get_or_init(Default::default);
        let held = cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&path)
            .filter(|(at, _)| *at == mtime)
            .map(|(_, read)| read.clone());
        match held {
            Some(read) => read,
            None => {
                let read = read_file(&path).map(Arc::new).map_err(Arc::new);
                cache
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(path, (mtime, read.clone()));
                read
            }
        }
    }

    pub fn find(&self, name: &str) -> Option<&FlowDecl> {
        self.flows.iter().find(|f| f.name == name)
    }
}

/// The one evaluation of `flow.nix`: the nix read and the declarations.
fn read_file(path: &Path) -> Result<FlowFile, Error> {
    let mut top = Section::top(path, nix::eval_file(path)?)?;
    top.check_keys(&["flow"])?;
    let mut declared = top.section("flow")?.ok_or_else(|| top.missing("flow"))?;
    let mut flows = Vec::new();
    for (name, at) in declared.sections()? {
        flows.push(read_flow(name, at)?);
    }
    Ok(FlowFile { flows })
}

/// The error of a flow file; `key` locates it in the file (D1).
fn bad(at: &Section, key: &str, problem: FunctionProblem) -> Error {
    FunctionError::Invalid {
        path: at.path().into(),
        key: at.key(key),
        problem,
    }
    .into()
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
    /// `timer = <seconds>`: while the flow stands in this state, `timer`
    /// fires once the seconds run out (D9, D16).
    timer: Option<Duration>,
    /// `nudges = [ "go" ]`: the `message` events this agent may deliver
    /// in this state, empty by default (D15).
    nudges: Vec<String>,
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
            "timer",
            "nudges",
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
        let timer = match body.integer("timer")? {
            Some(n) if n >= 1 => Some(Duration::from_secs(n as u64)),
            Some(_) => return Err(bad(&body, "timer", FunctionProblem::NotPositive)),
            None => None,
        };
        // The `message` events this agent may deliver here (D15).
        let nudges = body.strings("nudges")?.unwrap_or_default();
        let (next, otherwise) = read_next(&mut body)?;
        declared.push(Declared {
            name: state,
            actions: read_actions(&mut body)?,
            next,
            otherwise,
            max_steps,
            max_visits,
            on_limit,
            timer,
            nudges,
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
    actions.check_keys(&["command", "review", "artifact", "wake", "launch"])?;
    let mut read = Vec::new();
    let wait = |one: &mut Section| -> Result<(String, Duration), Error> {
        one.check_keys(&["cmd", "timeout"])?;
        let cmd = one.required_string("cmd")?;
        let timeout = match one.required_integer("timeout")? {
            n if n >= 1 => n as u64,
            _ => return Err(bad(one, "timeout", FunctionProblem::NotPositive)),
        };
        Ok((cmd, Duration::from_secs(timeout)))
    };
    if let Some(mut one) = actions.section("command")? {
        let (cmd, timeout) = wait(&mut one)?;
        read.push(Action::Command { cmd, timeout });
    }
    if let Some(mut one) = actions.section("launch")? {
        let (cmd, timeout) = wait(&mut one)?;
        read.push(Action::Launch { cmd, timeout });
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
    // `action.wake` is a bare session name or `{ session; text? }` (D14).
    if actions.is_object("wake") {
        let mut one = actions.section("wake")?.expect("just seen");
        one.check_keys(&["session", "text"])?;
        let session = one.required_string("session")?;
        let text = one.string("text")?;
        read.push(Action::Wake { session, text });
    } else if let Some(session) = actions.string("wake")? {
        read.push(Action::Wake {
            session,
            text: None,
        });
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
        if next.is_object(&key) {
            let mut details = next.section(&key)?.expect("just seen");
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
        } else {
            read.push(Transition {
                name: key.clone(),
                detail: None,
                target: next.required_string(&key)?,
            });
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
        states.push(FlowState {
            name: d.name.clone(),
            actions: d.actions.clone(),
            timer: d.timer,
            nudges: d.nudges.clone(),
            transitions: Transitions {
                max_steps: d.max_steps,
                max_visits: d.max_visits,
                on_limit,
                next: transitions,
                otherwise,
            },
        });
    }
    check_on_limit(at, &states)?;
    Ok(FlowDecl {
        name,
        states,
        initial,
        max_steps_total,
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
            Action::Launch { .. } => {
                for outcome in Outcome::ALL {
                    if !covers("launch", outcome.name()) {
                        return Err(bad(
                            at,
                            &key,
                            FunctionProblem::MissingTransition {
                                tool: "launch".into(),
                                outcome: outcome.name().into(),
                            },
                        ));
                    }
                }
            }
            // An artifact is pointed at, and a `say` starts a turn: neither
            // has an outcome of its own (D6, D10).
            Action::Artifact { .. } | Action::Wake { .. } => {}
        }
    }
    Ok(())
}

/// `on_limit` has no cycles (D83), so entering a limit always ends.
fn check_on_limit(at: &Section, states: &[FlowState]) -> Result<(), Error> {
    for from in 0..states.len() {
        let mut seen = vec![false; states.len()];
        let mut at_state = from;
        while let Some(next) = states[at_state].transitions.on_limit {
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

/// What is asked of a flow (AGL-118, D2, D7): one verb for the human's CLI
/// and the agent's tool, the same action of the same machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verb {
    /// Start a flow, or go on with one a human stopped (D2, D7).
    Start { name: String },
    /// Stop a flow; it stays where it stood (D7).
    Stop { name: String },
    /// Where a flow stands, from the fold of its journal (D7).
    Status { name: String },
    /// The flows the plane declares, each with where it stands (D7).
    List,
    /// Begin again from `initial`, the old journal kept beside it (D11).
    Reset { name: String },
    /// Deliver the `message.<event>` to a flow (D15, D21): the human's way,
    /// bounded by no list — the human owns the machine (D7).
    Message { name: String, event: String },
    /// The agent's way of the same delivery, bounded state by state by the
    /// declaration's `nudges` (D15, D21).
    Nudge { name: String, event: String },
}

#[cfg(test)]
mod tests;
