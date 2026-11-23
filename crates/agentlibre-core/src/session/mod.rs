//! Sessions: an append-only JSONL log of a conversation that can be
//! resumed (D30, D31, D36, D49, D104-D108, D139-D141).

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value as Json, json};

use agentlibre_common::clock::utc_now;

use crate::action::{Outcome, Value};
use crate::backend::{Message, ToolCall};
use crate::error::Error;
use error::{Error as SessionError, NameProblem};
use crate::media::{self, Frame, Kind, Media, Source};
use agentlibre_forge::memory::Kind as MemoryKind;

/// The session format version (D104, AGL-063, D2): the v1 files open as
/// they are, and a new event line type or a changed field meaning bumps
/// it with the migration.
const VERSION: u64 = 2;

/// The oldest version the reader of this build reads (AGL-063, D2).
const OLDEST_VERSION: u64 = 1;

/// The result of a call cut off before it finished (D108, AGL-063, D6).
pub const INTERRUPTED: &str = "The human interrupted before the result.";

/// Why a turn stopped without an answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// `max_steps_total` was reached (D33).
    StepLimit,
    /// The context would overflow (D32).
    ContextFull,
    /// llama-server failed (D97).
    ServerError,
    /// The user interrupted the turn.
    Interrupted,
    /// The model's output was not an action of the state after the stirs
    /// (AGL-030, D5).
    StirLimit,
}

impl StopReason {
    const ALL: [StopReason; 5] = [
        StopReason::StepLimit,
        StopReason::ContextFull,
        StopReason::ServerError,
        StopReason::Interrupted,
        StopReason::StirLimit,
    ];

    pub fn name(self) -> &'static str {
        match self {
            StopReason::StepLimit => "step_limit",
            StopReason::ContextFull => "context_full",
            StopReason::ServerError => "server_error",
            StopReason::Interrupted => "interrupted",
            StopReason::StirLimit => "stir_limit",
        }
    }
}

/// The kind of the agent's write to the forge (AGL-008): the word of
/// the `[forge: …]` marker line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForgeKind {
    /// `forge new` created a task-spec.
    Created,
    /// `forge edit` changed the draft.
    Edited,
    /// `forge append` added a bullet under a section.
    Appended,
    /// `forge remove` deleted the draft.
    Removed,
    /// `forge status` moved a task-spec.
    StatusChanged,
    /// `forge rename` renamed a task-spec (D6, AGL-032): the new name
    /// is the event's, the old one rides the section slot.
    Renamed,
}

impl ForgeKind {
    const ALL: [ForgeKind; 6] = [
        ForgeKind::Created,
        ForgeKind::Edited,
        ForgeKind::Appended,
        ForgeKind::Removed,
        ForgeKind::StatusChanged,
        ForgeKind::Renamed,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ForgeKind::Created => "created",
            ForgeKind::Edited => "edited",
            ForgeKind::Appended => "appended",
            ForgeKind::Removed => "removed",
            ForgeKind::StatusChanged => "status-changed",
            ForgeKind::Renamed => "renamed",
        }
    }
}

/// What a cut-off discarded (D154; AGL-043 F8).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CutOff {
    /// The budget of the generation: `max_action_tokens`, with the
    /// thinking's budget in the stirred mode, where the thinking and the
    /// action are one generation.
    pub limit: u32,
    /// The tokens the thinking took of `limit` (the stirred mode).
    pub thinking: Option<u32>,
    /// The tokens of the discarded action.
    pub discarded: u32,
    /// Whether the discarded action was heading for a tool call.
    pub call: bool,
}

/// A slash command the human ran (AGL-047, D5): the line, whether it failed
/// and what it said.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandRun {
    pub line: String,
    pub failed: bool,
    pub output: Option<String>,
}

impl CommandRun {
    /// What the model reads of it (AGL-047, D5), when anything: a failure in
    /// full, so that it can help; of `/help`, `/status` and `/memory list|show`
    /// the fact alone, their output being no use in the context (AGL-060, D6);
    /// `/quit`, `/exit` and `/new` leave the session. The prefix is added where
    /// the message is rendered.
    fn message(&self) -> Option<String> {
        let name = self.line.split_whitespace().next().unwrap_or(&self.line);
        let ran = format!("the human ran: {}", self.line);
        let output = self.output.as_deref().filter(|output| !output.is_empty());
        if self.failed {
            return Some(match output {
                Some(output) => format!("{ran}\nfailed: {output}"),
                None => format!("{ran}\nfailed"),
            });
        }
        match name {
            "/quit" | "/exit" | "/new" => None,
            "/help" | "/status" => Some(ran),
            "/memory" if self.line.split_whitespace().nth(1) != Some("remove") => Some(ran),
            _ => Some(match output {
                Some(output) => format!("{ran}\n{output}"),
                None => ran,
            }),
        }
    }
}

