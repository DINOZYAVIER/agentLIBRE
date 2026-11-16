//! The control socket of a session (AGL-074, D3): what a surface beside the
//! terminal sends a running session, and what it answers.
//!
//! A REPL session listens on `$XDG_RUNTIME_DIR/agentLIBRE/sessions/<id>.sock`
//! (the directory 0700, the socket 0600) while it runs. A request is one
//! line of JSON, and so is its answer:
//!
//! - `{"say":"TEXT"}` → `{"ok":true,"queued":false}`: TEXT is the session's
//!   next message, run as if typed; `"queued":true` while a turn runs, when
//!   it waits for the turn's end (D4);
//! - `{"stop":true}` → `{"ok":true}`: the running turn is interrupted as
//!   Ctrl-C does, and what waits is dropped;
//! - `{"hello":true}` → `{"pid":N,"session":"<id>","name":"…"|null,
//!   "busy":bool}`: who the session is, for a bridge's cards;
//! - anything else → `{"ok":false,"error":"…"}`.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value as Json, json};

/// Below the runtime directory, the directory of the sessions' sockets.
pub const SESSIONS_DIR: &str = "sessions";

/// The directory of the sockets, below agentLIBRE's runtime directory.
pub fn sockets_dir(runtime: &Path) -> PathBuf {
    runtime.join(SESSIONS_DIR)
}

/// The socket of the session `id`.
pub fn socket_path(runtime: &Path, id: &str) -> PathBuf {
    sockets_dir(runtime).join(format!("{id}.sock"))
}

/// The session id a socket's path names, when it is one.
pub fn session_of(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    name.strip_suffix(".sock").filter(|id| !id.is_empty())
}

/// A request to a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// The next message.
    Say(String),
    /// Interrupt the turn.
    Stop,
    /// Who the session is.
    Hello,
}

impl Request {
    /// The request of a line, or why it is none.
    pub fn parse(line: &str) -> Result<Request, String> {
        let json: Json =
            serde_json::from_str(line).map_err(|error| format!("not JSON: {error}"))?;
        let object = json.as_object().ok_or("not an object")?;
        match (object.len(), object.iter().next()) {
            (1, Some((key, value))) => match (key.as_str(), value) {
                ("say", Json::String(text)) => Ok(Request::Say(text.clone())),
                ("say", _) => Err("say takes a string".into()),
                ("stop", Json::Bool(true)) => Ok(Request::Stop),
                ("hello", Json::Bool(true)) => Ok(Request::Hello),
                (other, _) => Err(format!("no request {other:?}")),
            },
            _ => Err("one key a request".into()),
        }
    }

    /// The request's line, without its newline.
    pub fn line(&self) -> String {
        match self {
            Request::Say(text) => json!({ "say": text }),
            Request::Stop => json!({ "stop": true }),
            Request::Hello => json!({ "hello": true }),
        }
        .to_string()
    }
}

/// Who a session is, as `hello` answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    /// The process of the REPL.
    pub pid: u32,
    /// The session's id.
    pub session: String,
    /// The name the human gave it (AGL-062).
    pub name: Option<String>,
    /// A turn runs or messages wait for one.
    pub busy: bool,
}

/// An answer to a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// `say` taken; `queued` when it waits for the running turn.
    Said { queued: bool },
    /// `stop` taken.
    Stopped,
    /// `hello`'s answer.
    Hello(Hello),
    /// The request was not one.
    Refused(String),
}

impl Reply {
    /// The answer's line, without its newline.
    pub fn line(&self) -> String {
        match self {
            Reply::Said { queued } => json!({ "ok": true, "queued": queued }),
            Reply::Stopped => json!({ "ok": true }),
            Reply::Hello(hello) => json!({
                "pid": hello.pid,
                "session": hello.session,
                "name": hello.name,
                "busy": hello.busy,
            }),
            Reply::Refused(error) => json!({ "ok": false, "error": error }),
        }
        .to_string()
    }

    /// The answer of a line to `request`, or none when it is not one.
    pub fn parse(request: &Request, line: &str) -> Option<Reply> {
        let json: Json = serde_json::from_str(line).ok()?;
        if json["ok"] == false {
            return Some(Reply::Refused(json["error"].as_str()?.to_owned()));
        }
        match request {
            Request::Say(_) => Some(Reply::Said {
                queued: json["queued"].as_bool()?,
            }),
            Request::Stop => (json["ok"] == true).then_some(Reply::Stopped),
            Request::Hello => Some(Reply::Hello(Hello {
                pid: u32::try_from(json["pid"].as_u64()?).ok()?,
                session: json["session"].as_str()?.to_owned(),
                name: json["name"].as_str().map(str::to_owned),
                busy: json["busy"].as_bool()?,
            })),
        }
    }
}

