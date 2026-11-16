//! `agl notify` (AGL-074, D1, D2, D5, D6, D8): the running sessions told on
//! the desktop, and answered from it.
//!
//! The bridge finds the sessions by their control sockets
//! (`$XDG_RUNTIME_DIR/agentLIBRE/sessions/<id>.sock`, [`crate::control`]):
//! for each, `hello`, then its session file followed from its end. What
//! goes in comes back through the socket: an answer typed in a card is
//! `say`, its Stop is `stop`.
//!
//! The session file is the bridges' interface (D7). The records read here:
//! `user` (a turn starts), `transition` (its phase: `to` and, for
//! `Running`, `tool`), `call` (`tool`, `args`), `answer` (`text`), `stop`
//! (`reason`) and `cut_off`. A
//! change of their form is named in the spec of the change.
//!
//! The cards (D5, D8), over `org.freedesktop.Notifications`, the app name
//! `agentLIBRE`, the body inert (KS-034's hint), the title the session's
//! name or its id:
//! - one card a session, replaced in place: what the turn does now
//!   (thinking, a tool and its argument, writing the answer, compacting),
//!   with Stop, no expiry; at the turn's end the answer's beginning, the
//!   field to answer in (KS-034's protocol) and the buttons Terminal (the
//!   window that runs that agl, focused through Hyprland) and Session (its
//!   file, by `xdg-open`);
//! - a stop other than the human's, a critical card of its own with the
//!   field, Continue (a message to go on), Terminal and Session (D10); a
//!   cut-off, a critical card with no buttons. A tool that failed or timed
//!   out makes no card: the turn goes on and the agent sees it (D11).

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use serde_json::Value as Json;
use zbus::blocking::{Connection, MessageIterator};
use zbus::message::Type as MessageType;
use zbus::zvariant::Value;

use crate::control::{self, Hello, Reply, Request};
use crate::error::Error;
use crate::i18n::{Lang, m};
use crate::paths::Paths;
use crate::tr;

/// The notification server's names.
const NAME: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
/// katarsa's own interface, where an answer comes back (KS-034 D5).
const KATARSA: &str = "org.katarsa.Notifications";
/// The capability and the action key of the field to answer in, and the
/// hint of a body not searched for links (KS-034 D4, D5).
const REPLY: &str = "x-katarsa-reply";
const INERT: &str = "x-katarsa-inert";
/// The cards' buttons.
const STOP: &str = "stop";
const TERMINAL: &str = "terminal";
const SESSION: &str = "session";
const CONTINUE: &str = "continue";
/// How often the sockets and the files are looked at.
const TICK: Duration = Duration::from_millis(250);
/// The most of an answer an end card shows.
const ANSWER_LINES: usize = 4;
const ANSWER_CHARS: usize = 300;
/// The most of a tool's argument the live card shows.
const ARGUMENT_CHARS: usize = 60;

/// Runs the bridge until the session bus goes.
pub fn run(paths: &Paths, lang: Lang) -> Result<ExitCode, Error> {
    let runtime = paths.runtime.clone().ok_or(Error::NotifyNoRuntime)?;
    let failed = |error: zbus::Error| Error::Notify {
        detail: error.to_string(),
    };
    let bus = Connection::session().map_err(failed)?;
    let (signals, received) = mpsc::channel();
    listen(&bus, NAME, "ActionInvoked", signals.clone(), Signal::Action).map_err(failed)?;
    listen(&bus, KATARSA, "Replied", signals, Signal::Replied).map_err(failed)?;
    let mut desk = Bus { bus };
    let mut bridge = Bridge::new(
        paths.sessions(),
        control::sockets_dir(&runtime),
        lang,
        hyprland(),
    );
    loop {
        bridge.scan(&mut desk);
        bridge.follow(&mut desk);
        match received.recv_timeout(TICK) {
            Ok(signal) => bridge.signal(signal, &mut desk),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(Error::Notify {
                    detail: "the bus went".into(),
                });
            }
        }
    }
}

