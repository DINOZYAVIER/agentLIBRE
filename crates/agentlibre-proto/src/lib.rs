//! agentLIBRE's protocol (AGL-076): what a client says to the daemon and
//! what the daemon tells it, one line of JSON a message (D8), on the
//! daemon's socket (D7). It has no dependency on agentLIBRE's core (D2): a
//! client — the terminal, the chat, a bridge, a client on another computer
//! later — links it alone.
//!
//! A client says [`Request`]s; the daemon answers each with a [`Reply`] in
//! the order they came, but `stop` and `answer`, which have none, and sends
//! besides, unasked, what the sessions it
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
        /// The client's process, for a bridge to find its window.
        pid: u32,
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
    /// A session's past: its events (AGL-073, D10).
    History { session: String },
    /// A verb of a plane's flows (AGL-118, D13, D17): `agl plane flow`,
    /// run on that plane's thread, answered with what the flow's place says.
    Flow { plane: String, verb: FlowVerb },
}

/// A verb of a flow, as on the command line (AGL-118, D2, D7). `start`
/// moves the machine, `stop` and `reset` are the human's outside it,
/// `status` and `list` read it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "snake_case")]
pub enum FlowVerb {
    List,
    Start { name: String },
    Stop { name: String },
    Status { name: String },
    Reset { name: String },
    /// Deliver `message.<event>` to a flow (AGL-118, D15, D21).
    Message { name: String, event: String },
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
    /// What a flow verb said (AGL-118, D2, D7), of the plane named.
    Said { plane: String, said: Said },
    /// A session's events, its past (AGL-073, D10).
    History { session: String, events: Vec<Event> },
    /// What a subscribed session does (D3).
    Sent { session: String, sent: Sent },
    /// The end of a command of a subscribed session; `yours` when this
    /// client sent the command (D12: the commands of two clients queue).
    Done {
        session: String,
        done: Done,
        yours: bool,
    },
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
    /// The draft server of the live dictation (AGL-122, D9); `None` —
    /// no drafts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<DraftSpec>,
}

/// The draft server as the daemon resolved it (AGL-122, D9): a
/// whisper-server unit of its own, named, aimed, and idle-timed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DraftSpec {
    pub url: String,
    pub unit: String,
    pub model: String,
    pub program: Option<String>,
    pub args: Vec<String>,
}

/// A session the daemon holds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub name: Option<String>,
    pub plane: Option<String>,
    pub workspace: String,
    pub function: String,
    /// The processes of the clients that hold it, for a bridge to find
    /// their windows.
    pub pids: Vec<u32>,
    pub busy: bool,
}

/// A connection's lines. `new` reads with no limit, as the clients of
/// the daemon do; `with_limit` reads as the daemon does, whose limit of
/// `LINE_LIMIT` answers a longer line with `Refused` and keeps the
/// connection (AGL-121, D27, D54).
pub struct Lines<R> {
    reader: R,
    line: String,
    limit: Option<usize>,
}

/// The longest request line the daemon takes: enough for a base64 audio
/// pass (AGL-121, D27).
pub const LINE_LIMIT: usize = 32 << 20;

/// The read of one line (AGL-121, D27, D54).
#[derive(Debug)]
pub enum Read<T> {
    /// A message.
    Message(T),
    /// A line over the limit: read to its newline and dropped; the
    /// answer is `Refused`, the connection stays (D54).
    TooLong,
    /// The end of the stream.
    End,
}

impl<R: BufRead> Lines<R> {
    pub fn new(reader: R) -> Lines<R> {
        Lines {
            reader,
            line: String::new(),
            limit: None,
        }
    }

    /// Lines read with a longest line of `limit` (D27).
    pub fn with_limit(reader: R, limit: usize) -> Lines<R> {
        Lines {
            limit: Some(limit),
            ..Lines::new(reader)
        }
    }

    /// The next message; `End` at the end. A line that is not a message
    /// is an error of kind `InvalidData`. Over a limit, a longer line is
    /// read to its newline, dropped, and told `TooLong` (AGL-121, D54).
    pub fn read<T: for<'de> Deserialize<'de>>(&mut self) -> io::Result<Read<T>> {
        self.line.clear();
        match self.limit {
            None => {
                if self.reader.read_line(&mut self.line)? == 0 {
                    return Ok(Read::End);
                }
            }
            Some(limit) => {
                let mut bytes: Vec<u8> = Vec::new();
                let mut over = false;
                loop {
                    let chunk = self.reader.fill_buf()?;
                    if chunk.is_empty() {
                        break;
                    }
                    let room = (limit + 1).saturating_sub(bytes.len());
                    match chunk.iter().position(|b| *b == b'\n') {
                        Some(n) => {
                            let take = (n + 1).min(room);
                            bytes.extend_from_slice(&chunk[..take]);
                            self.reader.consume(n + 1);
                            over |= take < n + 1;
                            break;
                        }
                        None => {
                            let len = chunk.len();
                            let take = len.min(room);
                            bytes.extend_from_slice(&chunk[..take]);
                            self.reader.consume(take);
                            over |= take < len;
                        }
                    }
                }
                if bytes.is_empty() && !over {
                    return Ok(Read::End);
                }
                if over {
                    return Ok(Read::TooLong);
                }
                self.line = String::from_utf8(bytes)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            }
        }
        serde_json::from_str(self.line.trim_end())
            .map(Read::Message)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    /// The next line as it came, unparsed, without the limit; `None` at
    /// the end. For one who passes the daemon's lines on without reading
    /// them, as the bridge does (AGL-121, D31; D54: the clients read
    /// with no limit).
    pub fn read_raw(&mut self) -> io::Result<Option<String>> {
        self.line.clear();
        if self.reader.read_line(&mut self.line)? == 0 {
            return Ok(None);
        }
        Ok(Some(self.line.trim_end().to_owned()))
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
