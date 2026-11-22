//! The desktop notifier (AGL-074, D1, D2, D5, D6, D8; AGL-121, D23): the
//! running sessions told on the desktop, and answered from it. The crate of
//! the `agl-notify` binary; `agl notify` only launches it.
//!
//! The notifier is a client of the daemon (AGL-076, D3): it subscribes to
//! every session and draws what each does from the protocol, not from the
//! session files; what goes in goes back through the daemon: an answer typed
//! in a card is `say`, its Stop is `stop`. With no daemon running it waits
//! for one, and its cards close when the daemon goes.
//!
//! The cards (D5, D8), over `org.freedesktop.Notifications`, the app name
//! `agentLIBRE`, the body inert (KS-034's hint), the title the session's
//! name or its id:
//! - one card a session, replaced in place: what the turn does now
//!   (thinking, a tool and its argument, writing the answer, compacting),
//!   with Stop, no expiry; at the turn's end the answer's beginning, the
//!   field to answer in (KS-034's protocol) and the buttons Terminal (the
//!   window of the client that holds the session, focused through Hyprland)
//!   and Session (its file, by `xdg-open`);
//! - a stop other than the human's, a critical card of its own with the
//!   field, Continue (a message to go on), Terminal and Session (D10); a
//!   cut-off, a critical card with no buttons. A tool that failed or timed
//!   out makes no card: the turn goes on and the agent sees it (D11).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use agentlibre_core::client::Client;
use agentlibre_proto::{
    Event, Phase, PhaseEvent, Reply, Report, Request, Sent, SessionInfo, StopReason,
};
use serde_json::Value as Json;
use zbus::blocking::{Connection, MessageIterator};
use zbus::message::Type as MessageType;
use zbus::zvariant::Value;

use agentlibre_core::error::Error;
use agentlibre_core::i18n::{Lang, m};
use agentlibre_core::paths::Paths;
use agentlibre_core::tr;

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
/// How often the notifier wakes with nothing received.
const TICK: Duration = Duration::from_millis(250);
/// How often the daemon is asked for its sessions (the closed ones' cards
/// go), and how often a daemon that does not answer is tried again.
const LOOK: Duration = Duration::from_secs(1);
const RETRY: Duration = Duration::from_secs(2);
/// The most of an answer an end card shows.
const ANSWER_LINES: usize = 4;
const ANSWER_CHARS: usize = 300;
/// The most of a tool's argument the live card shows.
const ARGUMENT_CHARS: usize = 60;

/// What the notifier waits on: the notification server and the daemon.
enum Input {
    Signal(Signal),
    Reply(Box<Reply>),
    /// The daemon's connection ended.
    Gone,
}