/// What the notification server tells.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Signal {
    /// A button: the card's id and the action's key.
    Action(u32, String),
    /// An answer typed in a card's field (KS-034 D5).
    Replied(u32, String),
}

/// Sends `interface`'s signal `member`, made into a [`Signal`] by `make`,
/// to `signals`, from a thread of its own.
fn listen(
    bus: &Connection,
    interface: &'static str,
    member: &'static str,
    signals: Sender<Signal>,
    make: fn(u32, String) -> Signal,
) -> zbus::Result<()> {
    let rule = zbus::MatchRule::builder()
        .msg_type(MessageType::Signal)
        .interface(interface)?
        .member(member)?
        .build();
    let messages = MessageIterator::for_match_rule(rule, bus, Some(64))?;
    thread::spawn(move || {
        for message in messages.flatten() {
            if let Ok((id, text)) = message.body().deserialize::<(u32, String)>()
                && signals.send(make(id, text)).is_err()
            {
                return;
            }
        }
    });
    Ok(())
}

/// A card to show.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Card {
    /// The card it replaces; 0: a new one.
    replaces: u32,
    summary: String,
    body: String,
    /// Key and label.
    actions: Vec<(String, String)>,
    critical: bool,
}

/// The notification server, as the bridge uses it.
trait Desk {
    /// Shows `card`; its id.
    fn notify(&mut self, card: &Card) -> u32;
    /// Closes the card `id`.
    fn close(&mut self, id: u32);
}

/// The session bus's notification server.
struct Bus {
    bus: Connection,
}

impl Desk for Bus {
    fn notify(&mut self, card: &Card) -> u32 {
        let actions: Vec<&str> = card
            .actions
            .iter()
            .flat_map(|(key, label)| [key.as_str(), label.as_str()])
            .collect();
        let mut hints: HashMap<&str, Value> = HashMap::new();
        hints.insert(INERT, Value::Bool(true));
        hints.insert("urgency", Value::U8(if card.critical { 2 } else { 1 }));
        let body = (
            "agentLIBRE",
            card.replaces,
            "",
            card.summary.as_str(),
            card.body.as_str(),
            actions,
            hints,
            // Never expires: a live card goes with its turn, an end card
            // waits for the answer, a critical one for the human.
            0i32,
        );
        match self
            .bus
            .call_method(Some(NAME), PATH, Some(NAME), "Notify", &body)
            .and_then(|reply| reply.body().deserialize::<u32>())
        {
            Ok(id) => id,
            Err(error) => {
                eprintln!("agl notify: Notify: {error}");
                card.replaces
            }
        }
    }

    fn close(&mut self, id: u32) {
        let _ = self
            .bus
            .call_method(Some(NAME), PATH, Some(NAME), "CloseNotification", &(id,));
    }
}

/// What a record of a session file does to its cards.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Act {
    /// The live card shows what the turn does now.
    Live(String),
    /// The turn answered: the end card.
    End(String),
    /// The turn stopped without the human: a critical card of its own,
    /// answered from it (D10).
    Stopped(String),
    /// A critical card of its own, without buttons.
    Critical(String),
    /// The turn ended without an answer: the live card goes.
    Close,
}

/// What the bridge keeps of a session file's records between them.
#[derive(Default)]
struct Records {
    /// The last call's tool and its argument in short.
    last_call: Option<(String, String)>,
}

