//! agentLIBRE's protocol (AGL-076): what a client says to the daemon and
//! what the daemon tells it, one line of JSON a message (D8), on the
//! daemon's socket (D7). It has no dependency on agentLIBRE's core (D2): a
//! client — the terminal, the chat, a bridge, a client on another computer
//! later — links it alone.
//!
//! A client says [`Request`]s; the daemon answers each with a [`Reply`] in
//! the order they came, and sends besides, unasked, what the sessions it
//! subscribed to do ([`Reply::Sent`], [`Reply::Done`]) and the questions of
//! the planes it opened ([`Reply::Question`]).

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};

pub mod base64;
mod wire;

pub use wire::*;

/// The protocol's version, in `hello` (D9).
pub const VERSION: u32 = 1;

/// The daemon's socket below agentLIBRE's runtime directory (D7).
pub const SOCKET: &str = "agl.sock";

/// What a client asks (D10, D13, D15).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    /// The first request: the client's version, whether a human answers
    /// the planes' questions (D11), and the language of what is said.
    Hello {
        version: u32,
        human: bool,
        lang: String,
    },
    /// A session's plane: made or resumed, and started (D10).
    Open(Open),
    /// The sessions the daemon holds.
    Sessions,
    /// What a session does, or every session's with none (D3).
    Subscribe { session: Option<String> },
    /// A message and its media, at a reasoning level (D14).
    Say {
        session: String,
        message: String,
        attached: Vec<Attached>,
        reasoning: Option<String>,
    },
    /// The running turn stopped, as Ctrl-C does.
    Stop { session: String },
    /// `/remember name | text`; `workspace`, the workspace's memory in a
    /// plane (AGL-077, D8).
    Remember {
        session: String,
        name: String,
        text: String,
        workspace: bool,
    },
    /// `/memory remove <name>`, agreed.
    Remove { session: String, name: String },
    /// `/memory list`, `/memory show <name>`.
    Look { session: String, look: Look },
    Rename { session: String, name: String },
    /// A new session on the plane; the reply names it.
    New { session: String },
    Compact {
        session: String,
        reasoning: Option<String>,
    },
    /// The lines of a client's own commands, for the session's record
    /// (D13).
    Record {
        session: String,
        lines: Vec<CommandLine>,
    },
    /// An audio's text (D15), by the whisper of the plane of `dir` and
    /// `plane`, as `open` names them.
    Transcribe {
        dir: String,
        plane: Option<String>,
        audio: Attached,
    },
    /// The answer to a plane's question (D11); `None`, no answer.
    Answer { question: u64, answer: Option<String> },
    /// The client no longer holds the session (D12).
    Close { session: String },
}

/// What `open` names (D10): as on the command line.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Open {
    /// The directory of the run, the workspace of a new session.
    pub dir: String,
    pub plane: Option<String>,
    /// `last`, a name, an id or the start of one.
    pub resume: Option<String>,
    pub function: Option<String>,
    pub reasoning: Option<String>,
    /// `--yes`: agree to a restart without asking (D11).
    pub yes: bool,
    /// One request (`-p`): the server comes up before the session is made.
    pub one_request: bool,
    /// Whether the server comes up as the session starts (the REPL's
    /// `repl.start_server`, AGL-046).
    pub start_server: bool,
}

/// What `look` looks at.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Look {
    List,
    Show { name: String },
}

/// What the daemon answers and sends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Hello { version: u32, pid: u32 },
    /// A request refused, in the client's language.
    Refused { error: String },
    /// A request done that has nothing else to say.
    Ok,
    Opened(Opened),
    Sessions { sessions: Vec<SessionInfo> },
    Transcribed { text: String },
    /// What a subscribed session does (D3).
    Sent { session: String, sent: Sent },
    /// The end of a command of a subscribed session.
    Done { session: String, done: Done },
    /// A plane's question to the client that opened it (D11).
    Question {
        question: u64,
        session: String,
        ask: Ask,
        text: String,
    },
}

/// A plane's question (D11).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ask {
    /// Yes or no: the answer `y`/`yes` agrees.
    Yes,
    /// A line: a folder of weights (AGL-039, D24).
    Line,
}

/// A session as `open` made or resumed it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Opened {
    pub session: SessionView,
    /// Whether the session existed before (D126).
    pub resumed: bool,
    pub function: String,
    pub workspace: String,
    pub server_url: String,
    /// The server's context, when it answered at the start.
    pub context: Option<usize>,
    pub reasoning: Option<String>,
}

/// A session the daemon holds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: Option<String>,
    pub plane: Option<String>,
    pub workspace: String,
    pub function: String,
    /// The pid of the daemon, for a bridge's window lookup.
    pub pid: u32,
    pub busy: bool,
}

/// A connection's lines.
pub struct Lines<R> {
    reader: R,
    line: String,
}

impl<R: BufRead> Lines<R> {
    pub fn new(reader: R) -> Lines<R> {
        Lines {
            reader,
            line: String::new(),
        }
    }

    /// The next message; `None` at the end. A line that is not a message is
    /// an error of kind `InvalidData`.
    pub fn read<T: for<'de> Deserialize<'de>>(&mut self) -> io::Result<Option<T>> {
        self.line.clear();
        if self.reader.read_line(&mut self.line)? == 0 {
            return Ok(None);
        }
        serde_json::from_str(self.line.trim_end())
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
}

/// Writes `message` as one line.
pub fn send<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    let mut line = serde_json::to_string(message).map_err(io::Error::other)?;
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()
}

#[cfg(test)]
mod tests;