/// A `transition` line of the session file (AGL-066): one phase of a step
/// and how long it took, written by the observer (AGL-046). The reader
/// keeps the lines, in the order of the file: the compact frame and the
/// inspect mode take the phases' times from them.
#[derive(Clone, Debug, PartialEq)]
pub struct TransitionLine {
    /// The turn the step is in, 1-based.
    pub turn: usize,
    /// The step of the turn, in the order of its steps.
    pub step: usize,
    /// The phase left, as [`crate::phase::Phase::name`].
    pub from: String,
    /// The phase entered, as [`crate::phase::Phase::name`].
    pub to: String,
    /// What moved the turn, as [`crate::phase::PhaseEvent::name`].
    pub event: String,
    /// The time spent in `from`, in milliseconds.
    pub ms: u64,
    /// The tokens decoded since the last transition, when the line holds
    /// them.
    pub tokens: Option<u64>,
    /// The tool a `tool_start` names.
    pub tool: Option<String>,
}

/// A session event (D104). The header line is not an event.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    User(String),
    /// A flow's wake delivered to this session (AGL-121, D44, D55): the
    /// flow that woke and the line it sent. It opens a turn as a human's
    /// message does, and the model takes it behind the runtime prefix;
    /// the record keeps who spoke.
    Wake {
        flow: String,
        text: String,
    },
    /// A visit of a state begins.
    State(String),
    Thinking(String),
    /// A call and the text written before it (D114).
    Call {
        text: Option<String>,
        call: ToolCall,
    },
    Result {
        id: String,
        outcome: Outcome,
        content: String,
    },
    /// A write of the agent's forge tool (AGL-008): the artifact's
    /// name, the kind of the write and, for an append or a status
    /// change, the section.
    Forge {
        name: String,
        kind: ForgeKind,
        section: Option<String>,
    },
    Answer(String),
    /// A generation hit its token budget and was discarded (D154;
    /// AGL-043 F8).
    CutOff(CutOff),
    /// A stir of the stirred mode: the model's output was not an action of
    /// the state, the machine nudged it and retried (AGL-030, D4). `attempt`
    /// is the stir number of the step (1-based) and `error` the parse error.
    Stir {
        attempt: u32,
        error: String,
    },
    Stop(StopReason),
    /// A write of the `memory` tool or of `/remember` (AGL-006, M36): the
    /// entry, what was done and the text that was written.
    Memory {
        name: String,
        kind: MemoryKind,
        text: Option<String>,
    },
    /// A medium of the session, logged before the message that names it
    /// (AGL-014); its bytes are in the media directory (D7).
    Media(Media),
    /// A summary that replaces the history before it (D139). `question` is
    /// the user message that started the turn, kept after the summary so
    /// the turn goes on from the summary and the question together and the
    /// summary need not repeat it (F31); absent in sessions written before
    /// the field.
    Compact {
        summary: String,
        question: Option<String>,
    },
    /// A slash command the human ran (AGL-047, D5).
    Command(CommandRun),
    /// A line the program said about itself while the session ran
    /// (AGL-103, D1): the server's lines, a failed start. `text` is as
    /// rendered at emission; `failed` is what makes it red.
    System {
        text: String,
        failed: bool,
    },
}

/// Where an unfinished turn continues (D105).
#[derive(Debug, PartialEq, Eq)]
pub struct Unfinished {
    /// The state of the last visit; `None` before the first one.
    pub state: Option<String>,
    /// Visits of each state in the turn, in order of the first visit.
    pub visits: Vec<(String, u32)>,
    /// Calls in the last visit.
    pub steps_in_visit: u32,
}

/// An open session file.
/// The plane a session is in, as its header records it (AGL-077, D10,
/// D13, D18): its id, and its name when the session was made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionPlane {
    pub id: String,
    pub name: String,
}

pub struct Session {
    pub id: String,
    /// The name the human gave it, the last `name` line of the file
    /// (AGL-062, D4).
    pub name: Option<String>,
    pub function: String,
    pub workspace: PathBuf,
    pub events: Vec<Event>,
    /// The `transition` lines of the file, in its order (AGL-066): the
    /// phases' times of the steps.
    pub transitions: Vec<TransitionLine>,
    /// The `run` snapshot of the header (AGL-063, D4); `None` in the v1
    /// files and in the sessions that do not record it.
    pub run: Option<Json>,
    /// The plane of the session (AGL-077, D10); `None` for a session in no
    /// plane.
    pub plane: Option<SessionPlane>,
    /// The `think_budget` and `action_budget` of the last `step` line
    /// (AGL-063, D4): the budgets the loose continue runs on; `None` before
    /// a step was recorded.
    pub last_budgets: Option<(u32, u32)>,
    path: PathBuf,
    file: File,
    /// What the prompt was last made of, by kind (AGL-046, D14).
    prompt_inputs: BTreeMap<String, String>,
    /// The memory index as this run of the session read it; `None` before
    /// that (AGL-047, D2).
    memory_index: Option<Option<String>>,
}

impl Session {
    /// Starts a session in `dir`, created if needed (D64).
    pub fn create(dir: &Path, function: &str, workspace: &Path) -> Result<Session, Error> {
        Self::create_run(dir, function, workspace, None)
    }

    /// Starts a session that records the `run` snapshot of the evaluated
    /// Function in its header (AGL-063, D4): a resume runs on the
    /// configuration the session was made with, the Function file not
    /// needed.
    pub fn create_run(
        dir: &Path,
        function: &str,
        workspace: &Path,
        run: Option<&Json>,
    ) -> Result<Session, Error> {
        Self::create_in(dir, function, workspace, run, None)
    }

