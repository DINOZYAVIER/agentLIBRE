//! The journal of one flow and the position its lines leave the machine at
//! (AGL-118, D3): the truth is the file, the position is its fold, the way a
//! turn's `Unfinished` is a fold of the session's events.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value as Json, json};

use crate::error::Error;
use crate::machine::{Event, Machine, Progress};
use crate::session::Unfinished;

use super::FlowDecl;

/// The directory of the journals in a plane's store, beside `flow.nix` (D1, D3).
pub const JOURNAL_DIR: &str = "flow";

/// Seconds since the epoch, as the session files keep time.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// A line of a flow's journal (D3). `into` is what the state's transitions
/// gave, kept so `agl plane flow status` reads one line; the fold does not
/// trust it, it recomputes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// The flow was started: the machine stands at `state`.
    Started { time: u64, state: String },
    /// The event named `name` with `detail` moved the machine to `into`.
    Event {
        time: u64,
        name: String,
        detail: Option<String>,
        into: String,
    },
    /// A human stopped the flow; it stays where it stood (D7).
    Stopped { time: u64, state: String },
}

impl Entry {
    /// The line as the journal holds it.
    fn to_line(&self) -> Json {
        match self {
            Entry::Started { time, state } => {
                json!({ "type": "started", "time": time, "state": state })
            }
            Entry::Event {
                time,
                name,
                detail,
                into,
            } => json!({
                "type": "event",
                "time": time,
                "name": name,
                "detail": detail,
                "into": into,
            }),
            Entry::Stopped { time, state } => {
                json!({ "type": "stopped", "time": time, "state": state })
            }
        }
    }

    /// The line read back; `None` for a line that is not an entry of a flow.
    fn from_line(value: &Json) -> Option<Entry> {
        let time = value["time"].as_u64()?;
        match value["type"].as_str()? {
            "started" => Some(Entry::Started {
                time,
                state: value["state"].as_str()?.into(),
            }),
            "event" => Some(Entry::Event {
                time,
                name: value["name"].as_str()?.into(),
                detail: value["detail"].as_str().map(Into::into),
                into: value["into"].as_str()?.into(),
            }),
            "stopped" => Some(Entry::Stopped {
                time,
                state: value["state"].as_str()?.into(),
            }),
            _ => None,
        }
    }

}

/// Where a flow's journal file is (D3).
pub fn path(plane_dir: &Path, flow: &str) -> PathBuf {
    plane_dir.join(JOURNAL_DIR).join(format!("{flow}.jsonl"))
}

/// Reads the journal `path`; no file is no lines (D3).
pub fn read(path: &Path) -> Result<Vec<Entry>, Error> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::Read {
            path: path.into(),
            source,
        }),
    };
    let mut entries = Vec::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let value: Json = serde_json::from_str(line).map_err(|source| Error::PlaneFile {
            path: path.into(),
            detail: source.to_string(),
        })?;
        entries.push(Entry::from_line(&value).ok_or_else(|| Error::PlaneFile {
            path: path.into(),
            detail: "not an entry of a flow".into(),
        })?);
    }
    Ok(entries)
}

/// Appends `entry` as one line, making the directory when it is not there.
fn append(path: &Path, entry: &Entry) -> Result<(), Error> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|source| Error::Write {
            path: dir.into(),
            source,
        })?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| Error::Write {
            path: path.into(),
            source,
        })?;
    let line = entry.to_line().to_string();
    writeln!(file, "{line}").map_err(|source| Error::Write {
        path: path.into(),
        source,
    })
}

/// The position the journal leaves the machine at: the fold of its lines over
/// the declaration (D3). `Session::unfinished` is the same law for a turn. A
/// declaration whose transitions no longer give what the journal recorded is
/// refused, not silently followed (D11).
pub fn fold(decl: &FlowDecl, path: &Path, entries: &[Entry]) -> Result<Unfinished, Error> {
    let index = |name: &str| {
        decl.states()
            .iter()
            .position(|s| s.name == name)
            .expect("a state of the flow")
    };
    // The fold follows the flow's last run: a `started` line begins one, and
    // a `stopped` line only marks where a human left the flow (D3, D7).
    let from = entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::Started { .. }));
    let first = match from.map(|at| &entries[at]) {
        Some(Entry::Started { state, .. }) => index(state),
        _ => decl.table().initial,
    };
    let mut at = first;
    let mut visits: Vec<(String, u32)> = vec![(decl.state_name(first).into(), 1)];
    let mut steps_in_visit = 0;
    for entry in entries.iter().skip(from.map_or(0, |at| at + 1)) {
        match entry {
            Entry::Event {
                name,
                detail,
                into,
                ..
            } => {
                // An event the state has no transition for moves nothing (D9).
                let next = decl.states()[at]
                    .next_option(name, detail.as_deref())
                    .unwrap_or(at);
                if decl.state_name(next) != into.as_str() {
                    return Err(Error::PlaneFile {
                        path: path.into(),
                        detail: format!(
                            "`{name}` in `{}` gives `{}`, the journal recorded `{into}`",
                            decl.state_name(at),
                            decl.state_name(next)
                        ),
                    });
                }
                at = next;
                let name = decl.state_name(at);
                match visits.iter_mut().find(|(seen, _)| seen == name) {
                    Some((_, count)) => *count += 1,
                    None => visits.push((name.into(), 1)),
                };
                steps_in_visit = 0;
            }
            // A `stopped` line names a state without moving it; a `started`
            // after it belongs to the run this fold follows (D7).
            Entry::Started { .. } | Entry::Stopped { .. } => {}
        }
    }
    Ok(Unfinished {
        state: Some(decl.state_name(at).into()),
        visits,
        steps_in_visit,
    })
}