impl Records {
    /// What the record `json` does (D5).
    fn read(&mut self, json: &Json, lang: Lang) -> Vec<Act> {
        let text = |key: &str| json[key].as_str().unwrap_or_default();
        match text("type") {
            "user" => vec![Act::Live(tr!(lang, m::NOTIFY_THINKING).to_string())],
            "transition" => match text("to") {
                "Writing" => vec![Act::Live(tr!(lang, m::NOTIFY_WRITING).to_string())],
                "Compacting" => vec![Act::Live(tr!(lang, m::NOTIFY_COMPACTING).to_string())],
                "Running" => {
                    let tool = text("tool");
                    let shown = match &self.last_call {
                        Some((called, argument)) if called == tool && !argument.is_empty() => {
                            format!("{tool} {argument}")
                        }
                        _ => tool.to_owned(),
                    };
                    vec![Act::Live(shown)]
                }
                "Ready" if text("event") == "failed" => vec![Act::Close],
                // The answer, the stop and the cut-off have records of their
                // own; idle is no turn.
                "Ready" => Vec::new(),
                _ => vec![Act::Live(tr!(lang, m::NOTIFY_THINKING).to_string())],
            },
            "call" => {
                self.last_call = Some((text("tool").to_owned(), argument(&json["args"])));
                Vec::new()
            }
            "answer" => vec![Act::End(text("text").to_owned())],
            "stop" => match text("reason") {
                "interrupted" => vec![Act::Close],
                reason => vec![
                    Act::Close,
                    Act::Stopped(tr!(lang, m::NOTIFY_STOPPED, reason = reason)),
                ],
            },
            "cut_off" => vec![Act::Critical(tr!(lang, m::NOTIFY_CUT_OFF).to_string())],
            _ => Vec::new(),
        }
    }
}

/// A call's argument in short: its first string value's first line, or the
/// arguments as JSON, at most [`ARGUMENT_CHARS`].
fn argument(args: &Json) -> String {
    let first = args
        .as_object()
        .and_then(|object| object.values().find_map(Json::as_str))
        .map(|text| text.lines().next().unwrap_or_default().trim().to_owned())
        .unwrap_or_else(|| match args {
            Json::Null => String::new(),
            other => other.to_string(),
        });
    cut(&first, ARGUMENT_CHARS)
}

/// `text` cut to `chars` characters, an ellipsis ending what was cut.
fn cut(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_owned();
    }
    let kept: String = text.chars().take(chars.saturating_sub(1)).collect();
    format!("{}…", kept.trim_end())
}

/// An answer's beginning: its first lines, at most [`ANSWER_CHARS`].
fn beginning(answer: &str) -> String {
    let lines: Vec<&str> = answer
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(ANSWER_LINES)
        .collect();
    let more = answer
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
        > lines.len();
    let text = cut(&lines.join("\n"), ANSWER_CHARS);
    if more && !text.ends_with('…') {
        format!("{text}\n…")
    } else {
        text
    }
}

/// A session followed.
struct Followed {
    socket: PathBuf,
    file: PathBuf,
    hello: Hello,
    /// How far the file is read, and what of a line is read so far.
    offset: u64,
    partial: Vec<u8>,
    records: Records,
    /// The id of its card; 0 before one shows.
    card: u32,
    /// What its card shows now.
    shown: Option<Card>,
    /// The end card's answer, kept to show a queued message under it.
    answer: Option<String>,
    /// The id of its last stop's card (D10); 0 before one shows.
    stopped: u32,
}

impl Followed {
    fn title(&self) -> String {
        self.hello
            .name
            .clone()
            .unwrap_or_else(|| self.hello.session.clone())
    }
}

/// The bridge: the sessions it follows and what it tells of them.
struct Bridge {
    sessions: PathBuf,
    sockets: PathBuf,
    lang: Lang,
    /// Hyprland's socket, when it runs: the Terminal button needs it.
    hyprland: Option<PathBuf>,
    followed: HashMap<PathBuf, Followed>,
}

impl Bridge {
    fn new(sessions: PathBuf, sockets: PathBuf, lang: Lang, hyprland: Option<PathBuf>) -> Bridge {
        Bridge {
            sessions,
            sockets,
            lang,
            hyprland,
            followed: HashMap::new(),
        }
    }