    /// [`Session::create_run`] in the plane `plane` (AGL-077, D10, D18): its
    /// header records the plane's id and name.
    pub fn create_in(
        dir: &Path,
        function: &str,
        workspace: &Path,
        run: Option<&Json>,
        plane: Option<&SessionPlane>,
    ) -> Result<Session, Error> {
        let write_error = |path: &Path, source| SessionError::Write {
            path: path.into(),
            source,
        };
        // A session holds what the commands printed: the user's alone
        // (AGL-058, D19).
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|source| write_error(dir, source))?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
            .map_err(|source| write_error(dir, source))?;
        let id = uuid_v7().map_err(|source| write_error(dir, source))?;
        let path = dir.join(format!("{id}.jsonl"));
        let file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|source| write_error(&path, source))?;
        let mut session = Session {
            id,
            name: None,
            function: function.into(),
            workspace: workspace.into(),
            events: Vec::new(),
            transitions: Vec::new(),
            run: run.cloned(),
            plane: plane.cloned(),
            last_budgets: None,
            path,
            file,
            prompt_inputs: BTreeMap::new(),
            memory_index: None,
        };
        let mut header = json!({
            "type": "session",
            "version": VERSION,
            "function": function,
            "workspace": workspace.to_string_lossy(),
            "time": utc_now(),
        });
        // The `run` snapshot (AGL-063, D4), absent in the v1 files and in
        // the sessions that do not record it.
        if let Some(run) = run {
            header["run"] = run.clone();
        }
        if let Some(plane) = plane {
            header["plane"] = json!({ "id": plane.id, "name": plane.name });
        }
        session.write_line(&header)?;
        Ok(session)
    }

    /// The latest sessions in `dir` of the project at `project`, the newest
    /// first, at most `limit` (AGL-058, D20). A file that cannot be read is left
    /// out: this only lists.
    pub fn latest(dir: &Path, project: &Path, limit: usize) -> Vec<SessionInfo> {
        session_files(dir)
            .iter()
            .rev()
            .filter_map(|path| session_info(path, project))
            .take(limit)
            .collect()
    }

    /// The latest sessions in `dir` of the plane whose id is `plane`, the
    /// newest first, at most `limit` (AGL-077, D13).
    pub fn latest_of_plane(dir: &Path, plane: &str, limit: usize) -> Vec<SessionInfo> {
        session_files(dir)
            .iter()
            .rev()
            .filter_map(|path| session_info_if(path, |header| header["plane"]["id"] == plane))
            .take(limit)
            .collect()
    }

    /// [`Session::id_named`] among the sessions of the plane `plane`.
    pub fn id_named_of_plane(dir: &Path, plane: &str, name: &str) -> Option<String> {
        session_files(dir)
            .iter()
            .rev()
            .filter_map(|path| session_info_if(path, |header| header["plane"]["id"] == plane))
            .find(|info| info.name.as_deref() == Some(name))
            .map(|info| info.id)
    }

    /// How many sessions in `dir` are of the plane whose id is `plane`
    /// (AGL-077, D13, D18): their header names it.
    pub fn count_of_plane(dir: &Path, plane: &str) -> usize {
        session_files(dir)
            .iter()
            .filter(|path| {
                File::open(path)
                    .ok()
                    .and_then(|file| std::io::BufReader::new(file).lines().next()?.ok())
                    .and_then(|line| serde_json::from_str::<Json>(&line).ok())
                    .is_some_and(|header| header["plane"]["id"] == plane)
            })
            .count()
    }

    /// The id of the newest session of the project at `project` whose name is
    /// `name`, as it was typed (AGL-062, D1).
    pub fn id_named(dir: &Path, project: &Path, name: &str) -> Option<String> {
        session_files(dir)
            .iter()
            .rev()
            .filter_map(|path| session_info(path, project))
            .find(|info| info.name.as_deref() == Some(name))
            .map(|info| info.id)
    }

    /// Gives the session the name `name` (AGL-062, D4-D6): a line at the end of
    /// the file, and the last one counts. A name that another session of the
    /// project at `project` has is refused; the session's own name is no change.
    pub fn rename(&mut self, project: &Path, name: &str) -> Result<(), Error> {
        let name = check_name(name).map_err(|problem| SessionError::Name { problem })?;
        if self.name.as_deref() == Some(name) {
            return Ok(());
        }
        let dir = self.path.parent().unwrap_or(Path::new("."));
        let taken = session_files(dir)
            .iter()
            .filter_map(|path| session_info(path, project))
            .find(|info| info.name.as_deref() == Some(name) && info.id != self.id);
        if let Some(other) = taken {
            return Err(SessionError::NameTaken {
                name: name.into(),
                id: other.id,
            }
            .into());
        }
        self.write_line(&json!({
            "type": "name",
            "schema": 1,
            "name": name,
            "time": utc_now(),
        }))?;
        self.name = Some(name.into());
        Ok(())
    }

    /// Opens the session whose id is or starts with `id` (D107). A call
    /// left without a result gets one (D108).
    pub fn open(dir: &Path, id: &str) -> Result<Session, Error> {
        let path = find(dir, id)?;
        let read_error = |source| agentlibre_common::error::Error::Read {
            path: path.clone(),
            source,
        };
        let mut text = String::new();
        File::open(&path)
            .and_then(|mut file| file.read_to_string(&mut text))
            .map_err(read_error)?;
        // A crash may leave half a line at the end; it is not part of the
        // session.
        let complete = match text.rfind('\n') {
            Some(end) => &text[..=end],
            None => "",
        };
        let mut lines = complete.lines().enumerate();
        let bad = |line: usize, detail: &str| {
            Error::from(SessionError::Line {
                path: path.clone(),
                line: line + 1,
                detail: detail.into(),
            })
        };
        let (_, header) = lines.next().ok_or_else(|| bad(0, "no header"))?;
        let header: Json = serde_json::from_str(header).map_err(|e| bad(0, &e.to_string()))?;
        if header["type"] != "session" {
            return Err(bad(0, "the first line is not a session header"));
        }
        // The versions this build reads (AGL-063, D2): v1 as it is, v2
        // with the `run` field of the header and the `interrupted`
        // outcome; anything else is named.
        if !header["version"].as_u64().is_some_and(|v| (OLDEST_VERSION..=VERSION).contains(&v)) {
            let version = header["version"]
                .as_u64()
                .map_or_else(|| "unknown".to_string(), |v| v.to_string());
            return Err(bad(0, &format!("unsupported session version {version}")));
        }
        let (Some(function), Some(workspace)) =
            (header["function"].as_str(), header["workspace"].as_str())
        else {
            return Err(bad(0, "the header needs function and workspace"));
        };
        let mut events = Vec::new();
        let mut transitions = Vec::new();
        let mut prompt_inputs = BTreeMap::new();
        let mut name = None;
        // The budgets of the last `step` line (AGL-063, D4): the loose
        // continue of a session whose Function is not found runs on them.
        let mut last_budgets = None;
        for (index, line) in lines {
            let json: Json = serde_json::from_str(line).map_err(|e| bad(index, &e.to_string()))?;
            // What the observer wrote is not an event, so it can reach
            // neither the history nor the replay (AGL-046).
            match json["type"].as_str() {
                Some("step") => {
                    // The budgets the step ran on, the last one counts.
                    if let (Some(think), Some(action)) =
                        (json["think_budget"].as_u64(), json["action_budget"].as_u64())
                    {
                        last_budgets = Some((think as u32, action as u32));
                    }
                    continue;
                }
                Some("transition") => {
                    // The phases' times of the compact frame (AGL-066); a
                    // line without its numbers is left out.
                    if let Some(line) = transition_from_json(&json) {
                        transitions.push(line);
                    }
                    continue;
                }
                Some("prefill" | "prompt_progress") => continue,
                // The last name counts (AGL-062, D4); it is no event either.
                Some("name") => {
                    name = json["name"].as_str().map(str::to_owned).or(name);
                    continue;
                }
                Some("prompt_input") => {
                    if let (Some(kind), Some(text)) = (json["kind"].as_str(), json["text"].as_str())
                    {
                        prompt_inputs.insert(kind.to_owned(), text.to_owned());
                    }
                    continue;
                }
                _ => {}
            }
            // The line is not an event this reader knows (AGL-063, D2):
            // the number and the type of the line are named, so the file
            // is recognized as one of a newer version.
            events.push(
                event_from_json(&json).ok_or_else(|| {
                    let type_name = json["type"].as_str().unwrap_or("missing");
                    bad(index, &format!("unknown event of type {type_name}"))
                })?,
            );
        }
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|source| SessionError::Write {
                path: path.clone(),
                source,
            })?;
        let mut session = Session {
            id: path.file_stem().unwrap().to_string_lossy().into_owned(),
            name,
            function: function.into(),
            workspace: workspace.into(),
            events,
            transitions,
            run: header.get("run").cloned(),
            plane: match (header["plane"]["id"].as_str(), header["plane"]["name"].as_str()) {
                (Some(id), Some(name)) => Some(SessionPlane {
                    id: id.into(),
                    name: name.into(),
                }),
                _ => None,
            },
            last_budgets,
            path,
            file,
            prompt_inputs,
            memory_index: None,
        };
        if complete.len() < text.len() {
            // Drop the half line so that new events start on their own line.
            session
                .file
                .set_len(complete.len() as u64)
                .map_err(|source| SessionError::Write {
                    path: session.path.clone(),
                    source,
                })?;
        }
        if let Some(Event::Call { call, .. }) = session.events.last() {
            // A call the human cut off before its result is an
            // interruption, not a failure (D108, AGL-063, D6).
            let id = call.id.clone();
            session.append(Event::Result {
                id,
                outcome: Outcome::Interrupted,
                content: INTERRUPTED.into(),
            })?;
        }
        Ok(session)
    }

    /// The tools the session's record calls, in the order of the first call
    /// (AGL-063, D4): the tools of the loose continue of a session whose
    /// Function is not found.
    pub fn loose_tools(&self) -> Vec<String> {
        let mut tools = Vec::new();
        for event in &self.events {
            if let Event::Call { call, .. } = event
                && !tools.iter().any(|tool| tool == &call.name)
            {
                tools.push(call.name.clone());
            }
        }
        tools
    }

    /// Appends `event` to the file at once and to `events`.
    pub fn append(&mut self, event: Event) -> Result<(), Error> {
        let mut json = event_json(&event);
        json["time"] = utc_now().into();
        self.write_line(&json)?;
        self.events.push(event);
        Ok(())
    }

    /// Records a part of what the prompt is made of when it changed
    /// (AGL-046, D14). It is no event: the history does not hold it.
    pub fn record_input(&mut self, kind: &str, text: &str) -> Result<(), Error> {
        let known = self.prompt_inputs.get(kind).map_or("", String::as_str);
        if known == text {
            return Ok(());
        }
        self.write_line(&json!({
            "type": "prompt_input",
            "schema": 1,
            "kind": kind,
            "text": text,
            "time": utc_now(),
        }))?;
        self.prompt_inputs.insert(kind.into(), text.into());
        Ok(())
    }

    /// The memory index for the prompt, read once by `read` (AGL-047, D2):
    /// a change in the prompt's early part makes the server read all of it
    /// again.
    pub fn memory_index(&mut self, read: impl FnOnce() -> Option<String>) -> Option<String> {
        self.memory_index.get_or_insert_with(read).clone()
    }

    /// The next prompt reads the index anew: after a compaction, when the
    /// cache is lost anyway (AGL-047, D2).
    pub fn renew_memory_index(&mut self) {
        self.memory_index = None;
    }

    /// The directory where the whole output of the calls is kept, beside
    /// the session's file (AGL-049, D2).
    pub fn outputs_dir(&self) -> PathBuf {
        let dir = self.path.parent().map_or_else(PathBuf::new, Path::to_path_buf);
        dir.join("outputs").join(&self.id)
    }

    /// Where the whole output of a call is kept (AGL-049, D2).
    pub fn output_file(&self, call_id: &str) -> PathBuf {
        self.outputs_dir().join(format!("{call_id}.txt"))
    }

    /// The session's file (AGL-048).
    pub fn file_path(&self) -> &Path {
        &self.path
    }

    /// A handle on the session file for the observer of a turn (AGL-046).
    /// The file is opened to append, so the lines of both stay whole.
    pub fn log_file(&self) -> Option<File> {
        self.file.try_clone().ok()
    }

    /// The turns of the session: its user messages.
    pub fn turns(&self) -> usize {
        self.events
            .iter()
            .filter(|event| matches!(event, Event::User(_) | Event::Wake { .. }))
            .count()
    }

    fn write_line(&mut self, json: &Json) -> Result<(), Error> {
        let line = format!("{json}\n");
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.flush())
            .map_err(|source| SessionError::Write {
                path: self.path.clone(),
                source,
            })?;
        Ok(())
    }

    /// The conversation for the chat template, without the system prompt:
    /// the last summary as a runtime message, then what followed it
    /// (D139, D141). Thinking that no call or answer followed is left
    /// out.
    pub fn history(&self) -> Vec<Message> {
        let mut messages = Vec::new();
        let mut thinking = None;
        for event in &self.events {
            match event {
                Event::Compact { summary, question } => {
                    messages.clear();
                    thinking = None;
                    messages.push(Message::Runtime(summary.clone()));
                    if let Some(question) = question {
                        messages.push(Message::User(question.clone()));
                    }
                }
                Event::User(text) => {
                    thinking = None;
                    messages.push(Message::User(text.clone()));
                }
                Event::Wake { text, .. } => {
                    thinking = None;
                    messages.push(Message::Wake(text.clone()));
                }
                Event::Thinking(text) => thinking = Some(text.clone()),
                Event::Call { text, call } => messages.push(Message::Assistant {
                    thinking: thinking.take(),
                    text: text.clone(),
                    call: Some(call.clone()),
                }),
                Event::Result { id, content, .. } => messages.push(Message::Tool {
                    id: id.clone(),
                    content: content.clone(),
                }),
                Event::Answer(text) => messages.push(Message::Assistant {
                    thinking: thinking.take(),
                    text: Some(text.clone()),
                    call: None,
                }),
                Event::CutOff(cut) => {
                    thinking = None;
                    messages.push(Message::Runtime(cut_off_notice(cut)));
                }
                Event::Command(run) => {
                    if let Some(text) = run.message() {
                        messages.push(Message::Runtime(text));
                    }
                }
                Event::State(_)
                | Event::Stop(_)
                | Event::Forge { .. }
                | Event::Media(_)
                | Event::Memory { .. }
                | Event::Stir { .. }
                | Event::System { .. } => {}
            }
        }
        messages
    }

    /// The directory of the media of the sessions, next to their files
    /// (AGL-014, D7).
    pub fn media_dir(&self) -> PathBuf {
        self.path
            .parent()
            .map_or_else(|| PathBuf::from("media"), |dir| dir.join("media"))
    }

    /// The media of the session, in the order they were attached.
    pub fn media(&self) -> Vec<&Media> {
        self.events
            .iter()
            .filter_map(|event| match event {
                Event::Media(media) => Some(media),
                _ => None,
            })
            .collect()
    }

    /// The number the next medium of `kind` gets (D21).
    pub fn next_media_number(&self, kind: Kind) -> u32 {
        self.media().iter().filter(|m| m.kind == kind).count() as u32 + 1
    }

    /// Keeps `source` as the medium `number` of its kind: copies its
    /// bytes (and the frames of a video) to the media directory and logs
    /// it (AGL-014, D7, D8). `transcript` is what whisper heard (D22).
    pub fn store_media(
        &mut self,
        source: &Source,
        number: u32,
        transcript: Option<String>,
    ) -> Result<Media, Error> {
        let dir = self.media_dir();
        let sha256 = media::put(&dir, &source.bytes, &source.ext)?;
        let mut frames = Vec::new();
        if source.kind == Kind::Video {
            for (at, jpeg) in media::video_frames(&dir.join(format!("{sha256}.{}", source.ext)))? {
                frames.push(Frame {
                    at,
                    sha256: media::put(&dir, &jpeg, "jpg")?,
                });
            }
        }
        let kept = Media {
            kind: source.kind,
            number,
            sha256,
            ext: source.ext.clone(),
            name: source.name.clone(),
            transcript,
            frames,
        };
        self.append(Event::Media(kept.clone()))?;
        Ok(kept)
    }

    /// Whether the history holds more than the last summary.
    pub fn compactable(&self) -> bool {
        self.events
            .iter()
            .rev()
            .take_while(|e| !matches!(e, Event::Compact { .. }))
            .any(|e| {
                matches!(
                    e,
                    Event::User(_)
                        | Event::Wake { .. }
                        | Event::Call { .. }
                        | Event::Result { .. }
                        | Event::Answer(_)
                        | Event::CutOff(_)
                )
            })
    }

    /// The last turn if it has not ended with an answer (D105). A turn
    /// starts with the first user message after the last answer.
    pub fn unfinished(&self) -> Option<Unfinished> {
        let start = self
            .events
            .iter()
            .rposition(|e| matches!(e, Event::Answer(_)))
            .map_or(0, |i| i + 1);
        let turn = &self.events[start..];
        if !turn.iter().any(|e| matches!(e, Event::User(_) | Event::Wake { .. })) {
            return None;
        }
        let mut unfinished = Unfinished {
            state: None,
            visits: Vec::new(),
            steps_in_visit: 0,
        };
        for event in turn {
            match event {
                Event::State(name) => {
                    match unfinished.visits.iter_mut().find(|(n, _)| n == name) {
                        Some((_, count)) => *count += 1,
                        None => unfinished.visits.push((name.clone(), 1)),
                    }
                    unfinished.state = Some(name.clone());
                    unfinished.steps_in_visit = 0;
                }
                Event::Result { .. } | Event::CutOff(_) => unfinished.steps_in_visit += 1,
                _ => {}
            }
        }
        Some(unfinished)
    }

    /// The next call id: `c1`, `c2`, …
    pub fn next_call_id(&self) -> String {
        let calls = self
            .events
            .iter()
            .filter(|e| matches!(e, Event::Call { .. }))
            .count();
        format!("c{}", calls + 1)
    }
}