/// Runs the notifier until the session bus goes.
pub fn run(paths: &Paths, lang: Lang) -> Result<ExitCode, Error> {
    if paths.runtime.is_none() {
        return Err(Error::DaemonNoRuntime);
    }
    let failed = |error: zbus::Error| Error::Notify {
        detail: error.to_string(),
    };
    let bus = Connection::session().map_err(failed)?;
    let (inputs, received) = mpsc::channel();
    listen(&bus, NAME, "ActionInvoked", inputs.clone(), Signal::Action).map_err(failed)?;
    listen(&bus, KATARSA, "Replied", inputs.clone(), Signal::Replied).map_err(failed)?;
    let mut desk = Bus { bus };
    let mut notifier = Notifier::new(paths.sessions(), lang, hyprland());
    let mut client: Option<Arc<Client>> = None;
    let mut tried = None::<Instant>;
    let mut looked = Instant::now();
    loop {
        if client.is_none() && tried.is_none_or(|at| at.elapsed() >= RETRY) {
            tried = Some(Instant::now());
            // The daemon is not started from here: no session, nothing to tell.
            if let Ok(connected) = Client::connect(paths, false, lang, false) {
                let connected = Arc::new(connected);
                let reader = Arc::clone(&connected);
                let to = inputs.clone();
                thread::spawn(move || {
                    while let Ok(reply) = reader.next() {
                        if to.send(Input::Reply(Box::new(reply))).is_err() {
                            return;
                        }
                    }
                    let _ = to.send(Input::Gone);
                });
                let _ = connected.send(&Request::Subscribe { session: None });
                let _ = connected.send(&Request::Sessions);
                client = Some(connected);
            }
        }
        match received.recv_timeout(TICK) {
            Ok(Input::Signal(signal)) => {
                if let Some(client) = &client {
                    notifier.signal(signal, &mut desk, &mut Daemon(client));
                }
            }
            Ok(Input::Reply(reply)) => {
                if let Some(client) = &client {
                    notifier.reply(*reply, &mut desk, &mut Daemon(client));
                }
            }
            Ok(Input::Gone) => {
                notifier.lost(&mut desk);
                client = None;
                tried = Some(Instant::now());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(Error::Notify {
                    detail: "the bus went".into(),
                });
            }
        }
        if let Some(client) = &client
            && looked.elapsed() >= LOOK
        {
            looked = Instant::now();
            let _ = client.send(&Request::Sessions);
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
/// to `inputs`, from a thread of its own.
fn listen(
    bus: &Connection,
    interface: &'static str,
    member: &'static str,
    inputs: Sender<Input>,
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
                && inputs.send(Input::Signal(make(id, text))).is_err()
            {
                return;
            }
        }
    });
    Ok(())
}

/// The daemon, as the notifier asks it.
trait Agl {
    fn send(&mut self, request: Request);
}

/// The daemon's connection.
struct Daemon<'a>(&'a Client);

impl Agl for Daemon<'_> {
    fn send(&mut self, request: Request) {
        if let Err(error) = self.0.send(&request) {
            eprintln!("agl notify: {error:?}");
        }
    }
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

/// The notification server, as the notifier uses it.
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

/// What a session's report does to its cards.
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
    /// The plane is idle again: a live card left goes, an end card stays.
    Idle,
}

/// What the notifier keeps of a session's reports between them.
#[derive(Default)]
struct Records {
    /// The last call's tool and its argument in short.
    last_call: Option<(String, String)>,
}

impl Records {
    /// What `sent` does (D5).
    fn read(&mut self, sent: &Sent, lang: Lang) -> Vec<Act> {
        let thinking = || Act::Live(tr!(lang, m::NOTIFY_THINKING).to_string());
        match sent {
            Sent::Report(Report::Event(Event::Call { call, summary, .. })) => {
                // AGL-121, D52: the summary rides the wire; notify keeps
                // no copy of its own to make.
                self.last_call = Some((
                    call.name.clone(),
                    cut(summary.as_deref().unwrap_or_default(), ARGUMENT_CHARS),
                ));
                Vec::new()
            }
            Sent::Report(Report::Event(Event::Answer(text))) => vec![Act::End(text.clone())],
            Sent::Report(Report::Event(Event::CutOff(_))) => {
                vec![Act::Critical(tr!(lang, m::NOTIFY_CUT_OFF).to_string())]
            }
            Sent::Report(Report::Transition(transition)) => {
                match (&transition.to, &transition.event) {
                    (
                        Phase::Ready,
                        PhaseEvent::Failed | PhaseEvent::Stop(StopReason::Interrupted),
                    ) => {
                        vec![Act::Close]
                    }
                    (Phase::Ready, PhaseEvent::Stop(reason)) => vec![
                        Act::Close,
                        Act::Stopped(tr!(lang, m::NOTIFY_STOPPED, reason = reason_name(*reason))),
                    ],
                    (Phase::Ready, _) => vec![Act::Idle],
                    // The server's start before a session's first turn is no turn.
                    (Phase::StartingServer, _) => Vec::new(),
                    (Phase::Writing, _) => {
                        vec![Act::Live(tr!(lang, m::NOTIFY_WRITING).to_string())]
                    }
                    (Phase::Compacting, _) => {
                        vec![Act::Live(tr!(lang, m::NOTIFY_COMPACTING).to_string())]
                    }
                    (Phase::Running(tool), _) => {
                        let shown = match &self.last_call {
                            Some((called, argument)) if called == tool && !argument.is_empty() => {
                                format!("{tool} {argument}")
                            }
                            _ => tool.clone(),
                        };
                        vec![Act::Live(shown)]
                    }
                    _ => vec![thinking()],
                }
            }
            _ => Vec::new(),
        }
    }
}