    /// Follows the sessions whose sockets came and lets go of those whose
    /// sockets went, their cards closed.
    fn scan(&mut self, desk: &mut dyn Desk) {
        let found: HashSet<PathBuf> = std::fs::read_dir(&self.sockets)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| control::session_of(path).is_some())
                    .collect()
            })
            .unwrap_or_default();
        let gone: Vec<PathBuf> = self
            .followed
            .keys()
            .filter(|path| !found.contains(*path))
            .cloned()
            .collect();
        for path in gone {
            if let Some(followed) = self.followed.remove(&path)
                && followed.card != 0
            {
                desk.close(followed.card);
            }
        }
        for socket in found {
            if self.followed.contains_key(&socket) {
                continue;
            }
            // One that does not answer is an agl that ended; it is tried
            // again at the next scan.
            let Ok(Reply::Hello(hello)) = control::ask(&socket, &Request::Hello) else {
                continue;
            };
            let file = self.sessions.join(format!("{}.jsonl", hello.session));
            let offset = std::fs::metadata(&file).map_or(0, |meta| meta.len());
            self.followed.insert(
                socket.clone(),
                Followed {
                    socket,
                    file,
                    hello,
                    offset,
                    partial: Vec::new(),
                    records: Records::default(),
                    card: 0,
                    shown: None,
                    answer: None,
                    stopped: 0,
                },
            );
        }
    }

    /// Reads what each followed file got since, and tells it.
    fn follow(&mut self, desk: &mut dyn Desk) {
        let sockets: Vec<PathBuf> = self.followed.keys().cloned().collect();
        for socket in sockets {
            let lines = {
                let followed = self.followed.get_mut(&socket).expect("listed above");
                read_lines(followed)
            };
            for line in lines {
                let Ok(json) = serde_json::from_str::<Json>(&line) else {
                    continue;
                };
                let followed = self.followed.get_mut(&socket).expect("listed above");
                let acts = followed.records.read(&json, self.lang);
                for act in acts {
                    self.act(&socket, act, desk);
                }
            }
        }
    }

    /// Does `act` for the session of `socket`.
    fn act(&mut self, socket: &Path, act: Act, desk: &mut dyn Desk) {
        let lang = self.lang;
        let hyprland = self.hyprland.is_some();
        let Some(followed) = self.followed.get_mut(socket) else {
            return;
        };
        let title = followed.title();
        let card = match act {
            Act::Live(doing) => {
                followed.answer = None;
                Card {
                    replaces: followed.card,
                    summary: title,
                    body: doing,
                    actions: vec![(STOP.into(), tr!(lang, m::NOTIFY_STOP).into())],
                    critical: false,
                }
            }
            Act::End(answer) => {
                let card = end_card(followed.card, title, &beginning(&answer), lang, hyprland);
                followed.answer = Some(answer);
                card
            }
            Act::Stopped(text) => {
                let mut actions = vec![
                    (REPLY.to_owned(), tr!(lang, m::NOTIFY_ANSWER).to_string()),
                    (CONTINUE.into(), tr!(lang, m::NOTIFY_CONTINUE).into()),
                ];
                if hyprland {
                    actions.push((TERMINAL.into(), tr!(lang, m::NOTIFY_TERMINAL).into()));
                }
                actions.push((SESSION.into(), tr!(lang, m::NOTIFY_SESSION).into()));
                followed.stopped = desk.notify(&Card {
                    replaces: 0,
                    summary: title,
                    body: text,
                    actions,
                    critical: true,
                });
                return;
            }
            Act::Critical(text) => {
                desk.notify(&Card {
                    replaces: 0,
                    summary: title,
                    body: text,
                    actions: Vec::new(),
                    critical: true,
                });
                return;
            }
            Act::Close => {
                if followed.card != 0 {
                    desk.close(followed.card);
                }
                followed.shown = None;
                followed.answer = None;
                return;
            }
        };
        // The same text again is no new card.
        if followed.shown.as_ref() == Some(&card) {
            return;
        }
        followed.card = desk.notify(&card);
        followed.shown = Some(Card {
            replaces: followed.card,
            ..card
        });
    }

    /// What a card's button or field asks.
    fn signal(&mut self, signal: Signal, desk: &mut dyn Desk) {
        let id = match &signal {
            Signal::Action(id, _) | Signal::Replied(id, _) => *id,
        };
        let Some(socket) = self
            .followed
            .iter()
            .find(|(_, followed)| id != 0 && (followed.card == id || followed.stopped == id))
            .map(|(socket, _)| socket.clone())
        else {
            return;
        };
        let lang = self.lang;
        let hyprland = self.hyprland.clone();
        let followed = self.followed.get_mut(&socket).expect("found above");
        let on_stop = followed.stopped == id && followed.card != id;
        match signal {
            Signal::Action(_, key) if key == STOP => {
                let _ = control::ask(&followed.socket, &Request::Stop);
            }
            Signal::Action(_, key) if key == TERMINAL => {
                if let Some(hyprland) = &hyprland {
                    focus_terminal(hyprland, followed.hello.pid);
                }
            }
            Signal::Action(_, key) if key == SESSION => open(&followed.file),
            Signal::Action(_, key) if key == CONTINUE => {
                let text = tr!(lang, m::NOTIFY_CONTINUE_TEXT).to_string();
                let _ = control::ask(&followed.socket, &Request::Say(text));
            }
            Signal::Action(..) => {}
            Signal::Replied(_, text) => {
                let said = control::ask(&followed.socket, &Request::Say(text.clone()));
                // While a turn runs it waits, and its card says so (D4); an
                // idle session starts the turn, whose live card follows.
                if let Ok(Reply::Said { queued: true }) = said
                    && !on_stop
                {
                    let answer = followed.answer.clone().unwrap_or_default();
                    let body = format!(
                        "{}\n{}",
                        beginning(&answer),
                        tr!(lang, m::NOTIFY_QUEUED, text = cut(&text, ARGUMENT_CHARS))
                    );
                    let card = Card {
                        replaces: followed.card,
                        summary: followed.title(),
                        body: body.trim_start().to_owned(),
                        actions: Vec::new(),
                        critical: false,
                    };
                    followed.card = desk.notify(&card);
                    followed.shown = Some(card);
                }
            }
        }
    }
}