/// D11's way out: the journal is put aside as `<name>.jsonl.old`, so the flow
/// starts again from its `initial` and the history stays readable beside it.
/// A later reset replaces that file; no journal is nothing to reset.
pub fn reset(plane_dir: &Path, flow: &str) -> Result<(), Error> {
    let file = path(plane_dir, flow);
    if !file.is_file() {
        return Ok(());
    }
    let old = PathBuf::from(format!("{}.old", file.display()));
    fs::rename(&file, &old).map_err(|source| Error::Write {
        path: old,
        source,
    })
}

/// A flow that runs: its machine, and the journal its steps are written to
/// (D3). It borrows the declaration, which the plane holds.
#[derive(Debug)]
pub struct FlowRun<'d> {
    path: PathBuf,
    machine: Machine<'d>,
}

impl<'d> FlowRun<'d> {
    /// Starts `decl` fresh, and says so in the journal (D3).
    pub fn start(decl: &'d FlowDecl, plane_dir: &Path) -> Result<FlowRun<'d>, Error> {
        let path = path(plane_dir, decl.name());
        let machine = Machine::new(decl.table());
        let state = machine.state().name.clone();
        append(&path, &Entry::Started { time: now(), state })?;
        Ok(FlowRun { path, machine })
    }

    /// Finds `decl` where its journal left it; no journal starts it (D3).
    pub fn resume(decl: &'d FlowDecl, plane_dir: &Path) -> Result<FlowRun<'d>, Error> {
        let file = path(plane_dir, decl.name());
        let entries = read(&file)?;
        if entries.is_empty() {
            return Self::start(decl, plane_dir);
        }
        let turn = fold(decl, &file, &entries)?;
        let machine = Machine::resume(decl.table(), decl.name(), &turn)?;
        Ok(FlowRun {
            path: file,
            machine,
        })
    }

    /// Where the machine stands, by name: what `agl plane flow status` shows.
    pub fn state(&self) -> &str {
        &self.machine.state().name
    }

    /// The state each line of the journal names, oldest first: the same fold
    /// the machine runs on, for `agl plane flow status` and its tests (D3).
    pub fn trail(plane_dir: &Path, decl: &FlowDecl) -> Result<Vec<String>, Error> {
        let entries = read(&path(plane_dir, decl.name()))?;
        let states = decl.states();
        let mut at = decl.initial().to_string();
        let mut trail = Vec::new();
        for entry in &entries {
            match entry {
                // A `started` line begins a run of the flow, the first or a
                // new one after a stop (D7).
                Entry::Started { state, .. } => {
                    at = state.clone();
                    trail.push(at.clone());
                }
                Entry::Event { name, detail, .. } => {
                    let index = states
                        .iter()
                        .position(|s| s.name == at)
                        .expect("a state of the flow");
                    at = states[index]
                        .next_option(name, detail.as_deref())
                        .map_or(at.clone(), |next| decl.state_name(next).into());
                    trail.push(at.clone());
                }
                // A stopped line marks where the human left it; it moves
                // nothing (D7).
                Entry::Stopped { .. } => {}
            }
        }
        Ok(trail)
    }

    /// Writes `event` and moves, recording where it landed (D3).
    pub fn on_event(&mut self, decl: &FlowDecl, event: Event) -> Result<Progress, Error> {
        // The key is taken before the event moves into the machine (D4).
        let (name, detail) = {
            let (name, detail) = event.key();
            (name.to_string(), detail.map(str::to_string))
        };
        // An event the state has no transition for is written and moves
        // nothing: the machine of a turn is complete by loading (D21), a
        // flow's state answers only what its declaration covers (D9).
        let at = decl
            .states()
            .iter()
            .position(|s| s.name == self.state())
            .expect("a state of the flow");
        if decl.states()[at]
            .next_option(&name, detail.as_deref())
            .is_none()
        {
            append(
                &self.path,
                &Entry::Event {
                    time: now(),
                    name,
                    detail,
                    into: self.state().into(),
                },
            )?;
            return Ok(Progress::Continue);
        }
        let progress = self.machine.after_event(event);
        let into = self.machine.state().name.clone();
        append(
            &self.path,
            &Entry::Event {
                time: now(),
                name,
                detail,
                into,
            },
        )?;
        Ok(progress)
    }

    /// A stopped flow goes on (D7): a `started` line for the state it stood
    /// in says the human moved it again, and the counts of its visits begin
    /// anew, as they do for a machine that starts over (D44).
    pub fn restart(&mut self) -> Result<(), Error> {
        append(
            &self.path,
            &Entry::Started {
                time: now(),
                state: self.state().into(),
            },
        )
    }

    /// Says a human stopped the flow (D7): the journal keeps where it stood.
    pub fn stop(&self) -> Result<(), Error> {
        append(
            &self.path,
            &Entry::Stopped {
                time: now(),
                state: self.state().into(),
            },
        )
    }
}
