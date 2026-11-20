//! Hosting a flow above the turn (AGL-118, D3, D7): the journal is the truth,
//! so a run is folded from it when it is touched and set down again; what a
//! state does by itself runs here, on the plane's thread. What needs a turn —
//! a `say` or a review — waits for a session, and `status` says what waits
//! (D6, D7).
//!
//! A flow's commands run in `workdir`, the workspace of the session that
//! asked: a build and a launch are the plane's own, as `action.artifact`
//! points at the plane's builds (D6, D10).

use std::path::Path;

use crate::error::Error;
use crate::i18n::{m, Lang};
use crate::machine::{Event, Progress};
use crate::plane::{Said, Told};
use crate::tr;

use super::act;
use super::journal::{self, Entry, FlowRun};
use super::{Action, FlowDecl, FlowFile, Verb, FLOW_FILE};

/// The journal's lines `status` shows (D7): the fold answers short, the prose
/// stays in the journal.
const TAIL: usize = 5;

/// The steps one touch of a flow may take: states that hand the run to each
/// other with no outside event in between chase their own tail, and a run
/// that long says the declaration does that (D9). A human bounds it in the
/// file with `max_visits` or `max_steps_total`; this is the wall behind them.
const MAX_WALK: usize = 32;

/// What a state waits for from a session (D6, D9): the plane runs the turn,
/// which is not this layer's to start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Waiting {
    /// `action.say = "<session>"`: a turn of that session (D6).
    Say { session: String },
    /// `action.review = { function; events; }`: a turn whose answer is one of
    /// `events`, under another Function (D5).
    Review { function: String, events: Vec<String> },
}

/// Where a flow stands, folded from its journal (D3, D7).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Place {
    /// The state's name; `None` when the flow was never started.
    pub state: Option<String>,
    /// Visits of each state, from the first of them (D3).
    pub visits: Vec<(String, u32)>,
    /// A human stopped it; it stays where it stood (D7).
    pub stopped: bool,
    /// What its state waits for (D6).
    pub waiting: Vec<Waiting>,
    /// The journal's last lines, oldest first, in their short form (D7).
    pub tail: Vec<String>,
}

/// Does `verb` of a plane's flows (AGL-118, D2, D7): `agl plane flow` and the
/// agent's `flow` tool, the same verb of the same machine. The declarations
/// are read again every time, so an edited `flow.nix` cannot move a running
/// flow's future transitions without a word — the fold refuses a mismatch
/// (D11). A state's own commands run here, in `workdir`; what needs a turn
/// waits for a session, and the place says what waits (D6, D7).
pub fn run(plane_dir: &Path, workdir: &Path, verb: &Verb, lang: Lang) -> Said {
    let decls = match FlowFile::load(plane_dir) {
        Ok(decls) => decls,
        Err(error) => return Said::fault(&error, lang),
    };
    let declared = |name: &str| -> Result<&FlowDecl, String> {
        decls.find(name).ok_or_else(|| {
            tr!(lang, m::FLOW_UNKNOWN, name = name, file = FLOW_FILE).to_string()
        })
    };
    let place_said = |name: &str, result: Result<Place, Error>| match result {
        Ok(place) => Said::told(Told::Output(said(lang, name, &place))),
        Err(error) => Said::fault(&error, lang),
    };
    // A refused verb did not do what was asked: it is a failed one (D2).
    let refused = |text: String| Said {
        lines: vec![Told::Refusal(text.clone())],
        content: Some(text),
        failed: true,
        ..Said::default()
    };
    match verb {
        Verb::List => {
            if decls.flows.is_empty() {
                return Said::told(Told::Notice(
                    tr!(lang, m::FLOW_NONE, file = FLOW_FILE).to_string(),
                ));
            }
            let lines = decls
                .flows
                .iter()
                .map(|decl| match place(decl, plane_dir) {
                    // A list is one line per flow: what it waits for and the
                    // journal's tail are the verb `status` for (D7).
                    Ok(place) => match place.state.as_deref() {
                        Some(state) => {
                            let mark = if place.stopped {
                                format!(" ({})", tr!(lang, m::FLOW_STOPPED))
                            } else {
                                String::new()
                            };
                            format!("{}: {state}{mark}", decl.name())
                        }
                        None => tr!(lang, m::FLOW_NOT_STARTED, name = decl.name()).to_string(),
                    },
                    Err(error) => format!("{}: {}", decl.name(), error.message(lang)),
                })
                .collect::<Vec<_>>()
                .join("\n");
            Said::told(Told::Output(lines))
        }
        Verb::Start { name } => match declared(name) {
            Ok(decl) => place_said(name, start(decl, plane_dir, workdir)),
            Err(text) => refused(text),
        },
        Verb::Stop { name } => match declared(name) {
            Ok(decl) => place_said(name, stop(decl, plane_dir)),
            Err(text) => refused(text),
        },
        Verb::Status { name } => match declared(name) {
            Ok(decl) => place_said(name, place(decl, plane_dir)),
            Err(text) => refused(text),
        },
        Verb::Reset { name } => match declared(name) {
            Ok(decl) => {
                let file = journal::path(plane_dir, name);
                if let Err(error) = journal::reset(plane_dir, name) {
                    return Said::fault(&error, lang);
                }
                Said::told(Told::Notice(
                    tr!(
                        lang,
                        m::FLOW_RESET,
                        name = name,
                        state = decl.initial(),
                        file = format!("{}.old", file.display())
                    )
                    .to_string(),
                ))
            }
            Err(text) => refused(text),
        },
    }
}