/// A stop's reason as the session's file names it (`step_limit`…).
fn reason_name(reason: StopReason) -> String {
    serde_json::to_value(reason)
        .ok()
        .and_then(|json| json.as_str().map(str::to_owned))
        .unwrap_or_default()
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
#[derive(Default)]
struct Followed {
    name: Option<String>,
    /// The process of a client that holds it: its window is the Terminal.
    pid: Option<u32>,
    /// Whether a turn runs: an answer typed now waits for its end (D4).
    busy: bool,
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

/// The notifier: the sessions it follows and what it tells of them.
struct Notifier {
    sessions: PathBuf,
    lang: Lang,
    /// Hyprland's socket, when it runs: the Terminal button needs it.
    hyprland: Option<PathBuf>,
    /// By the session's id.
    followed: HashMap<String, Followed>,
}

impl Notifier {
    fn new(sessions: PathBuf, lang: Lang, hyprland: Option<PathBuf>) -> Notifier {
        Notifier {
            sessions,
            lang,
            hyprland,
            followed: HashMap::new(),
        }
    }

    /// What the daemon sent.
    fn reply(&mut self, reply: Reply, desk: &mut dyn Desk, agl: &mut dyn Agl) {
        match reply {
            Reply::Sessions { sessions } => self.sessions(&sessions, desk),
            // A line before a session is made has no session.
            Reply::Sent { session, .. } if session.is_empty() => {}
            Reply::Sent { session, sent } => {
                if !self.followed.contains_key(&session) {
                    self.followed.insert(session.clone(), Followed::default());
                    // Its name and its client's process.
                    agl.send(Request::Sessions);
                }
                let followed = self.followed.get_mut(&session).expect("inserted above");
                let acts = followed.records.read(&sent, self.lang);
                for act in acts {
                    self.act(&session, act, desk);
                }
            }
            Reply::Done { session, done, .. } => {
                let Some(mut followed) = self.followed.remove(&session) else {
                    return;
                };
                followed.busy = false;
                followed.name = done.session.name.clone();
                // `/new`: the cards follow the new session.
                self.followed.insert(done.session.id, followed);
            }
            Reply::Refused { error } => eprintln!("agl notify: {error}"),
            _ => {}
        }
    }

    /// The daemon's sessions: the names and processes learnt, the cards of
    /// those it no longer holds closed.
    fn sessions(&mut self, sessions: &[SessionInfo], desk: &mut dyn Desk) {
        let gone: Vec<String> = self
            .followed
            .keys()
            .filter(|id| !sessions.iter().any(|session| &session.id == *id))
            .cloned()
            .collect();
        for id in gone {
            if let Some(followed) = self.followed.remove(&id)
                && followed.card != 0
            {
                desk.close(followed.card);
            }
        }
        for session in sessions {
            let followed = self.followed.entry(session.id.clone()).or_default();
            followed.name = session.name.clone();
            followed.pid = session.pids.first().copied();
            followed.busy = session.busy;
        }
    }

    /// The daemon went: every card closed.
    fn lost(&mut self, desk: &mut dyn Desk) {
        for (_, followed) in self.followed.drain() {
            if followed.card != 0 {
                desk.close(followed.card);
            }
        }
    }

    /// Does `act` for the session `id`.
    fn act(&mut self, id: &str, act: Act, desk: &mut dyn Desk) {
        let lang = self.lang;
        let hyprland = self.hyprland.is_some();
        let Some(followed) = self.followed.get_mut(id) else {
            return;
        };
        let title = followed.name.clone().unwrap_or_else(|| id.to_owned());
        let card = match act {
            Act::Live(doing) => {
                followed.busy = true;
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
            Act::Close | Act::Idle => {
                followed.busy = false;
                if act == Act::Idle && followed.answer.is_some() {
                    return;
                }
                if followed.card != 0 && followed.shown.is_some() {
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
    fn signal(&mut self, signal: Signal, desk: &mut dyn Desk, agl: &mut dyn Agl) {
        let id = match &signal {
            Signal::Action(id, _) | Signal::Replied(id, _) => *id,
        };
        let Some(session) = self
            .followed
            .iter()
            .find(|(_, followed)| id != 0 && (followed.card == id || followed.stopped == id))
            .map(|(session, _)| session.clone())
        else {
            return;
        };
        let lang = self.lang;
        let file = self.sessions.join(format!("{session}.jsonl"));
        let hyprland = self.hyprland.clone();
        let followed = self.followed.get_mut(&session).expect("found above");
        let on_stop = followed.stopped == id && followed.card != id;
        let say = |message: String| Request::Say {
            session: session.clone(),
            message,
            attached: Vec::new(),
            reasoning: None,
        };
        match signal {
            Signal::Action(_, key) if key == STOP => agl.send(Request::Stop {
                session: session.clone(),
            }),
            Signal::Action(_, key) if key == TERMINAL => {
                if let (Some(hyprland), Some(pid)) = (&hyprland, followed.pid) {
                    focus_terminal(hyprland, pid);
                }
            }
            Signal::Action(_, key) if key == SESSION => open(&file),
            Signal::Action(_, key) if key == CONTINUE => {
                agl.send(say(tr!(lang, m::NOTIFY_CONTINUE_TEXT).to_string()));
            }
            Signal::Action(..) => {}
            Signal::Replied(_, text) => {
                agl.send(say(text.clone()));
                // While a turn runs it waits, and its card says so (D4); an
                // idle session starts the turn, whose live card follows.
                if followed.busy && !on_stop {
                    let answer = followed.answer.clone().unwrap_or_default();
                    let body = format!(
                        "{}\n{}",
                        beginning(&answer),
                        tr!(lang, m::NOTIFY_QUEUED, text = cut(&text, ARGUMENT_CHARS))
                    );
                    let card = Card {
                        replaces: followed.card,
                        summary: followed.name.clone().unwrap_or_else(|| session.clone()),
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
    use agentlibre_proto::{Done, SessionView, ToolCall, Transition, Value as Arg};

    use super::*;

    const LANG: Lang = Lang::En;

    fn to(to: Phase, event: PhaseEvent) -> Sent {
        Sent::Report(Report::Transition(Transition {
            from: Phase::Ready,
            to,
            event,
            micros: 0,
            violation: false,
        }))
    }

    fn event(event: Event) -> Sent {
        Sent::Report(Report::Event(event))
    }

    fn call(tool: &str, command: &str) -> Sent {
        event(Event::Call {
            text: None,
            // What the daemon of the table computes (AGL-121, D52).
            summary: Some(command.lines().next().unwrap_or_default().to_owned()),
            call: ToolCall {
                id: "c1".into(),
                name: tool.into(),
                args: vec![("command".into(), Arg::Text(command.into()))],
            },
        })
    }

    #[test]
    fn reports_tell_what_the_turn_does() {
        let mut records = Records::default();
        let mut acts = |sent: Sent| records.read(&sent, LANG);
        let live = |text: &str| vec![Act::Live(text.into())];
        assert_eq!(
            acts(to(Phase::Prefill, PhaseEvent::StepStart)),
            live("thinking")
        );
        assert_eq!(
            acts(to(Phase::Writing, PhaseEvent::FirstText)),
            live("writing the answer")
        );
        assert_eq!(
            acts(to(Phase::Compacting, PhaseEvent::CompactStart)),
            live("compacting")
        );
        assert!(acts(to(Phase::StartingServer, PhaseEvent::ServerStart)).is_empty());
        assert!(acts(call("bash", "cargo test\n--quiet")).is_empty());
        let running = to(
            Phase::Running("bash".into()),
            PhaseEvent::ToolStart {
                tool: "bash".into(),
            },
        );
        assert_eq!(acts(running), live("bash cargo test"));
        // A failed tool makes no card: the turn goes on (D11).
        assert!(
            acts(event(Event::Result {
                id: "c1".into(),
                outcome: agentlibre_proto::Outcome::Fail,
                content: String::new(),
            }))
            .is_empty()
        );
        assert_eq!(
            acts(event(Event::Answer("done".into()))),
            [Act::End("done".into())]
        );
        assert_eq!(acts(to(Phase::Ready, PhaseEvent::Answer)), [Act::Idle]);
        assert_eq!(acts(to(Phase::Ready, PhaseEvent::Idle)), [Act::Idle]);
        assert_eq!(acts(to(Phase::Ready, PhaseEvent::Failed)), [Act::Close]);
        assert_eq!(
            acts(to(Phase::Ready, PhaseEvent::Stop(StopReason::Interrupted))),
            [Act::Close]
        );
        assert_eq!(
            acts(to(Phase::Ready, PhaseEvent::Stop(StopReason::StepLimit))),
            [Act::Close, Act::Stopped("stopped: step_limit".into())]
        );
        assert_eq!(
            acts(event(Event::CutOff(agentlibre_proto::CutOff {
                limit: 1,
                thinking: None,
                discarded: 0,
                call: false,
            }))),
            [Act::Critical(
                "a generation was cut off at its token budget".into()
            )]
        );
        assert!(acts(Sent::Over).is_empty());
    }

    #[test]
    fn arguments_and_answers_are_cut_short() {
        assert_eq!(cut("/a/b", ARGUMENT_CHARS), "/a/b");
        assert_eq!(
            cut(&"x".repeat(100), ARGUMENT_CHARS).chars().count(),
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

    /// The requests the notifier sent.
    #[derive(Default)]
    struct Asked(Vec<Request>);

    impl Agl for Asked {
        fn send(&mut self, request: Request) {
            self.0.push(request);
        }
    }

    fn sent(sent: Sent) -> Reply {
        Reply::Sent {
            session: "s1".into(),
            sent,
        }
    }

    fn said(text: &str) -> Request {
        Request::Say {
            session: "s1".into(),
            message: text.into(),
            attached: Vec::new(),
            reasoning: None,
        }
    }

    fn info(busy: bool) -> Reply {
        Reply::Sessions {
            sessions: vec![SessionInfo {
                id: "s1".into(),
                name: Some("tea".into()),
                pids: vec![300],
                busy,
                ..Default::default()
            }],
        }
    }

    #[test]
    fn a_session_has_one_card_replaced_in_place_and_is_answered_from_it() {
        let mut desk = Fake::default();
        let mut agl = Asked::default();
        let mut notifier = Notifier::new(PathBuf::from("/sessions"), LANG, None);
        notifier.reply(info(false), &mut desk, &mut agl);
        assert!(
            desk.shown.is_empty(),
            "a session that does nothing has no card"
        );

        notifier.reply(
            sent(to(Phase::Prefill, PhaseEvent::StepStart)),
            &mut desk,
            &mut agl,
        );
        notifier.reply(
            sent(to(Phase::Thinking, PhaseEvent::FirstThinking)),
            &mut desk,
            &mut agl,
        );
        assert_eq!(desk.shown.len(), 1, "thinking once, not twice");
        let live = &desk.shown[0];
        assert_eq!(
            (live.replaces, live.summary.as_str(), live.body.as_str()),
            (0, "tea", "thinking")
        );
        assert_eq!(live.actions, [("stop".to_owned(), "Stop".to_owned())]);

        // An answer while a turn runs waits, and the card says so.
        notifier.signal(Signal::Replied(1, "and then?".into()), &mut desk, &mut agl);
        assert!(agl.0.contains(&said("and then?")));
        assert_eq!(desk.shown.last().unwrap().body, "queued: and then?");

        notifier.reply(
            sent(event(Event::Answer("It is done.".into()))),
            &mut desk,
            &mut agl,
        );
        notifier.reply(
            sent(to(Phase::Ready, PhaseEvent::Answer)),
            &mut desk,
            &mut agl,
        );
        let end = desk.shown.last().unwrap();
        assert_eq!((end.replaces, end.body.as_str()), (1, "It is done."));
        let keys: Vec<&str> = end.actions.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, [REPLY, SESSION], "no Terminal without Hyprland");

        // The plane's idle report after the turn keeps the end card.
        notifier.reply(
            sent(to(Phase::Ready, PhaseEvent::Idle)),
            &mut desk,
            &mut agl,
        );
        assert!(desk.closed.is_empty());

        // An idle session's turn tells itself.
        let shown = desk.shown.len();
        notifier.signal(Signal::Replied(1, "go on".into()), &mut desk, &mut agl);
        assert!(agl.0.contains(&said("go on")));
        assert_eq!(desk.shown.len(), shown);

        notifier.signal(Signal::Action(1, STOP.into()), &mut desk, &mut agl);
        notifier.signal(Signal::Action(99, STOP.into()), &mut desk, &mut agl);
        let stops = agl
            .0
            .iter()
            .filter(|r| matches!(r, Request::Stop { .. }))
            .count();
        assert_eq!(stops, 1, "a card of another program is not ours");

        notifier.reply(
            sent(to(Phase::Prefill, PhaseEvent::StepStart)),
            &mut desk,
            &mut agl,
        );
        notifier.reply(
            sent(to(Phase::Ready, PhaseEvent::Stop(StopReason::ServerError))),
            &mut desk,
            &mut agl,
        );
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
        let stop_card = notifier.followed["s1"].stopped;
        assert_eq!(stop_card, 2);
        let shown = desk.shown.len();
        notifier.signal(
            Signal::Action(stop_card, CONTINUE.into()),
            &mut desk,
            &mut agl,
        );
        notifier.signal(
            Signal::Replied(stop_card, "try another way".into()),
            &mut desk,
            &mut agl,
        );
        assert!(agl.0.contains(&said("Continue.")));
        assert!(agl.0.contains(&said("try another way")));
        assert_eq!(desk.shown.len(), shown, "an answer on it changes no card");

        // `/new`: the cards follow the session's new id.
        notifier.reply(
            sent(to(Phase::Prefill, PhaseEvent::StepStart)),
            &mut desk,
            &mut agl,
        );
        notifier.reply(
            Reply::Done {
                session: "s1".into(),
                done: Done {
                    turn: None,
                    why: None,
                    said: None,
                    session: SessionView {
                        id: "s2".into(),
                        ..Default::default()
                    },
                },
                yours: false,
            },
            &mut desk,
            &mut agl,
        );
        assert!(notifier.followed.contains_key("s2") && !notifier.followed.contains_key("s1"));

        // The daemon no longer holds it: its card closed.
        notifier.reply(
            Reply::Sessions {
                sessions: Vec::new(),
            },
            &mut desk,
            &mut agl,
        );
        assert!(notifier.followed.is_empty());
        assert_eq!(desk.closed, [1, 1]);
    }

    #[test]
    fn a_session_first_seen_by_its_reports_is_asked_for_and_the_daemon_gone_closes_its_cards() {
        let mut desk = Fake::default();
        let mut agl = Asked::default();
        let mut notifier = Notifier::new(PathBuf::from("/sessions"), LANG, None);
        notifier.reply(
            sent(to(Phase::Prefill, PhaseEvent::StepStart)),
            &mut desk,
            &mut agl,
        );
        assert_eq!(agl.0, [Request::Sessions]);
        assert_eq!(
            desk.shown[0].summary, "s1",
            "its id until its name is known"
        );
        notifier.reply(info(true), &mut desk, &mut agl);
        assert_eq!(notifier.followed["s1"].pid, Some(300));
        notifier.lost(&mut desk);
        assert!(notifier.followed.is_empty());
        assert_eq!(desk.closed, [1]);
    }
}