/// A session as `agl --resume` lists it.
#[derive(Debug, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub time: String,
    /// The first line of its first message, short.
    pub first: String,
    /// Its name, when it has one (AGL-062).
    pub name: Option<String>,
}

impl SessionInfo {
    /// What the list shows of the session: its name, else the first line of
    /// its first message (AGL-062, D3).
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.first)
    }
}

/// The most of a first message `agl --resume` shows.
const FIRST_SHOWN: usize = 70;

/// The longest name (AGL-062, D6), in characters.
pub const NAME_MAX: usize = 60;

/// The word that `--resume` reads as the newest session, so no name
/// (AGL-062, D6).
const LAST: &str = "last";

/// The name a human typed, trimmed, when it is one (AGL-062, D6).
pub fn check_name(name: &str) -> Result<&str, NameProblem> {
    let name = name.trim();
    if name.is_empty() {
        Err(NameProblem::Empty)
    } else if name.contains(['\n', '\r']) {
        Err(NameProblem::Lines)
    } else if name.chars().count() > NAME_MAX {
        Err(NameProblem::TooLong)
    } else if name == LAST {
        Err(NameProblem::Reserved)
    } else {
        Ok(name)
    }
}

/// The session files of `dir`, the oldest first: a file's name sorts by time.
fn session_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    paths.sort();
    paths
}