/// A place in the short form `status` answers with (D7): the state, what it
/// waits for, the journal's last lines. The prose stays in the journal.
pub fn said(lang: Lang, name: &str, place: &Place) -> String {
    let Some(state) = place.state.as_deref() else {
        return tr!(lang, m::FLOW_NOT_STARTED, name = name).to_string();
    };
    let mut text = tr!(lang, m::FLOW_AT, name = name, state = state).to_string();
    if place.stopped {
        text.push_str(&format!(" ({})", tr!(lang, m::FLOW_STOPPED)));
    }
    for wait in &place.waiting {
        text.push_str(&format!("\n  {} {}", tr!(lang, m::FLOW_WAITS), text_of(wait)));
    }
    for line in &place.tail {
        text.push_str(&format!("\n  {line}"));
    }
    text
}

/// A thing a state waits for, in one line (D6).
fn text_of(wait: &Waiting) -> String {
    match wait {
        Waiting::Say { session } => format!("say {session}"),
        Waiting::Review { function, events } => format!("review {function}: {}", events.join("|")),
    }
}

/// Where `decl` stands in `plane_dir`: the fold of its journal, and what its
/// state waits for (D3). No journal is a flow that has not started.
pub fn place(decl: &FlowDecl, plane_dir: &Path) -> Result<Place, Error> {
    let file = journal::path(plane_dir, decl.name());
    let entries = journal::read(&file)?;
    let Some(last) = entries.last() else {
        return Ok(Place::default());
    };
    let turn = journal::fold(decl, &file, &entries)?;
    let state = turn.state.clone();
    Ok(Place {
        waiting: state.as_deref().map_or_else(Vec::new, |name| waiting_of(decl, name)),
        tail: entries.iter().rev().take(TAIL).rev().map(short).collect(),
        visits: turn.visits,
        stopped: matches!(last, Entry::Stopped { .. }),
        state,
    })
}

/// Starts `decl` or goes on with it (D2, D3), running what its states do by
/// themselves (D5, D10). A state whose actions need a turn ends the run, and
/// the place says what it waits for (D6); the limit of the whole run stops it
/// for good, and the journal keeps that too (D33).
pub fn start(decl: &FlowDecl, plane_dir: &Path, workdir: &Path) -> Result<Place, Error> {
    let file = journal::path(plane_dir, decl.name());
    let mut run = FlowRun::resume(decl, plane_dir)?;
    // A stop is not a wall: `start` moves the flow again from where it stood,
    // and the journal says so (D7).
    if matches!(journal::read(&file)?.last(), Some(Entry::Stopped { .. })) {
        run.restart()?;
    }
    for _ in 0..MAX_WALK {
        let at = index(decl, run.state());
        // The state's actions in the order they are declared; the first event
        // that moves the machine leaves the rest of them behind (D9).
        let mut moved = false;
        for action in decl.actions(at) {
            let event = match action {
                Action::Command { cmd, timeout } => {
                    Some(Event::Exit(act::run(workdir, cmd, *timeout)?))
                }
                Action::Launch { cmd, timeout } => {
                    Some(Event::Launch(act::run(workdir, cmd, *timeout)?))
                }
                // An artifact is pointed at; a `say` and a review are turns
                // of a session (D6, D10).
                Action::Artifact { .. } | Action::Review { .. } | Action::Say { .. } => None,
            };
            let Some(event) = event else { continue };
            // The limit of the whole run ends it where it stands (D33).
            if run.on_event(decl, event)? == Progress::StepLimit {
                return place(decl, plane_dir);
            }
            moved = run.state() != decl.state_name(at);
            if moved {
                break;
            }
        }
        if !moved {
            return place(decl, plane_dir);
        }
    }
    // The states handed the run to each other `MAX_WALK` times with no
    // outside event between: the declaration has no end, and the human has
    // to bound it (D9).
    Err(Error::PlaneFile {
        path: file,
        detail: format!(
            "`{}`: {MAX_WALK} steps in a row without an event from outside; give a state `max_visits` or the flow `max_steps_total`",
            decl.name()
        ),
    })
}

/// A human stopped the flow (D7): the journal keeps where it stood.
pub fn stop(decl: &FlowDecl, plane_dir: &Path) -> Result<Place, Error> {
    let run = FlowRun::resume(decl, plane_dir)?;
    run.stop()?;
    place(decl, plane_dir)
}

/// What the state `name` asks of a session (D6).
fn waiting_of(decl: &FlowDecl, name: &str) -> Vec<Waiting> {
    let at = index(decl, name);
    decl.actions(at)
        .iter()
        .filter_map(|action| match action {
            Action::Say { session } => Some(Waiting::Say {
                session: session.clone(),
            }),
            Action::Review { function, events } => Some(Waiting::Review {
                function: function.clone(),
                events: events.clone(),
            }),
            _ => None,
        })
        .collect()
}

/// One line of a journal in the short form `status` shows (D7).
fn short(entry: &Entry) -> String {
    match entry {
        Entry::Started { state, .. } => format!("started {state}"),
        Entry::Event { name, detail, into, .. } => match detail {
            Some(detail) => format!("{name}.{detail} -> {into}"),
            None => format!("{name} -> {into}"),
        },
        Entry::Stopped { state, .. } => format!("stopped {state}"),
    }
}

/// The index of the state `name`; a journal names only states of its flow.
fn index(decl: &FlowDecl, name: &str) -> usize {
    decl.states()
        .iter()
        .position(|s| s.name == name)
        .expect("a state of the flow")
}