/// The end card of a turn (D5, D8): the answer's beginning, the field to
/// answer in, Terminal (where Hyprland runs) and Session.
fn end_card(replaces: u32, summary: String, body: &str, lang: Lang, hyprland: bool) -> Card {
    let mut actions = vec![(REPLY.to_owned(), tr!(lang, m::NOTIFY_ANSWER).to_string())];
    if hyprland {
        actions.push((TERMINAL.into(), tr!(lang, m::NOTIFY_TERMINAL).into()));
    }
    actions.push((SESSION.into(), tr!(lang, m::NOTIFY_SESSION).into()));
    Card {
        replaces,
        summary,
        body: body.to_owned(),
        actions,
        critical: false,
    }
}

/// The complete lines appended to `followed`'s file since it was last read.
fn read_lines(followed: &mut Followed) -> Vec<String> {
    let Ok(mut file) = File::open(&followed.file) else {
        return Vec::new();
    };
    let length = file.metadata().map_or(0, |meta| meta.len());
    if length < followed.offset {
        // Not appended to: read again from its start.
        followed.offset = 0;
        followed.partial.clear();
    }
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(followed.offset)).is_err() || file.read_to_end(&mut bytes).is_err()
    {
        return Vec::new();
    }
    followed.offset += bytes.len() as u64;
    followed.partial.extend_from_slice(&bytes);
    let Some(last) = followed.partial.iter().rposition(|byte| *byte == b'\n') else {
        return Vec::new();
    };
    let complete: Vec<u8> = followed.partial.drain(..=last).collect();
    String::from_utf8_lossy(&complete)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}

/// Hyprland's command socket, when it runs.
fn hyprland() -> Option<PathBuf> {
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let socket = PathBuf::from(runtime)
        .join("hypr")
        .join(signature)
        .join(".socket.sock");
    socket.exists().then_some(socket)
}

/// Asks Hyprland's socket `request`; its answer.
fn hyprctl(socket: &Path, request: &str) -> Option<String> {
    let mut stream = UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream.write_all(request.as_bytes()).ok()?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer).ok()?;
    Some(answer)
}

/// Focuses the window that runs the agl of `pid` (D8): the window whose
/// process is `pid` or an ancestor of it.
fn focus_terminal(hyprland: &Path, pid: u32) {
    let Some(clients) = hyprctl(hyprland, "j/clients") else {
        return;
    };
    let pids = client_pids(&clients);
    if let Some(window) = window_of(pid, &pids, parent) {
        let _ = hyprctl(hyprland, &format!("dispatch focuswindow pid:{window}"));
    }
}