/// The session at `path`, when it is of the project at `project`. It reads the
/// whole file for the last name, parsing only the lines that can be the first
/// message or a name (AGL-062, D8).
fn session_info(path: &Path, project: &Path) -> Option<SessionInfo> {
    session_info_if(path, |header| {
        header["workspace"]
            .as_str()
            .is_some_and(|workspace| Path::new(workspace).starts_with(project))
    })
}

/// The session at `path`, when its header is `of` (AGL-077: a plane's).
fn session_info_if(path: &Path, of: impl Fn(&Json) -> bool) -> Option<SessionInfo> {
    let mut lines = std::io::BufReader::new(File::open(path).ok()?).lines();
    let header: Json = serde_json::from_str(&lines.next()?.ok()?).ok()?;
    if header["type"] != "session" || !of(&header) {
        return None;
    }
    let mut first: Option<String> = None;
    let mut name = None;
    for line in lines.map_while(Result::ok) {
        if first.is_none() && line.contains(r#""type":"user""#) {
            if let Ok(json) = serde_json::from_str::<Json>(&line)
                && json["type"] == "user"
            {
                first = Some(json["text"].as_str().unwrap_or_default().to_owned());
            }
        } else if line.contains(r#""type":"name""#)
            && let Ok(json) = serde_json::from_str::<Json>(&line)
            && json["type"] == "name"
        {
            name = json["name"].as_str().map(str::to_owned).or(name);
        }
    }
    let first = first.unwrap_or_default();
    let first = first.lines().next().unwrap_or_default();
    let mut shown: String = first.chars().take(FIRST_SHOWN).collect();
    if first.chars().count() > FIRST_SHOWN {
        shown.push('…');
    }
    Some(SessionInfo {
        id: path.file_stem()?.to_string_lossy().into_owned(),
        time: header["time"].as_str().unwrap_or_default().to_owned(),
        first: shown,
        name,
    })
}

/// The session file for `id`, a full id or a unique prefix.
fn find(dir: &Path, id: &str) -> Result<PathBuf, Error> {
    let not_found = || {
        Error::from(SessionError::NotFound {
            id: id.into(),
            dir: dir.into(),
        })
    };
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err(not_found());
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(not_found()),
        Err(source) => {
            return Err(agentlibre_common::error::Error::Read {
                path: dir.into(),
                source,
            }
            .into());
        }
    };
    let mut matches: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|e| e == "jsonl")
                && path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .is_some_and(|stem| stem.starts_with(id))
        })
        .collect();
    match matches.len() {
        0 => Err(not_found()),
        1 => Ok(matches.pop().unwrap()),
        count => Err(SessionError::Ambiguous {
            id: id.into(),
            count,
        }
        .into()),
    }
}

