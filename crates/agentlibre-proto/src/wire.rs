//! What a plane does, as it goes on the wire (AGL-076, D2): the forms of
//! the core's reports, events and ends, which the core maps both ways.

use serde::{Deserialize, Serialize};

/// What a plane sends while it takes a command (the core's `Sent`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sent {
    Report(Report),
    /// A line of the server's start or of the transcription.
    Line(String),
    /// The server answered, with a context of this many tokens.
    Context(usize),
    /// The media of the message are stored: the session's media of each
    /// kind (image, video, audio).
    Stored([u32; 3]),
    /// The turn is over, before its FSM is closed.
    Over,
}

/// The end of a command (the core's `Done`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Done {
    /// A message's turn; an error in the client's language.
    pub turn: Option<Result<TurnEnd, String>>,
    /// Why a turn stopped, in the words of its Function.
    pub why: Option<String>,
    /// What a slash command said; `/new` fails as an error.
    pub said: Option<Result<Said, String>>,
    pub session: SessionView,
}

/// A session as a client holds it after a command.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionView {
    pub id: String,
    pub name: Option<String>,
    pub plane: Option<String>,
    /// The directory of its outputs.
    pub outputs: String,
    /// The state of an unfinished turn.
    pub unfinished: Option<String>,
    /// The media of each kind: image, video, audio.
    pub media: [u32; 3],
}

/// How a turn ended.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEnd {
    Answer(String),
    Stopped(StopReason),
}

/// What a slash command did (the core's `Said`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Said {
    pub lines: Vec<Told>,
    pub memory: Option<MemoryWrite>,
    pub content: Option<String>,
    pub failed: bool,
    pub interrupted: bool,
    /// A line the session would not take, in the client's language.
    pub record_error: Option<String>,
}

/// A line a slash command says.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Told {
    Output(String),
    Notice(String),
    Refusal(String),
    Fault(String),
    Pending(String),
}

/// A write of the memory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryWrite {
    pub name: String,
    pub kind: MemoryKind,
    pub text: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Added,
    Edited,
    Removed,
}

/// What a turn tells (the core's `Report`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Report {
    Event(Event),
    Prefill,
    Thinking(String),
    ThinkingDone,
    ActionStarted,
    ActionText(String),
    Stirred { attempt: u32, max: u32, error: String },
    Compacting,
    Compacted,
    CompactFailed,
    ContextUsed(usize),
    Decoded(usize),
    PromptProgress(PromptProgress),
    PromptRead(Prefill),
    StepStarted(StepRecord),
    Transition(Transition),
}

/// An event of a session (the core's `Event`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    User(String),
    State(String),
    Thinking(String),
    Call {
        text: Option<String>,
        call: ToolCall,
    },
    Result {
        id: String,
        outcome: Outcome,
        content: String,
    },
    Forge {
        name: String,
        kind: ForgeKind,
        section: Option<String>,
    },
    Answer(String),
    CutOff(CutOff),
    Stir {
        attempt: u32,
        error: String,
    },
    Stop(StopReason),
    Memory {
        name: String,
        kind: MemoryKind,
        text: Option<String>,
    },
    Media(Media),
    Compact {
        summary: String,
        question: Option<String>,
    },
    Command(CommandLine),
    System {
        text: String,
        failed: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Vec<(String, Value)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Value {
    Text(String),
    Int(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Fail,
    Timeout,
    Interrupted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    StepLimit,
    ContextFull,
    ServerError,
    Interrupted,
    StirLimit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForgeKind {
    Created,
    Edited,
    Appended,
    Removed,
    StatusChanged,
    Renamed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CutOff {
    pub limit: u32,
    pub thinking: Option<u32>,
    pub discarded: u32,
    pub call: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Video,
    Audio,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Media {
    pub kind: MediaKind,
    pub number: u32,
    pub sha256: String,
    pub ext: String,
    pub name: String,
    pub transcript: Option<String>,
    pub frames: Vec<Frame>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub at: f64,
    pub sha256: String,
}

/// A command run at the prompt, as the session records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandLine {
    pub line: String,
    pub failed: bool,
    pub output: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PromptProgress {
    pub total: usize,
    pub cache: usize,
    pub processed: usize,
    pub time_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Prefill {
    pub tokens: usize,
    pub ms: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepRecord {
    pub state: String,
    pub prompt_sha256: String,
    pub prompt_tokens: usize,
    pub think: bool,
    pub think_budget: usize,
    pub action_budget: u32,
    pub context: usize,
    pub stirred: bool,
    pub reasoning: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Ready,
    StartingServer,
    Transcribing,
    Prefill,
    Thinking,
    Writing,
    Running(String),
    Compacting,
    Retry { attempt: u32, max: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseEvent {
    ServerStart,
    Transcribe,
    StepStart,
    FirstThinking,
    FirstText,
    ToolStart { tool: String },
    ToolEnd(Outcome),
    CompactStart,
    Compacted,
    CompactFailed,
    CutOff,
    Stir { attempt: u32, max: u32 },
    Answer,
    Stop(StopReason),
    Failed,
    Idle,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    pub from: Phase,
    pub to: Phase,
    pub event: PhaseEvent,
    /// The time spent in `from`, in microseconds.
    pub micros: u64,
    pub violation: bool,
}

/// A medium a message names (D14): its bytes in base64.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attached {
    pub number: u32,
    pub kind: MediaKind,
    pub name: String,
    pub ext: String,
    pub base64: String,
}