/// The processes of Hyprland's clients, from `j/clients`.
fn client_pids(clients: &str) -> Vec<u32> {
    serde_json::from_str::<Json>(clients)
        .ok()
        .and_then(|json| json.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|client| u32::try_from(client["pid"].as_i64()?).ok())
        .collect()
}

/// The first of `pid` and its ancestors that is one of `windows`.
fn window_of(pid: u32, windows: &[u32], parent: impl Fn(u32) -> Option<u32>) -> Option<u32> {
    let mut at = pid;
    for _ in 0..64 {
        if windows.contains(&at) {
            return Some(at);
        }
        at = parent(at).filter(|parent| *parent > 1)?;
    }
    None
}

/// The parent of `pid`, from `/proc/<pid>/stat`.
fn parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command's name is in parentheses and may hold spaces.
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(1)?.parse().ok()
}

/// Opens `file` by the desktop's handler, detached.
fn open(file: &Path) {
    if let Ok(mut child) = Command::new("xdg-open")
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use serde_json::json;

    use super::*;

    const LANG: Lang = Lang::En;

    fn acts(records: &mut Records, json: Json) -> Vec<Act> {
        records.read(&json, LANG)
    }

    #[test]
    fn records_tell_what_the_turn_does() {
        let mut records = Records::default();
        let live = |text: &str| vec![Act::Live(text.into())];
        assert_eq!(
            acts(&mut records, json!({"type": "user", "text": "hi"})),
            live("thinking")
        );
        let to = |to: &str| json!({"type": "transition", "to": to, "event": "x"});
        assert_eq!(acts(&mut records, to("Prefill")), live("thinking"));
        assert_eq!(
            acts(&mut records, to("Writing")),
            live("writing the answer")
        );
        assert_eq!(acts(&mut records, to("Compacting")), live("compacting"));
        let call = json!({"type": "call", "id": "c1", "tool": "bash",
                          "args": {"command": "cargo test\n--quiet"}});
        assert!(acts(&mut records, call).is_empty());
        let running = json!({"type": "transition", "to": "Running", "tool": "bash"});
        assert_eq!(acts(&mut records, running), live("bash cargo test"));
        assert!(
            acts(
                &mut records,
                json!({"type": "result", "id": "c1", "outcome": "ok"})
            )
            .is_empty()
        );
        // A failed tool makes no card: the turn goes on (D11).
        for outcome in ["fail", "timeout"] {
            let result = json!({"type": "result", "id": "c1", "outcome": outcome});
            assert!(acts(&mut records, result).is_empty(), "{outcome}");
        }
        assert_eq!(
            acts(&mut records, json!({"type": "answer", "text": "done"})),
            [Act::End("done".into())]
        );
        assert!(
            acts(
                &mut records,
                json!({"type": "transition", "to": "Ready", "event": "answer"})
            )
            .is_empty()
        );
        assert_eq!(
            acts(
                &mut records,
                json!({"type": "transition", "to": "Ready", "event": "failed"})
            ),
            [Act::Close]
        );
        assert_eq!(
            acts(
                &mut records,
                json!({"type": "stop", "reason": "interrupted"})
            ),
            [Act::Close]
        );
        assert_eq!(
            acts(
                &mut records,
                json!({"type": "stop", "reason": "step_limit"})
            ),
            [Act::Close, Act::Stopped("stopped: step_limit".into())]
        );
        assert_eq!(
            acts(&mut records, json!({"type": "cut_off", "call": false})),
            [Act::Critical(
                "a generation was cut off at its token budget".into()
            )]
        );
        assert!(acts(&mut records, json!({"type": "prefill"})).is_empty());
    }

    #[test]
    fn arguments_and_answers_are_cut_short() {
        assert_eq!(argument(&json!({"path": "/a/b"})), "/a/b");
        assert_eq!(argument(&json!({"n": 3})), r#"{"n":3}"#);
        assert_eq!(argument(&Json::Null), "");
        assert_eq!(
            argument(&json!({"q": "x".repeat(100)})).chars().count(),
            ARGUMENT_CHARS
        );
        assert_eq!(beginning("one\n\ntwo"), "one\ntwo");
        assert_eq!(beginning("1\n2\n3\n4\n5"), "1\n2\n3\n4\n…");
        assert!(beginning(&"word ".repeat(100)).ends_with('…'));
    }

    #[test]
    fn the_terminal_is_the_nearest_ancestor_with_a_window() {
        let clients = r#"[{"pid": 100, "class": "kitty"}, {"pid": 7, "class": "firefox"}]"#;
        let pids = client_pids(clients);
        assert_eq!(pids, [100, 7]);
        // agl 300 under a shell 200 under the terminal 100.
        let parents = HashMap::from([(300, 200), (200, 100), (100, 1)]);
        let parent = |pid| parents.get(&pid).copied();
        assert_eq!(window_of(300, &pids, parent), Some(100));
        assert_eq!(window_of(300, &[], parent), None);
        assert_eq!(client_pids("not json"), Vec::<u32>::new());
        assert!(
            super::parent(std::process::id()).is_some(),
            "this test's own parent"
        );
    }

    /// The cards shown and closed.
    #[derive(Default)]
    struct Fake {
        shown: Vec<Card>,
        closed: Vec<u32>,
        next: u32,
    }

    impl Desk for Fake {
        fn notify(&mut self, card: &Card) -> u32 {
            self.shown.push(card.clone());
            if card.replaces != 0 {
                card.replaces
            } else {
                self.next += 1;
                self.next
            }
        }

        fn close(&mut self, id: u32) {
            self.closed.push(id);
        }
    }

    /// A fake session: its socket answering `hello` and recording the rest,
    /// its file.
    struct Session {
        dir: PathBuf,
        file: PathBuf,
        asked: std::sync::Arc<std::sync::Mutex<Vec<Request>>>,
        busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl Session {
        fn new(name: &str) -> Session {
            let dir =
                std::env::temp_dir().join(format!("agl-notify-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("sockets")).unwrap();
            std::fs::create_dir_all(dir.join("sessions")).unwrap();
            let file = dir.join("sessions/s1.jsonl");
            std::fs::write(
                &file,
                "{\"type\":\"session\"}\n{\"type\":\"user\",\"text\":\"old\"}\n",
            )
            .unwrap();
            let listener = UnixListener::bind(dir.join("sockets/s1.sock")).unwrap();
            let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            {
                let asked = asked.clone();
                let busy = busy.clone();
                thread::spawn(move || {
                    for stream in listener.incoming().flatten() {
                        let _ = control::serve(stream, &mut |request| {
                            asked.lock().unwrap().push(request.clone());
                            match request {
                                Request::Hello => Reply::Hello(Hello {
                                    pid: 300,
                                    session: "s1".into(),
                                    name: Some("tea".into()),
                                    busy: false,
                                }),
                                Request::Say(_) => Reply::Said {
                                    queued: busy.load(std::sync::atomic::Ordering::SeqCst),
                                },
                                Request::Stop => Reply::Stopped,
                            }
                        });
                    }
                });
            }
            Session {
                dir,
                file,
                asked,
                busy,
            }
        }

        fn append(&self, line: Json) {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&self.file)
                .unwrap();
            writeln!(file, "{line}").unwrap();
        }

        fn bridge(&self) -> Bridge {
            Bridge::new(
                self.dir.join("sessions"),
                self.dir.join("sockets"),
                LANG,
                None,
            )
        }
    }

    #[test]
    fn a_session_has_one_card_replaced_in_place_and_is_answered_from_it() {
        let session = Session::new("cards");
        let mut desk = Fake::default();
        let mut bridge = session.bridge();
        bridge.scan(&mut desk);
        bridge.follow(&mut desk);
        assert!(
            desk.shown.is_empty(),
            "followed from its end: the old turn is not told"
        );

        session.append(json!({"type": "user", "text": "go"}));
        session.append(json!({"type": "transition", "to": "Prefill", "event": "step_start"}));
        session.append(json!({"type": "transition", "to": "Thinking", "event": "first_thinking"}));
        bridge.follow(&mut desk);
        assert_eq!(desk.shown.len(), 1, "thinking once, not thrice");
        let live = &desk.shown[0];
        assert_eq!(
            (live.replaces, live.summary.as_str(), live.body.as_str()),
            (0, "tea", "thinking")
        );
        assert_eq!(live.actions, [("stop".to_owned(), "Stop".to_owned())]);

        // A line written in two parts is read once whole.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&session.file)
            .unwrap();
        write!(file, "{{\"type\":\"transition\",").unwrap();
        bridge.follow(&mut desk);
        writeln!(file, "\"to\":\"Writing\"}}").unwrap();
        bridge.follow(&mut desk);
        assert_eq!(desk.shown.len(), 2);
        assert_eq!(
            (desk.shown[1].replaces, desk.shown[1].body.as_str()),
            (1, "writing the answer")
        );

        session.append(json!({"type": "answer", "text": "It is done."}));
        bridge.follow(&mut desk);
        let end = desk.shown.last().unwrap();
        assert_eq!((end.replaces, end.body.as_str()), (1, "It is done."));
        let keys: Vec<&str> = end.actions.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, [REPLY, SESSION], "no Terminal without Hyprland");

        // An answer while a turn runs waits, and the card says so.
        bridge.signal(Signal::Replied(1, "and then?".into()), &mut desk);
        assert!(
            session
                .asked
                .lock()
                .unwrap()
                .contains(&Request::Say("and then?".into()))
        );
        let queued = desk.shown.last().unwrap();
        assert_eq!(queued.body, "It is done.\nqueued: and then?");
        assert!(queued.actions.is_empty());
        session
            .busy
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let shown = desk.shown.len();
        bridge.signal(Signal::Replied(1, "go on".into()), &mut desk);
        assert_eq!(
            desk.shown.len(),
            shown,
            "an idle session's turn tells itself"
        );

        bridge.signal(Signal::Action(1, STOP.into()), &mut desk);
        bridge.signal(Signal::Action(99, STOP.into()), &mut desk);
        let stops = session
            .asked
            .lock()
            .unwrap()
            .iter()
            .filter(|r| **r == Request::Stop)
            .count();
        assert_eq!(stops, 1, "a card of another program is not ours");

        session.append(json!({"type": "stop", "reason": "server_error"}));
        bridge.follow(&mut desk);
        assert_eq!(desk.closed, [1]);
        let critical = desk.shown.last().unwrap();
        assert!(critical.critical && critical.replaces == 0);
        assert_eq!(critical.body, "stopped: server_error");
        // Answered from it (D10): the field, Continue, Session.
        let keys: Vec<&str> = critical
            .actions
            .iter()
            .map(|(key, _)| key.as_str())
            .collect();
        assert_eq!(keys, [REPLY, CONTINUE, SESSION]);
        let stop_card = bridge.followed.values().next().unwrap().stopped;
        assert_eq!(stop_card, 2);
        let shown = desk.shown.len();
        bridge.signal(Signal::Action(stop_card, CONTINUE.into()), &mut desk);
        bridge.signal(
            Signal::Replied(stop_card, "try another way".into()),
            &mut desk,
        );
        let asked = session.asked.lock().unwrap().clone();
        assert!(asked.contains(&Request::Say("Continue.".into())));
        assert!(asked.contains(&Request::Say("try another way".into())));
        assert_eq!(
            desk.shown.len(),
            shown,
            "a queued answer on it changes no card"
        );

        // The socket goes: the session is let go of, its card closed.
        std::fs::remove_file(session.dir.join("sockets/s1.sock")).unwrap();
        bridge.scan(&mut desk);
        assert!(bridge.followed.is_empty());
        assert_eq!(desk.closed, [1, 1]);
        let _ = std::fs::remove_dir_all(&session.dir);
    }
}