fn event_json(event: &Event) -> Json {
    match event {
        Event::User(text) => json!({"type": "user", "text": text}),
        Event::Wake { flow, text } => json!({"type": "wake", "flow": flow, "text": text}),
        Event::State(state) => json!({"type": "state", "state": state}),
        Event::Thinking(text) => json!({"type": "thinking", "text": text}),
        Event::Call { text, call } => {
            let args: Map<String, Json> = call
                .args
                .iter()
                .map(|(name, value)| {
                    let value = match value {
                        Value::Text(text) => Json::from(text.as_str()),
                        Value::Int(n) => Json::from(*n),
                    };
                    (name.clone(), value)
                })
                .collect();
            let mut json = json!({"type": "call", "id": call.id, "tool": call.name, "args": args});
            if let Some(text) = text {
                json["text"] = text.as_str().into();
            }
            json
        }
        Event::Result {
            id,
            outcome,
            content,
        } => json!({"type": "result", "id": id, "outcome": outcome.name(), "content": content}),
        Event::Forge {
            name,
            kind,
            section,
        } => {
            let mut json = json!({"type": "forge", "name": name, "kind": kind.name()});
            if let Some(section) = section {
                json["section"] = section.as_str().into();
            }
            json
        }
        Event::Answer(text) => json!({"type": "answer", "text": text}),
        Event::CutOff(cut) => {
            let mut json = json!({
                "type": "cut_off",
                "limit": cut.limit,
                "discarded": cut.discarded,
                "call": cut.call,
            });
            if let Some(thinking) = cut.thinking {
                json["thinking"] = json!(thinking);
            }
            json
        }
        Event::Stir { attempt, error } => json!({"type": "stir", "attempt": attempt, "error": error}),
        Event::Command(run) => {
            let mut json = json!({"type": "command", "line": run.line, "failed": run.failed});
            if let Some(output) = &run.output {
                json["output"] = output.as_str().into();
            }
            json
        }
        Event::System { text, failed } => {
            let mut json = json!({"type": "system", "text": text});
            if *failed {
                json["failed"] = Json::from(true);
            }
            json
        }
        Event::Stop(reason) => json!({"type": "stop", "reason": reason.name()}),
        Event::Compact { summary, question } => {
            let mut json = json!({"type": "compact", "summary": summary});
            if let Some(question) = question {
                json["question"] = question.as_str().into();
            }
            json
        }
        Event::Memory { name, kind, text } => {
            let mut json = json!({"type": "memory", "name": name, "kind": kind.name()});
            if let Some(text) = text {
                json["text"] = text.as_str().into();
            }
            json
        }
        Event::Media(media) => {
            let mut json = json!({
                "type": "media",
                "kind": media.kind.label().to_lowercase(),
                "number": media.number,
                "sha256": media.sha256,
                "ext": media.ext,
                "name": media.name,
            });
            if let Some(transcript) = &media.transcript {
                json["transcript"] = transcript.as_str().into();
            }
            if !media.frames.is_empty() {
                json["frames"] = media
                    .frames
                    .iter()
                    .map(|frame| json!({"at": frame.at, "sha256": frame.sha256}))
                    .collect();
            }
            json
        }
    }
}