/// Answers each line of `stream` with `answer`, until the other side
/// closes it.
pub fn serve(stream: UnixStream, answer: &mut dyn FnMut(Request) -> Reply) -> io::Result<()> {
    let mut writer = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match Request::parse(&line) {
            Ok(request) => answer(request),
            Err(error) => Reply::Refused(error),
        };
        writeln!(writer, "{}", reply.line())?;
    }
    Ok(())
}

/// How long [`ask`] waits for a session's answer.
const ASK_TIMEOUT: Duration = Duration::from_secs(5);

/// Sends `request` to the session listening on `socket` and reads its
/// answer.
pub fn ask(socket: &Path, request: &Request) -> io::Result<Reply> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(ASK_TIMEOUT))?;
    writeln!(stream, "{}", request.line())?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Reply::parse(request, &line).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("not an answer: {}", line.trim()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_one_key_lines() {
        assert_eq!(
            Request::parse(r#"{"say":"go on"}"#),
            Ok(Request::Say("go on".into()))
        );
        assert_eq!(Request::parse(r#"{"stop":true}"#), Ok(Request::Stop));
        assert_eq!(Request::parse(r#"{"hello":true}"#), Ok(Request::Hello));
        for line in [
            "say go on",
            "[]",
            r#"{"say":1}"#,
            r#"{"stop":false}"#,
            r#"{"say":"a","stop":true}"#,
            r#"{"quit":true}"#,
        ] {
            assert!(Request::parse(line).is_err(), "{line}");
        }
        for request in [
            Request::Say("a \"b\"\n".into()),
            Request::Stop,
            Request::Hello,
        ] {
            assert_eq!(Request::parse(&request.line()), Ok(request));
        }
    }

    #[test]
    fn answers_read_back_for_their_request() {
        let hello = Reply::Hello(Hello {
            pid: 42,
            session: "01a1".into(),
            name: None,
            busy: true,
        });
        assert_eq!(
            hello.line(),
            r#"{"busy":true,"name":null,"pid":42,"session":"01a1"}"#
        );
        assert_eq!(Reply::parse(&Request::Hello, &hello.line()), Some(hello));
        let said = Reply::Said { queued: true };
        assert_eq!(said.line(), r#"{"ok":true,"queued":true}"#);
        assert_eq!(
            Reply::parse(&Request::Say("x".into()), &said.line()),
            Some(said)
        );
        assert_eq!(
            Reply::parse(&Request::Stop, &Reply::Stopped.line()),
            Some(Reply::Stopped)
        );
        let refused = Reply::Refused("no request \"quit\"".into());
        assert_eq!(Reply::parse(&Request::Stop, &refused.line()), Some(refused));
        assert_eq!(Reply::parse(&Request::Hello, "{}"), None);
    }

    #[test]
    fn a_socket_names_its_session() {
        let runtime = Path::new("/run/user/1000/agentLIBRE");
        let path = socket_path(runtime, "01a1-b2");
        assert_eq!(
            path,
            Path::new("/run/user/1000/agentLIBRE/sessions/01a1-b2.sock")
        );
        assert_eq!(session_of(&path), Some("01a1-b2"));
        assert_eq!(session_of(Path::new("/x/.sock")), None);
        assert_eq!(session_of(Path::new("/x/a.json")), None);
    }

    #[test]
    fn a_served_stream_answers_each_line_and_refuses_what_is_none() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut said = Vec::new();
            serve(theirs, &mut |request| match request {
                Request::Say(text) => {
                    said.push(text);
                    Reply::Said {
                        queued: said.len() > 1,
                    }
                }
                Request::Stop => Reply::Stopped,
                Request::Hello => Reply::Refused("not here".into()),
            })
            .unwrap();
            said
        });
        let mut writer = ours.try_clone().unwrap();
        let mut lines = BufReader::new(ours).lines();
        let mut ask = |line: &str| {
            writeln!(writer, "{line}").unwrap();
            lines.next().unwrap().unwrap()
        };
        assert_eq!(ask(r#"{"say":"one"}"#), r#"{"ok":true,"queued":false}"#);
        assert_eq!(ask(r#"{"say":"two"}"#), r#"{"ok":true,"queued":true}"#);
        assert!(ask("nonsense").starts_with(r#"{"error":"not JSON"#));
        assert_eq!(ask(r#"{"stop":true}"#), r#"{"ok":true}"#);
        drop(writer);
        drop(lines);
        assert_eq!(server.join().unwrap(), ["one", "two"]);
    }
}
