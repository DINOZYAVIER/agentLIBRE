//! Sessions: an append-only JSONL log of a conversation that can be
//! resumed (D30, D31, D36, D49, D104-D108, D139-D141).

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value as Json, json};

use crate::action::{Outcome, Value};
use crate::backend::{Message, ToolCall};
use crate::error::Error;
use crate::media::{self, Frame, Kind, Media, Source};
use crate::memory::Kind as MemoryKind;

/// The session format version (D104).
const VERSION: u64 = 1;

/// The result of a call cut off before it finished (D108).
pub const INTERRUPTED: &str = "Interrupted before the result.";

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

    fn name(self) -> &'static str {
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

/// A session event (D104). The header line is not an event.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    User(String),
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
pub struct Session {
    pub id: String,
    pub function: String,
    pub workspace: PathBuf,
    pub events: Vec<Event>,
    path: PathBuf,
    file: File,
    /// What the prompt was last made of, by kind (AGL-046, D14).
    prompt_inputs: BTreeMap<String, String>,
}

impl Session {
    /// Starts a session in `dir`, created if needed (D64).
    pub fn create(dir: &Path, function: &str, workspace: &Path) -> Result<Session, Error> {
        let write_error = |path: &Path, source| Error::SessionWrite {
            path: path.into(),
            source,
        };
        fs::create_dir_all(dir).map_err(|source| write_error(dir, source))?;
        let id = uuid_v7().map_err(|source| write_error(dir, source))?;
        let path = dir.join(format!("{id}.jsonl"));
        let file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(&path)
            .map_err(|source| write_error(&path, source))?;
        let mut session = Session {
            id,
            function: function.into(),
            workspace: workspace.into(),
            events: Vec::new(),
            path,
            file,
            prompt_inputs: BTreeMap::new(),
        };
        let header = json!({
            "type": "session",
            "version": VERSION,
            "function": function,
            "workspace": workspace.to_string_lossy(),
            "time": utc_now(),
        });
        session.write_line(&header)?;
        Ok(session)
    }

    /// Opens the session whose id is or starts with `id` (D107). A call
    /// left without a result gets one (D108).
    pub fn open(dir: &Path, id: &str) -> Result<Session, Error> {
        let path = find(dir, id)?;
        let read_error = |source| Error::Read {
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
        let bad = |line: usize, detail: &str| Error::SessionLine {
            path: path.clone(),
            line: line + 1,
            detail: detail.into(),
        };
        let (_, header) = lines.next().ok_or_else(|| bad(0, "no header"))?;
        let header: Json = serde_json::from_str(header).map_err(|e| bad(0, &e.to_string()))?;
        if header["type"] != "session" {
            return Err(bad(0, "the first line is not a session header"));
        }
        if header["version"] != VERSION {
            return Err(bad(0, "unsupported session version"));
        }
        let (Some(function), Some(workspace)) =
            (header["function"].as_str(), header["workspace"].as_str())
        else {
            return Err(bad(0, "the header needs function and workspace"));
        };
        let mut events = Vec::new();
        let mut prompt_inputs = BTreeMap::new();
        for (index, line) in lines {
            let json: Json = serde_json::from_str(line).map_err(|e| bad(index, &e.to_string()))?;
            // What the observer wrote is not an event, so it can reach
            // neither the history nor the replay (AGL-046).
            match json["type"].as_str() {
                Some("transition" | "step" | "prefill" | "prompt_progress") => continue,
                Some("prompt_input") => {
                    if let (Some(kind), Some(text)) = (json["kind"].as_str(), json["text"].as_str())
                    {
                        prompt_inputs.insert(kind.to_owned(), text.to_owned());
                    }
                    continue;
                }
                _ => {}
            }
            events.push(event_from_json(&json).ok_or_else(|| bad(index, "unknown event"))?);
        }
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|source| Error::SessionWrite {
                path: path.clone(),
                source,
            })?;
        let mut session = Session {
            id: path.file_stem().unwrap().to_string_lossy().into_owned(),
            function: function.into(),
            workspace: workspace.into(),
            events,
            path,
            file,
            prompt_inputs,
        };
        if complete.len() < text.len() {
            // Drop the half line so that new events start on their own line.
            session
                .file
                .set_len(complete.len() as u64)
                .map_err(|source| Error::SessionWrite {
                    path: session.path.clone(),
                    source,
                })?;
        }
        if let Some(Event::Call { call, .. }) = session.events.last() {
            let id = call.id.clone();
            session.append(Event::Result {
                id,
                outcome: Outcome::Fail,
                content: INTERRUPTED.into(),
            })?;
        }
        Ok(session)
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

    /// A handle on the session file for the observer of a turn (AGL-046).
    /// The file is opened to append, so the lines of both stay whole.
    pub fn log_file(&self) -> Option<File> {
        self.file.try_clone().ok()
    }

    /// The turns of the session: its user messages.
    pub fn turns(&self) -> usize {
        self.events
            .iter()
            .filter(|event| matches!(event, Event::User(_)))
            .count()
    }

    fn write_line(&mut self, json: &Json) -> Result<(), Error> {
        let line = format!("{json}\n");
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.flush())
            .map_err(|source| Error::SessionWrite {
                path: self.path.clone(),
                source,
            })
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
                Event::State(_)
                | Event::Stop(_)
                | Event::Forge { .. }
                | Event::Media(_)
                | Event::Memory { .. }
                | Event::Stir { .. } => {}
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
        if !turn.iter().any(|e| matches!(e, Event::User(_))) {
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

/// The session file for `id`, a full id or a unique prefix.
fn find(dir: &Path, id: &str) -> Result<PathBuf, Error> {
    let not_found = || Error::SessionNotFound {
        id: id.into(),
        dir: dir.into(),
    };
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err(not_found());
    }
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(not_found()),
        Err(source) => {
            return Err(Error::Read {
                path: dir.into(),
                source,
            });
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
        count => Err(Error::SessionAmbiguous {
            id: id.into(),
            count,
        }),
    }
}

fn event_json(event: &Event) -> Json {
    match event {
        Event::User(text) => json!({"type": "user", "text": text}),
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

fn event_from_json(json: &Json) -> Option<Event> {
    let text = |key: &str| json[key].as_str().map(str::to_owned);
    Some(match json["type"].as_str()? {
        "user" => Event::User(text("text")?),
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
            outcome: [Outcome::Ok, Outcome::Fail, Outcome::Timeout]
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
    let order: &[&str] = match tool {
        "bash" => &["command", "timeout"],
        "edit" => &["path", "old", "new"],
        "view" => &["path"],
        "search" => &["language", "limit", "query", "source"],
        "memory" => &["action", "hook", "name", "new", "old", "text", "title"],
        "forge" => &["action", "name", "new", "old", "section", "state", "text", "title"],
        _ => &[],
    };
    args.sort_by_key(|(name, _)| order.iter().position(|o| o == name).unwrap_or(usize::MAX));
    args
}

/// A new UUIDv7: milliseconds since the epoch, then random bits (D49).
fn uuid_v7() -> io::Result<String> {
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

/// The current UTC time as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn utc_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    utc(seconds)
}

/// Formats Unix `seconds` as UTC (the days-to-civil algorithm by Howard
/// Hinnant).
fn utc(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests;