pub fn transition_from_json(json: &Json) -> Option<TransitionLine> {
    Some(TransitionLine {
        turn: usize::try_from(json["turn"].as_u64()?).ok()?,
        step: usize::try_from(json["step"].as_u64()?).ok()?,
        from: json["from"].as_str()?.to_owned(),
        to: json["to"].as_str()?.to_owned(),
        event: json["event"].as_str()?.to_owned(),
        ms: json["ms"].as_u64()?,
        tokens: json["tokens"].as_u64(),
        tool: json["tool"].as_str().map(str::to_owned),
    })
}

pub fn event_from_json(json: &Json) -> Option<Event> {
    let text = |key: &str| json[key].as_str().map(str::to_owned);
    Some(match json["type"].as_str()? {
        "user" => Event::User(text("text")?),
        "wake" => Event::Wake { flow: text("flow")?, text: text("text")? },
        "state" => Event::State(text("state")?),
        "thinking" => Event::Thinking(text("text")?),
        "call" => {
            // `serde_json` keeps object keys sorted; `args_in_order`
            // restores the parameter order (D79).
            let args = json["args"]
                .as_object()?
                .iter()
                .map(|(name, value)| {
                    let value = match value {
                        Json::String(text) => Value::Text(text.clone()),
                        Json::Number(n) => Value::Int(n.as_i64()?),
                        _ => return None,
                    };
                    Some((name.clone(), value))
                })
                .collect::<Option<Vec<_>>>()?;
            let name = text("tool")?;
            Event::Call {
                text: text("text"),
                call: ToolCall {
                    args: args_in_order(&name, args),
                    id: text("id")?,
                    name,
                },
            }
        }
        "result" => Event::Result {
            id: text("id")?,
            outcome: [Outcome::Ok, Outcome::Fail, Outcome::Timeout, Outcome::Interrupted]
                .into_iter()
                .find(|o| json["outcome"] == o.name())?,
            content: text("content")?,
        },
        "forge" => Event::Forge {
            name: text("name")?,
            kind: ForgeKind::ALL
                .into_iter()
                .find(|kind| json["kind"] == kind.name())?,
            section: text("section"),
        },
        "answer" => Event::Answer(text("text")?),
        "command" => Event::Command(CommandRun {
            line: text("line")?,
            failed: json["failed"].as_bool()?,
            output: text("output"),
        }),
        "system" => Event::System {
            text: text("text")?,
            failed: json["failed"].as_bool().unwrap_or(false),
        },
        "cut_off" => {
            let count = |key: &str| u32::try_from(json[key].as_u64()?).ok();
            Event::CutOff(CutOff {
                limit: count("limit")?,
                thinking: match json.get("thinking") {
                    None => None,
                    Some(_) => Some(count("thinking")?),
                },
                discarded: count("discarded")?,
                call: json["call"].as_bool()?,
            })
        }
        "stir" => Event::Stir {
            attempt: u32::try_from(json["attempt"].as_u64()?).ok()?,
            error: text("error")?,
        },
        "stop" => Event::Stop(
            StopReason::ALL
                .into_iter()
                .find(|r| json["reason"] == r.name())?,
        ),
        "compact" => Event::Compact {
            summary: text("summary")?,
            question: text("question"),
        },
        "memory" => Event::Memory {
            name: text("name")?,
            kind: MemoryKind::ALL
                .into_iter()
                .find(|kind| json["kind"] == kind.name())?,
            text: text("text"),
        },
        "media" => Event::Media(Media {
            kind: Kind::ALL
                .into_iter()
                .find(|kind| json["kind"] == kind.label().to_lowercase())?,
            number: u32::try_from(json["number"].as_u64()?).ok()?,
            sha256: text("sha256")?,
            ext: text("ext")?,
            name: text("name")?,
            transcript: text("transcript"),
            frames: match json.get("frames") {
                None => Vec::new(),
                Some(frames) => frames
                    .as_array()?
                    .iter()
                    .map(|frame| {
                        Some(Frame {
                            at: frame["at"].as_f64()?,
                            sha256: frame["sha256"].as_str()?.to_owned(),
                        })
                    })
                    .collect::<Option<Vec<_>>>()?,
            },
        }),
        _ => return None,
    })
}

/// What the model reads after a generation hit its budget (D154,
/// AGL-009); in the stirred mode it says how much of the budget the
/// thinking took (AGL-043 F8). The prefix is added where the message is
/// rendered.
fn cut_off_notice(cut: &CutOff) -> String {
    let CutOff {
        limit,
        discarded,
        ..
    } = cut;
    match cut.thinking {
        None => format!(
            "Your last action hit max_action_tokens ({limit}) and was discarded; nothing was \
             executed or shown. Split the work into smaller actions, for example write a large \
             file in parts."
        ),
        Some(thinking) => format!(
            "Your last step hit its limit of {limit} tokens, {thinking} of them thinking and \
             {discarded} the action, and was discarded; nothing was executed or shown. Think \
             less, or split the work into smaller actions, for example write a large file in \
             parts."
        ),
    }
}

/// `args` in the parameter order of the built-in tool `tool` (D79).
fn args_in_order(tool: &str, mut args: Vec<(String, Value)>) -> Vec<(String, Value)> {
    let order = crate::tools::spec(tool).map_or(&[][..], |spec| spec.params);
    args.sort_by_key(|(name, _)| {
        order.iter().position(|param| param.name == name).unwrap_or(usize::MAX)
    });
    args
}

/// A new UUIDv7: milliseconds since the epoch, then random bits (D49).
pub(crate) fn uuid_v7() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    bytes[..6].copy_from_slice(&millis.to_be_bytes()[2..]);
    bytes[6] = 0x70 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

pub mod error;

#[cfg(test)]
mod tests;
