//! The interactive session (D34, D115-D140): a line editor and a dock in
//! the terminal while the turn runs, or lines on stdin without a terminal.

mod browser;
mod dock;
mod editor;
mod inspect;
mod markdown;
mod reading;
mod splash;
mod transcript;
mod text;
pub(crate) mod term;
mod voice;

use std::collections::VecDeque;
use std::cell::RefCell;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;

use dock::Dock;
use editor::{Editor, MenuItem, Outcome, save_history};
use voice::VoiceInput;
use term::{Key, Parser, RawGuard};
use transcript::{Presenter, Screen, notice_lines, user_lines};

use crate::Setup;
use crate::agent::{Report, Sink, TurnEnd};
use crate::plane::{Command, Done, Sent, Told};
use crate::plane::Turn;
use crate::attach::Pending;
use crate::backend::ReasoningLevel;
use crate::function::catalog::Catalog;
use crate::paths::Paths;
use crate::cli::args::{MemoryAction, memory_action};
use crate::tty::is_yes;
use crate::error::Error;
use crate::i18n::{Lang, m, repl as texts};
use crate::interrupt;
use crate::media::{self, Clip, Kind, Source};
use crate::session::{CommandRun, Event, Session, StopReason, TransitionLine};
use crate::status::{
    COMMANDS, Facts, Info, Remember, display_path, help_text, parse_remember, status_text,
};
use crate::style::Role;
use crate::theme::Theme;
use crate::tr;

/// A message that waits for its turn, with the media attached to it
/// (AGL-014).
#[derive(Debug)]
struct Queued {
    text: String,
    pending: Vec<Pending>,
}

#[derive(Default)]
struct Inner {
    queue: VecDeque<Queued>,
    /// Media attached to the line being written (Ctrl-V, `/attach`); they
    /// go with the message when it is sent (AGL-014, D21).
    pending: Vec<Pending>,
    /// How many media of each kind the session holds, in the order of
    /// [`Kind::ALL`]: the numbers of new placeholders follow them.
    media_counts: [u32; 3],
    /// Commands run at the prompt, for the main thread to record in the
    /// session it holds (AGL-047, D5).
    commands: Vec<CommandRun>,
    /// A turn is running.
    running: bool,
    /// What the input thread asks of the main thread, taken in the order of
    /// [`Request::rank`] (AGL-067, F10).
    requests: Requests,
    /// A `/memory remove` waits for its `[y/N]` (AGL-060, D7).
    question: Option<Question>,
    info: Info,
    /// The model's reasoning level (AGL-013); it survives `/new`.
    reasoning: Option<ReasoningLevel>,
}

/// What the input thread and the turn thread share (D119).
struct Control {
    inner: Mutex<Inner>,
    wake: Condvar,
}

impl Control {
    fn new(info: Info, reasoning: Option<ReasoningLevel>) -> Control {
        Control {
            inner: Mutex::new(Inner {
                info,
                reasoning,
                ..Inner::default()
            }),
            wake: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Attaches `source` to the line being written; returns its
    /// placeholder (D21).
    fn attach(&self, source: Source) -> String {
        let mut inner = self.lock();
        let index = Kind::ALL.iter().position(|k| *k == source.kind).unwrap_or(0);
        let waiting = inner
            .pending
            .iter()
            .filter(|p| p.source.kind == source.kind)
            .count() as u32;
        let number = inner.media_counts[index] + waiting + 1;
        let item = Pending { number, source };
        let placeholder = item.placeholder();
        inner.pending.push(item);
        placeholder
    }

    /// Takes the counts of the media of `session` for the numbers of new
    /// placeholders.
    fn set_media_counts(&self, session: &Session) {
        let mut inner = self.lock();
        inner.media_counts = Kind::ALL.map(|kind| session.next_media_number(kind) - 1);
    }

    fn set_state(&self, state: Option<String>) {
        self.lock().info.state = state;
    }

}

impl From<&str> for Queued {
    fn from(text: &str) -> Queued {
        Queued {
            text: text.into(),
            pending: Vec::new(),
        }
    }
}

impl Inner {
    /// The texts of the queued messages.
    #[cfg(test)]
    fn texts(&self) -> Vec<&str> {
        self.queue.iter().map(|q| q.text.as_str()).collect()
    }

    /// A turn runs or messages wait for one.
    fn busy(&self) -> bool {
        self.running || !self.queue.is_empty()
    }
}

/// The Function browser of Ctrl-F (AGL-109, D1): the mode itself, given
/// the threads and the surfaces it draws on.
type OpenBrowser = Box<
    dyn Fn(&Control, &Dock, Lang, &Theme, &Catalog, &[browser::SettingsItem]) -> browser::Exit
        + Send
        + Sync,
>;

/// What the input thread asks of the main thread (AGL-067, F10).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Request {
    /// `/quit`, `/exit`, Ctrl-D, Ctrl-C on an empty prompt.
    Quit,
    /// `/new`.
    New,
    /// `/function`: the list of the Functions (AGL-036, D9).
    Functions,
    /// `/function <name>`: the switch, for the main thread to check
    /// (AGL-036, D4).
    Switch(String),
    /// `/plane`: the plane and the planes of the directory (AGL-077, D12).
    Planes,
    /// `/plane <name>`: a new session in that plane (AGL-077, D12).
    PlaneSwitch(String),
    /// `/remember name | text`, for the main thread to write (AGL-006).
    Remember(String, String, bool),
    /// `/rename <name>`, for the main thread to write (AGL-062, D7).
    Rename(String),
    /// `/memory`, its line and action, for the main thread to run
    /// (AGL-060).
    Memory(String, MemoryAction),
    /// `/compact`.
    Compact,
}

impl Request {
    /// The place in the order the main thread takes them: a quit first,
    /// then a new session, the Functions, a switch, a remember, a rename,
    /// the memory and a compaction.
    fn rank(&self) -> u8 {
        match self {
            Request::Quit => 0,
            Request::New => 1,
            Request::Functions => 2,
            Request::Switch(_) => 3,
            Request::Planes => 4,
            Request::PlaneSwitch(_) => 5,
            Request::Remember(..) => 6,
            Request::Rename(_) => 7,
            Request::Memory(..) => 8,
            Request::Compact => 9,
        }
    }
}

/// The requests that wait: one of each kind, a later one replaces the one of
/// its kind that has not been taken.
#[derive(Default)]
struct Requests(Vec<Request>);

impl Requests {
    #[cfg(test)]
    fn remember(&self) -> Option<(String, String)> {
        self.0.iter().find_map(|request| match request {
            Request::Remember(name, text, _) => Some((name.clone(), text.clone())),
            _ => None,
        })
    }

    #[cfg(test)]
    fn rename(&self) -> Option<String> {
        self.0.iter().find_map(|request| match request {
            Request::Rename(name) => Some(name.clone()),
            _ => None,
        })
    }

    #[cfg(test)]
    fn memory(&self) -> Option<(String, MemoryAction)> {
        self.0.iter().find_map(|request| match request {
            Request::Memory(line, action) => Some((line.clone(), action.clone())),
            _ => None,
        })
    }

    fn push(&mut self, request: Request) {
        self.0.retain(|waiting| waiting.rank() != request.rank());
        self.0.push(request);
    }

    /// The request of the first place in the order, if any waits.
    fn take(&mut self) -> Option<Request> {
        let first = (0..self.0.len()).min_by_key(|index| self.0[*index].rank())?;
        Some(self.0.remove(first))
    }

    fn contains(&self, request: &Request) -> bool {
        self.0.contains(request)
    }

    fn quit(&self) -> bool {
        self.contains(&Request::Quit)
    }
}

/// A removal waiting for the answer of the human (AGL-060, D7): the next line
/// is the answer, and no turn starts until it comes.
#[derive(Debug, PartialEq, Eq)]
struct Question {
    line: String,
    name: String,
}

/// What a slash command said (AGL-047, D5).
#[derive(Default)]
struct Said {
    lines: Vec<String>,
    failed: bool,
}

/// The open buffer of `/function edit` (AGL-102, D4): the file it edits,
/// the lines as they are now, and the lines as saved — unequal says
/// there are changes the file never got.
/// What the inspect mode re-reads on its open (AGL-066, D4): the session's
/// id and directory, to read the file fresh — its events and the
/// transitions the observer wrote — and, when the read fails, the copy
/// kept in memory.
struct InspectData {
    id: String,
    sessions_dir: PathBuf,
    events: Vec<Event>,
    transitions: Vec<TransitionLine>,
    outputs: PathBuf,
}

/// Reacts to what the user does at the prompt (D118, D120).
struct Input {
    dock: Arc<Dock>,
    control: Arc<Control>,
    facts: Facts,
    /// The Functions of `/function show|new` (AGL-102, D2, D3).
    catalog: Arc<Catalog>,
    /// The settings' paths for the browser's tab (AGL-109, D6).
    paths: Paths,
    workspace: PathBuf,
    theme: Theme,
    lang: Lang,
    history_file: PathBuf,
    /// Asks the running turn to stop; true when it was already asked.
    request_stop: Box<dyn Fn() -> bool + Send + Sync>,
    /// Leaves at once, after giving the terminal back (D120, D135).
    exit_now: Box<dyn Fn() + Send + Sync>,
    /// Opens the inspect mode on the alternate screen (AGL-066, D4); it
    /// runs here, in the input thread, and gives back what leaving means
    /// (AGL-098, D7; AGL-111, D1): the text to send, else nothing.
    open_inspect: Box<dyn Fn() -> inspect::Exit + Send + Sync>,
    /// Opens the Function browser on the alternate screen (AGL-109, D1);
    /// `Some` only with a terminal of its own, as `/function` itself.
    open_browser: Option<OpenBrowser>,
    /// What Ctrl-V reads (AGL-014, D21).
    clipboard: Box<dyn Fn() -> Clip + Send + Sync>,
    /// Ctrl-R records the voice (AGL-016).
    voice: Arc<VoiceInput>,
    /// What the running slash command says, while one runs (AGL-047, D5).
    said: Mutex<Option<Said>>,
}

impl Input {
    fn notice(&self, text: &str, role: Role) {
        if let Some(said) = self.said.lock().unwrap().as_mut() {
            said.lines.push(text.to_owned());
            said.failed |= matches!(role, Role::StatusFailure | Role::StatusPending);
        }
        let columns = self.dock.columns();
        self.dock
            .print(&notice_lines(&self.theme, text, columns, role));
    }

    /// Enter.
    fn submitted(&self, message: String) {
        let question = self.control.lock().question.take();
        if let Some(question) = question {
            if is_yes(&message) {
                let action = MemoryAction::Remove {
                    name: question.name,
                    yes: true,
                };
                self.control.lock().requests.push(Request::Memory(question.line, action));
                self.control.wake.notify_all();
            } else {
                self.declined(question);
            }
            return;
        }
        let _ = save_history(&self.history_file, &self.dock.history());
        let trimmed = message.trim();
        if trimmed.is_empty() {
            return;
        }
        if !trimmed.starts_with('/') {
            let mut inner = self.control.lock();
            let pending = std::mem::take(&mut inner.pending);
            inner.queue.push_back(Queued {
                text: message,
                pending,
            });
            let queued = inner.queue.len();
            drop(inner);
            self.dock.set_queued(queued);
            self.control.wake.notify_all();
            return;
        }
        *self.said.lock().unwrap() = Some(Said::default());
        // AGL-101: Enter echoes the command like a message, in the tool's
        // color as the menu paints it; the model never sees the line.
        let mut echo = user_lines(&self.theme, trimmed, self.dock.columns(), Role::Tool);
        echo.push(String::new());
        self.dock.print(&echo);
        let handed = self.command(trimmed);
        let said = self.said.lock().unwrap().take().unwrap_or_default();
        // AGL-102: a command of this thread is recorded whatever it said —
        // silence is a result too. What went to the main thread is recorded
        // there, with its result (AGL-047, D5).
        if !handed {
            self.control.lock().commands.push(CommandRun {
                line: trimmed.to_owned(),
                failed: said.failed,
                output: Some(said.lines.join("\n")).filter(|output| !output.is_empty()),
            });
        }
    }

    /// The human did not agree to a removal: nothing happened, and the session
    /// says so (AGL-060, D6).
    fn declined(&self, question: Question) {
        let text = tr!(self.lang, m::MEMORY_NOT_REMOVED, name = question.name);
        self.notice(&text, Role::Muted);
        self.control.lock().commands.push(CommandRun {
            line: question.line,
            failed: false,
            output: Some(text),
        });
        self.control.wake.notify_all();
    }

    /// A slash command; what it says is what [`Input::notice`] gathers.
    /// True when the main thread took the command: it records there, with
    /// its result (AGL-047, D5; AGL-102).
    fn command(&self, trimmed: &str) -> bool {
        let name = trimmed.split_whitespace().next().unwrap_or(trimmed);
        match name {
            "/help" => {
                self.notice(&help_text(self.lang, true), Role::Muted);
                false
            }
            "/status" => {
                let (info, reasoning) = {
                    let inner = self.control.lock();
                    (inner.info.clone(), inner.reasoning)
                };
                self.notice(
                    &status_text(&self.facts, &info, reasoning, self.lang),
                    Role::Muted,
                );
                false
            }
            "/think" => {
                let value = trimmed.split_whitespace().nth(1);
                match value {
                    None => {
                        let level = self.control.lock().reasoning;
                        let shown = texts::reasoning_level(level, self.lang);
                        self.notice(&tr!(self.lang, m::THINK_LEVEL, level = shown), Role::Muted);
                    }
                    Some(value) => match ReasoningLevel::parse(value) {
                        Some(level) => {
                            self.control.lock().reasoning = Some(level);
                            let shown = texts::reasoning_level(Some(level), self.lang);
                            self.notice(
                                &tr!(self.lang, m::THINK_LEVEL, level = shown),
                                Role::Muted,
                            );
                        }
                        None => self.notice(
                            &tr!(self.lang, m::REASONING_BAD, value = value),
                            Role::StatusFailure,
                        ),
                    },
                }
                false
            }
            "/attach" => {
                let path = trimmed["/attach".len()..].trim();
                let path = path
                    .strip_prefix('"')
                    .and_then(|p| p.strip_suffix('"'))
                    .or_else(|| path.strip_prefix('\'').and_then(|p| p.strip_suffix('\'')))
                    .unwrap_or(path);
                if path.is_empty() {
                    self.notice(tr!(self.lang, m::ATTACH_USAGE), Role::StatusPending);
                } else {
                    self.attach_files(&[std::path::PathBuf::from(path)]);
                }
                false
            }
            "/remember" => match parse_remember(trimmed["/remember".len()..].trim()) {
                Remember::Usage => {
                    self.notice(tr!(self.lang, m::REMEMBER_USAGE), Role::StatusPending);
                    false
                }
                // The entry is written by the main thread, which holds the
                // session: not while a turn runs (as `/new`).
                Remember::Entry {
                    name,
                    text,
                    workspace,
                } => {
                    let mut inner = self.control.lock();
                    if inner.busy() {
                        drop(inner);
                        self.notice(
                            &tr!(self.lang, m::BUSY, name = "/remember"),
                            Role::StatusPending,
                        );
                        false
                    } else {
                        inner.requests.push(Request::Remember(name, text, workspace));
                        drop(inner);
                        self.control.wake.notify_all();
                        true
                    }
                }
                // An ordinary message: the model names the entry (M37).
                Remember::Turn(message) => {
                    self.submitted(message);
                    false
                }
            },
            // `/rename <name>` (AGL-062, D7): the main thread writes the name,
            // which it holds the session for; not while a turn runs (as `/new`).
            "/rename" => {
                let text = trimmed["/rename".len()..].trim();
                if text.is_empty() {
                    self.notice(tr!(self.lang, m::RENAME_USAGE), Role::StatusPending);
                    return false;
                }
                let mut inner = self.control.lock();
                if inner.busy() {
                    drop(inner);
                    self.notice(&tr!(self.lang, m::BUSY, name = name), Role::StatusPending);
                    return false;
                }
                inner.requests.push(Request::Rename(text.to_owned()));
                drop(inner);
                self.control.wake.notify_all();
                true
            }
            // `/memory` (AGL-060, D5): the main thread finds the store, which
            // takes the evaluation of its marker, too long for the keyboard to
            // wait; not while a turn runs, when the model can be writing it.
            "/memory" => {
                let words = trimmed.split_whitespace().skip(1).map(str::to_owned);
                let Ok(action) = memory_action(words) else {
                    self.notice(tr!(self.lang, m::MEMORY_USAGE), Role::StatusPending);
                    return false;
                };
                let mut inner = self.control.lock();
                if inner.busy() {
                    drop(inner);
                    self.notice(&tr!(self.lang, m::BUSY, name = name), Role::StatusPending);
                    return false;
                }
                inner.requests.push(Request::Memory(trimmed.to_owned(), action));
                drop(inner);
                self.control.wake.notify_all();
                true
            }
            // `/plane [<name>]` (AGL-077, D12): the main thread says the
            // planes, or checks the plane and leaves the session for a new
            // one in it — not while a turn runs (as `/function`).
            "/plane" => {
                let plane = trimmed.split_whitespace().nth(1).map(str::to_owned);
                let mut inner = self.control.lock();
                if inner.busy() {
                    drop(inner);
                    self.notice(&tr!(self.lang, m::BUSY, name = name), Role::StatusPending);
                    return false;
                }
                match plane {
                    Some(plane) => inner.requests.push(Request::PlaneSwitch(plane)),
                    None => inner.requests.push(Request::Planes),
                }
                drop(inner);
                self.control.wake.notify_all();
                true
            }
            // `/function [<name>]` (AGL-036, D3, D4, D9): the main thread
            // shows the list, or checks the Function and leaves the session
            // for a new one with it — not while a turn runs (as `/new`).
            // AGL-102, D2-D3: `show` and `new` are this thread's own, and a
            // name that exists stays the main thread's switch. AGL-109, D2:
            // the line editor is gone; `edit` points at Ctrl-F and $EDITOR.
            "/function" => {
                let rest = trimmed["/function".len()..].trim();
                let word = rest.split_whitespace().next().unwrap_or("");
                if matches!(word, "show" | "new" | "edit")
                    && !self.catalog.entries(self.lang).iter().any(|e| e.name == word)
                {
                    // The rest keeps its spaces: `/function new`'s words.
                    let tail = &rest[word.len()..];
                    match word {
                        "show" => self.function_show(tail.trim()),
                        "new" => self.function_new(tail),
                        _ => self.notice(tr!(self.lang, m::FUNCTION_EDIT_GONE), Role::StatusPending),
                    }
                    return false;
                }
                let function = rest.split_whitespace().next().map(str::to_owned);
                let mut inner = self.control.lock();
                if inner.busy() {
                    drop(inner);
                    self.notice(&tr!(self.lang, m::BUSY, name = name), Role::StatusPending);
                    return false;
                }
                match function {
                    Some(function) => inner.requests.push(Request::Switch(function)),
                    None => inner.requests.push(Request::Functions),
                }
                drop(inner);
                self.control.wake.notify_all();
                true
            }
            "/quit" | "/exit" | "/new" | "/compact" => {
                let mut inner = self.control.lock();
                if inner.busy() {
                    drop(inner);
                    self.notice(&tr!(self.lang, m::BUSY, name = name), Role::StatusPending);
                    return false;
                }
                if name == "/new" {
                    inner.requests.push(Request::New);
                    inner.pending.clear();
                } else if name == "/compact" {
                    inner.requests.push(Request::Compact);
                } else {
                    inner.requests.push(Request::Quit);
                }
                drop(inner);
                self.control.wake.notify_all();
                true
            }
            _ => {
                self.notice(
                    &tr!(self.lang, m::UNKNOWN_COMMAND, name = name),
                    Role::StatusPending,
                );
                false
            }
        }
    }

    /// `/function show <name>` (AGL-102, D2): the source of the Function
    /// whose name is given, and the file a run loads it from.
    fn function_show(&self, name: &str) {
        if name.is_empty() {
            self.notice(tr!(self.lang, m::FUNCTION_SHOW_USAGE), Role::StatusPending);
            return;
        }
        let shown = self
            .catalog
            .text(name)
            .and_then(|text| Ok((text, self.catalog.file(name)?.path)));
        match shown {
            Ok((text, path)) => {
                self.notice(&text, Role::Muted);
                self.notice(&path.display().to_string(), Role::Muted);
            }
            Err(error) => self.notice(&error.message(self.lang), Role::StatusFailure),
        }
    }

    /// `/function new <name> [from]` (AGL-102, D3): a copy of an existing
    /// Function — the current one by default — to the workspace's
    /// `functions/`; a name that exists is refused.
    fn function_new(&self, rest: &str) {
        let mut words = rest.split_whitespace();
        let (Some(name), from) = (words.next(), words.next()) else {
            self.notice(tr!(self.lang, m::FUNCTION_NEW_USAGE), Role::StatusPending);
            return;
        };
        let source = from.unwrap_or(&self.catalog.current);
        let made = self
            .catalog
            .text(source)
            .and_then(|text| self.catalog.create(name, &text));
        match made {
            Ok(path) => self.notice(
                &tr!(
                    self.lang,
                    m::FUNCTION_NEW,
                    name = name,
                    source = source,
                    path = path.display()
                ),
                Role::Muted,
            ),
            Err(error) => self.notice(&error.message(self.lang), Role::StatusFailure),
        }
    }

    /// Attaches the files and puts their placeholders into the line at
    /// the cursor (AGL-014, D21); a file that cannot be read is told.
    fn attach_files(&self, paths: &[std::path::PathBuf]) {
        let mut placeholders = Vec::new();
        for path in paths {
            match Source::from_file(path) {
                Ok(source) => {
                    let name = source.name.clone();
                    let placeholder = self.control.attach(source);
                    self.notice(
                        &tr!(self.lang, m::ATTACHED, placeholder = placeholder, name = name),
                        Role::Muted,
                    );
                    placeholders.push(placeholder);
                }
                Err(error) => self.notice(&error.message(self.lang), Role::StatusFailure),
            }
        }
        if !placeholders.is_empty() {
            self.dock
                .key(Key::Paste(format!("{} ", placeholders.join(" "))));
        }
    }

    /// Ctrl-V: files copied in a file manager, a picture, or text from
    /// the clipboard (AGL-014, D21).
    fn paste(&self) {
        match (self.clipboard)() {
            Clip::Files(paths) => self.attach_files(&paths),
            Clip::Image(bytes) => {
                let source = Source {
                    kind: Kind::Image,
                    bytes,
                    ext: "png".into(),
                    name: "clipboard.png".into(),
                };
                let placeholder = self.control.attach(source);
                self.dock.key(Key::Paste(format!("{placeholder} ")));
            }
            Clip::Text(text) => {
                self.dock.key(Key::Paste(text));
            }
            Clip::Empty => self.notice(tr!(self.lang, m::CLIPBOARD_EMPTY), Role::StatusPending),
        }
    }

    /// Ctrl-C (D120).
    fn interrupt(&self) {
        // At a question it is the answer no (AGL-060, D7).
        let question = self.control.lock().question.take();
        if let Some(question) = question {
            self.declined(question);
            self.dock.clear_input();
            return;
        }
        let mut inner = self.control.lock();
        if inner.busy() {
            let already = (self.request_stop)();
            if already {
                drop(inner);
                (self.exit_now)();
                return;
            }
            inner.queue.clear();
            drop(inner);
            self.dock.set_queued(0);
        } else if self.dock.input_is_empty() {
            inner.requests.push(Request::Quit);
            drop(inner);
            self.control.wake.notify_all();
        } else {
            drop(inner);
            self.dock.clear_input();
        }
    }

    /// Ctrl-D on an empty prompt, or the end of input.
    fn end_of_input(&self) {
        self.control.lock().requests.push(Request::Quit);
        self.control.wake.notify_all();
    }

    fn key(&self, key: Key) {
        if self.voice.recording() && matches!(key, Key::Escape | Key::Ctrl('c')) {
            // Not a stop of the turn: only the recording is cancelled (D2).
            self.voice.cancel();
            return;
        }
        if key == Key::Ctrl('r') {
            self.voice.toggle();
            return;
        }
        if key == Key::Ctrl('v') {
            self.paste();
            return;
        }
        if key == Key::Ctrl('o') {
            // The inspect mode takes the alternate screen and returns the
            // terminal as it was (AGL-066, D4, D8), printing nothing on its
            // way out (AGL-111, D1); `Enter` inside the mode sends the
            // input's text (AGL-098, D7).
            if let Some(message) = (self.open_inspect)() {
                self.submitted(message);
            }
            return;
        }
        if key == Key::Ctrl('f') {
            // The Function browser (AGL-109, D1): `s` leaves the session
            // for a Function, as `/function <name>` does.
            // The settings' tab: the user's file and this workspace's
            // (AGL-109, D6).
            let items = vec![
                browser::SettingsItem {
                    label: "settings.nix".into(),
                    path: self.paths.settings_file(),
                    note: tr!(self.lang, m::BROWSER_SETTINGS_USER).to_string(),
                },
                browser::SettingsItem {
                    label: "settings.nix".into(),
                    path: self.workspace.join(crate::settings::SETTINGS_FILE),
                    note: tr!(self.lang, m::BROWSER_SETTINGS_PROJECT).to_string(),
                },
            ];
            if let Some(open) = &self.open_browser
                && let browser::Exit::Switch(name) = open(
                    &self.control,
                    &self.dock,
                    self.lang,
                    &self.theme,
                    &self.catalog,
                    &items,
                )
            {
                let mut inner = self.control.lock();
                inner.requests.push(Request::Switch(name));
                drop(inner);
                self.control.wake.notify_all();
            }
            return;
        }
        match self.dock.key(key) {
            Outcome::Edited => {}
            Outcome::Submit(message) => self.submitted(message),
            Outcome::Interrupt => self.interrupt(),
            Outcome::Quit => {
                if self.control.lock().busy() {
                    self.notice(
                        &tr!(self.lang, m::BUSY, name = "Ctrl-D"),
                        Role::StatusPending,
                    );
                } else {
                    self.end_of_input();
                }
            }
        }
    }
}

/// Reads the keyboard until `done` (D115).
fn input_loop(input: Input, done: Arc<AtomicBool>) {
    let mut parser = Parser::default();
    let mut buffer = [0u8; 4096];
    while !done.load(Ordering::SeqCst) {
        if term::take_resized() {
            input.dock.resize(term::size());
        }
        let keys = match term::read_input(&mut buffer, 100) {
            Ok(None) => {
                if parser.waiting() {
                    parser.timeout()
                } else {
                    continue;
                }
            }
            Ok(Some(0)) | Err(_) => {
                input.end_of_input();
                return;
            }
            Ok(Some(n)) => parser.feed(&buffer[..n]),
        };
        for key in keys {
            input.key(key);
        }
    }
}

/// What the REPL does with a report of a turn: `/status` shows the state,
/// the presenter draws the rest.
fn show(presenter: &mut Presenter, control: &Control, report: Report) {
    if let Report::Event(Event::State(state)) = &report {
        control.set_state(Some(state.clone()));
    }
    presenter.send(report);
}

/// A turn of the REPL: its reports are drawn, what the start says is shown
/// as a notice, the dock and `/status` follow the server and the media.
struct ReplTurn<'a> {
    presenter: &'a mut Presenter,
    control: &'a Control,
    dock: &'a Dock,
}

impl Sink for ReplTurn<'_> {
    fn send(&mut self, report: Report) {
        show(self.presenter, self.control, report);
    }
}

impl Turn for ReplTurn<'_> {
    fn line(&mut self, line: &str) {
        self.presenter.show_notice(line, Role::Muted);
    }

    fn context(&mut self, context: usize) {
        self.dock.set_context_size(context);
        self.control.lock().info.context = Some(context);
    }

    fn stored(&mut self, media: [u32; 3]) {
        self.control.lock().media_counts = media;
    }

    fn over(&mut self) {
        self.presenter.close();
    }
}

/// What the main thread waits for.
enum Next {
    Message(Queued),
    New,
    Compact,
    Remember(String, String, bool),
    /// `/rename`: the name asked for (AGL-062).
    Rename(String),
    /// `/memory`: its line and action (AGL-060).
    Memory(String, MemoryAction),
    /// `/function` (AGL-036, D9).
    Functions,
    /// `/function <name>` (AGL-036, D4).
    Switch(String),
    /// `/plane` (AGL-077, D12).
    Planes,
    /// `/plane <name>` (AGL-077, D12).
    PlaneSwitch(String),
    Quit,
}

/// How the interactive session ends (AGL-036, D4): the human left, or a
/// `/function <name>` asks for a new session with that Function at the
/// reasoning level of this one.
pub enum End {
    Exit(ExitCode),
    /// `/plane <name>`: a new session in that plane (AGL-077, D12).
    Plane {
        name: String,
        reasoning: Option<ReasoningLevel>,
    },
    /// `/function <name>`: a new session with that Function, in the same
    /// plane (AGL-077, D6).
    Function {
        name: String,
        reasoning: Option<ReasoningLevel>,
        plane: Option<String>,
    },
}

/// A slash command goes to the session (AGL-047, D5). A session that cannot
/// be written to says so at the next event of the turn.
/// The line of a command run at the prompt, for the session's record.
fn command(line: String, failed: bool, output: Option<String>) -> Event {
    Event::Command(CommandRun {
        line,
        failed,
        output,
    })
}

/// A command sent to the plane and its end; `None` when the plane's thread is
/// gone.
type Ask<'a> = dyn Fn(Command, &mut dyn FnMut(Sent)) -> Option<Done> + 'a;

/// A command of the run that says what it did: shown, and its line
/// recorded; false when the plane's thread is gone.
fn said(
    ask: &Ask,
    record: &dyn Fn(String, bool, Option<String>) -> bool,
    presenter: &Presenter,
    run: Command,
    line: String,
    lang: Lang,
) -> bool {
    match ask(run, &mut drop).and_then(|done| done.said) {
        Some(Ok(said)) => {
            let (failed, output) = (said.failed, said.content.clone());
            show_said(presenter, &said, lang);
            record(line, failed, output)
        }
        Some(Err(error)) => {
            let text = error.message(lang);
            presenter.show_notice(&text, Role::StatusFailure);
            record(line, true, Some(text))
        }
        None => false,
    }
}

/// What a command of the run said (AGL-067, F2): an answer and a notice
/// muted, a refusal and an error's text as a failure, a command cut short
/// pending, and the memory line of an entry the command wrote.
fn show_said(presenter: &Presenter, said: &crate::plane::Said, lang: Lang) {
    for told in &said.lines {
        let (text, role) = match told {
            Told::Output(text) | Told::Notice(text) => (text, Role::Muted),
            Told::Refusal(text) | Told::Fault(text) => (text, Role::StatusFailure),
            Told::Pending(text) => (text, Role::StatusPending),
        };
        presenter.show_notice(text, role);
    }
    if let Some(write) = &said.memory {
        presenter.show_memory(&write.name, write.kind, write.text.as_deref());
    }
    if let Some(error) = &said.record_error {
        let text = error.message(lang);
        presenter.show_notice(&text, Role::StatusFailure);
    }
}

/// The commands run at the prompt go to the session between turns, so that
/// none stands between a call and its result (AGL-047, D5).
fn take_commands(control: &Control) -> Vec<Event> {
    std::mem::take(&mut control.lock().commands)
        .into_iter()
        .map(Event::Command)
        .collect()
}

fn wait_for_next(control: &Control) -> Next {
    let mut inner = control.lock();
    loop {
        if inner.requests.quit() {
            return Next::Quit;
        }
        if inner.question.is_some() {
            inner = control.wake.wait(inner).unwrap_or_else(|e| e.into_inner());
            continue;
        }
        if let Some(request) = inner.requests.take() {
            return match request {
                Request::Quit => Next::Quit,
                Request::New => Next::New,
                Request::Functions => Next::Functions,
                Request::Switch(name) => Next::Switch(name),
                Request::Planes => Next::Planes,
                Request::PlaneSwitch(name) => Next::PlaneSwitch(name),
                Request::Remember(name, text, workspace) => Next::Remember(name, text, workspace),
                Request::Rename(name) => Next::Rename(name),
                Request::Memory(line, action) => Next::Memory(line, action),
                Request::Compact => {
                    // Ctrl-C stops the compaction as it stops a turn.
                    inner.running = true;
                    interrupt::reset();
                    Next::Compact
                }
            };
        }
        if let Some(message) = inner.queue.pop_front() {
            inner.running = true;
            // A Ctrl-C from before this turn is forgotten (D131).
            interrupt::reset();
            return Next::Message(message);
        }
        inner = control.wake.wait(inner).unwrap_or_else(|e| e.into_inner());
    }
}

/// The interactive session in a terminal (D115-D138).
pub fn run(setup: Setup) -> Result<End, Error> {
    let Setup {
        remote,
        opened,
        session: file,
        lang,
        theme,
        repl,
        history_file,
        catalog,
        paths,
        voice: dictation,
    } = setup;
    let resumed = opened.resumed;
    let sessions_dir = paths.sessions();
    let context = opened.context;
    let reasoning = opened.reasoning.as_deref().and_then(crate::backend::ReasoningLevel::parse);
    let workspace = PathBuf::from(&opened.workspace);
    let facts = Facts {
        function: opened.function.clone(),
        workspace: display_path(&workspace),
        plane: opened.session.plane.clone(),
        server: opened.server_url.clone(),
    };
    let commands: Vec<MenuItem> = COMMANDS
        .iter()
        .map(|name| MenuItem {
            name,
            description: texts::command(name, lang).into(),
        })
        .collect();
    let editor = Editor::new(editor::load_history(&history_file), commands);
    let _raw = RawGuard::new().map_err(|source| Error::Terminal { source })?;
    let dock = Arc::new(
        Dock::new(
            Box::new(io::stdout()),
            theme.clone(),
            lang,
            facts.function.clone(),
            facts.workspace.clone(),
            editor,
            term::size(),
        )
        .with_context(context.unwrap_or(0)),
    );
    let control = Arc::new(Control::new(
        Info {
            session: file.id.clone(),
            context,
            ..Info::default()
        },
        reasoning,
    ));
    control.set_media_counts(&file);
    let voice = Arc::new(VoiceInput::new(
        dock.clone(),
        dictation,
        Box::new(crate::record::start),
        repl.max_record_seconds,
        lang,
        theme.clone(),
    ));
    let screen: Arc<dyn Screen> = dock.clone();
    let mut presenter = Presenter::new(screen, theme.clone(), lang, repl);

    interrupt::install_request_only();
    // Ctrl-C, its flag set, is sent to the daemon as `stop` (AGL-076, D5).
    crate::remote::forward_stop(&remote);
    // A SIGINT from outside comes to this thread only (D131); the stop
    // wakes the thread of the turn itself (AGL-075).
    term::block_sigint();
    let done = Arc::new(AtomicBool::new(false));
    // What the inspect mode re-reads on its open (AGL-066, D4).
    let inspect_data = Arc::new(Mutex::new(InspectData {
        id: file.id.clone(),
        sessions_dir: sessions_dir.clone(),
        events: file.events.clone(),
        transitions: Vec::new(),
        outputs: file.outputs_dir(),
    }));
    let open_inspect = {
        let data = inspect_data.clone();
        let dock = dock.clone();
        let theme = theme.clone();
        move || {
            // The file holds what the observer wrote of the phases; the
            // in-memory copy is the fallback for when it cannot be read.
            let (events, transitions, outputs) = {
                let inner = data.lock().unwrap();
                match Session::open(&inner.sessions_dir, &inner.id) {
                    Ok(session) => {
                        let outputs = session.outputs_dir();
                        (session.events, session.transitions, outputs)
                    }
                    Err(_) => (
                        inner.events.clone(),
                        inner.transitions.clone(),
                        inner.outputs.clone(),
                    ),
                }
            };
            // The dock's rows of the moment (D14): the input prompt
            // stays on the alternate screen, and its rows come anew at
            // each redraw (AGL-098, D7).
            inspect::run(
                &|text| dock.write_raw(text),
                &theme,
                lang,
                events,
                transitions,
                outputs,
                &dock,
            )
        }
    };
    let input = Input {
        dock: dock.clone(),
        control: control.clone(),
        facts: facts.clone(),
        catalog: Arc::clone(&catalog),
        paths: paths.clone(),
        workspace: workspace.clone(),
        theme: theme.clone(),
        lang,
        history_file,
        request_stop: Box::new(interrupt::request),
        exit_now: Box::new(|| {
            term::restore();
            unsafe { libc::_exit(i32::from(interrupt::EXIT_CODE)) }
        }),
        clipboard: Box::new(media::clipboard),
        voice: voice.clone(),
        open_inspect: Box::new(open_inspect),
        open_browser: Some(Box::new(|control, dock, lang, theme, catalog, items| {
            browser::run(
                &|text| dock.write_raw(text),
                theme,
                lang,
                catalog,
                items,
                dock,
                &|| control.lock().busy(),
                &|message| dock.print(&[message.to_string()]),
            )
        })),
        said: Mutex::new(None),
    };
    let reader = {
        let done = done.clone();
        thread::spawn(move || input_loop(input, done))
    };
    // The dock's clock while a prompt is read (AGL-046, D19).
    let ticker = {
        let done = done.clone();
        let dock = dock.clone();
        thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                thread::sleep(std::time::Duration::from_millis(150));
                dock.tick();
            }
        })
    };
    term::unblock_sigint();

    dock.start();
    // The splash of the REPL's start (AGL-065, D2 to D7): the art, and
    // under it, muted, the version, the tagline and the address.
    if repl.splash {
        let rev = std::env::var(crate::about::REV_VAR).ok();
        dock.print(&splash::lines(
            &theme,
            dock.columns(),
            lang,
            &crate::about::version(rev.as_deref()),
        ));
    }
    presenter.show_notice(&tr!(lang, m::SESSION, id = file.id), Role::Muted);
    if resumed {
        presenter.replay(&file.events, &file.transitions);
        if let Some(state) = file.unfinished().and_then(|turn| turn.state) {
            control.lock().info.unfinished = Some(state.clone());
            presenter.show_notice(&tr!(lang, m::UNFINISHED, state = state), Role::Muted);
        }
    }

    // The server comes up as the session starts (AGL-046), in the daemon,
    // which was asked so at the open. It runs like a turn: what the human
    // types meanwhile waits in the queue. A server that fails is told here
    // and tried again at the first message.
    if repl.start_server {
        control.lock().running = true;
        interrupt::reset();
        let done = remote.wait(&mut |sent| match sent {
            Sent::Line(line) => presenter.show_notice(&line, Role::Muted),
            Sent::Context(size) => {
                dock.set_context_size(size);
                control.lock().info.context = Some(size);
            }
            Sent::Report(report) => show(&mut presenter, &control, report),
            _ => {}
        });
        if let Some(Some(Err(error))) = done.map(|done| done.said) {
            presenter.show_notice(&error.message(lang), Role::StatusFailure);
        }
        control.lock().running = false;
        presenter.set_ready();
    }

    // What the REPL keeps beside the plane: what its session is after each
    // command.
    let plane_name = opened.session.plane.clone();
    let session = RefCell::new(agentlibre_core::plane::wire::view_of(opened.session.clone()));
    // The plane takes the commands in the daemon (AGL-076, D5).
    // A command and its end; `None` when the daemon is gone.
    let ask = |command: Command, each: &mut dyn FnMut(Sent)| -> Option<Done> {
        let done = remote.ask(command, each)?;
        *session.borrow_mut() = done.session.clone();
        Some(done)
    };
    let mut gone = false;
    let mut end = End::Exit(ExitCode::SUCCESS);
    loop {
        let next = wait_for_next(&control);
        let commands = take_commands(&control);
        if !commands.is_empty() && ask(Command::Append(commands), &mut drop).is_none() {
            gone = true;
            break;
        }
        // The inspect mode re-reads the file, this is the fallback and
        // the new id of a `/new` (AGL-066).
        {
            let mut inner = inspect_data.lock().unwrap();
            inner.id = session.borrow().id.clone();
            inner.events = session.borrow().events.clone();
            inner.outputs = session.borrow().outputs.clone();
        }
        // The command's line in the session's record (AGL-047, D5).
        let record = |line: String, failed: bool, output: Option<String>| {
            ask(Command::Append(vec![command(line, failed, output)]), &mut drop).is_some()
        };
        let kept = match next {
            Next::Quit => {
                record("/quit".into(), false, None);
                break;
            }
            Next::Functions => {
                let list = catalog.lines(lang).join("\n");
                presenter.show_notice(&list, Role::Muted);
                record("/function".into(), false, Some(list))
            }
            Next::Planes => {
                let here = crate::plane::PlaneFile::of_directory(&paths, &workspace)
                    .map(|planes| planes.into_iter().map(|plane| plane.name).collect::<Vec<_>>());
                match here {
                    Ok(names) => {
                        let none = "—".to_owned();
                        let text = tr!(
                            lang,
                            m::PLANE_HERE,
                            plane = plane_name.as_ref().unwrap_or(&none),
                            planes = if names.is_empty() { none.clone() } else { names.join(", ") }
                        );
                        presenter.show_notice(&text, Role::Muted);
                        record("/plane".into(), false, Some(text))
                    }
                    Err(error) => {
                        let text = error.message(lang);
                        presenter.show_notice(&text, Role::StatusFailure);
                        record("/plane".into(), true, Some(text))
                    }
                }
            }
            // The plane must be there before the session is left (AGL-077,
            // D12): a refusal keeps this session as it is.
            Next::PlaneSwitch(name) => match crate::plane::PlaneFile::open(&paths, &name) {
                Ok(_) => {
                    presenter.show_notice(&tr!(lang, m::PLANE_SWITCH, name = name), Role::Muted);
                    record(format!("/plane {name}"), false, None);
                    end = End::Plane {
                        name,
                        reasoning: control.lock().reasoning,
                    };
                    break;
                }
                Err(error) => {
                    let text = error.message(lang);
                    presenter.show_notice(&text, Role::StatusFailure);
                    record(format!("/plane {name}"), true, Some(text))
                }
            },
            // The Function must load and fit before the session is left
            // (AGL-036, D4): a refusal keeps this session as it is.
            Next::Switch(name) => match catalog.check(&name) {
                Ok(()) => {
                    presenter.show_notice(&tr!(lang, m::FUNCTION_SWITCH, name = name), Role::Muted);
                    record(format!("/function {name}"), false, None);
                    end = End::Function {
                        name,
                        reasoning: control.lock().reasoning,
                        plane: plane_name.clone(),
                    };
                    break;
                }
                Err(error) => {
                    let text = error.message(lang);
                    presenter.show_notice(&text, Role::StatusFailure);
                    record(format!("/function {name}"), true, Some(text))
                }
            },
            Next::New => {
                if !record("/new".into(), false, None) {
                    gone = true;
                    break;
                }
                match ask(Command::New, &mut drop).and_then(|done| done.said) {
                    Some(Ok(said)) => {
                        show_said(&presenter, &said, lang);
                        let mut inner = control.lock();
                        inner.info = Info {
                            session: session.borrow().id.clone(),
                            context: inner.info.context,
                            ..Info::default()
                        };
                        inner.media_counts = session.borrow().media;
                        true
                    }
                    Some(Err(error)) => {
                        let text = error.message(lang);
                        presenter.show_notice(&text, Role::StatusFailure);
                        true
                    }
                    None => false,
                }
            }
            Next::Rename(name) => {
                // `/rename <name>` (AGL-062, D7): the name is a line of the
                // file. The model has the command from its record (AGL-047,
                // D5), as with the others.
                said(&ask, &record, &presenter, Command::Rename(name.clone()), format!("/rename {name}"), lang)
            }
            Next::Remember(name, text, workspace) => {
                // `/remember name | text` (AGL-006, M34, M37): the program
                // writes the entry, no turn. The model has it from the
                // record of the command (AGL-047, D5).
                let flag = if workspace { "--workspace " } else { "" };
                let line = format!("/remember {flag}{name} | {text}");
                said(&ask, &record, &presenter, Command::Remember { name, text, workspace }, line, lang)
            }
            // Without `--yes`, the question: the removal comes back with the
            // answer, and the record with it (AGL-060, D7).
            Next::Memory(line, MemoryAction::Remove { name, yes: false }) => {
                // The entry must be there before the question (AGL-060, D7).
                let found = match ask(Command::LookAt(MemoryAction::Show { name: name.clone() }), &mut drop) {
                    Some(done) => match done.said {
                        Some(Ok(said)) if !said.failed => Ok(()),
                        Some(Ok(said)) => Err(said.lines.iter().find_map(|told| match told {
                            Told::Fault(text) | Told::Refusal(text) => Some(text.clone()),
                            _ => None,
                        }).unwrap_or_default()),
                        Some(Err(error)) => Err(error.message(lang)),
                        None => Ok(()),
                    },
                    None => Err(Error::PlaneGone.message(lang)),
                };
                match found {
                    Ok(()) => {
                        let question = tr!(lang, m::MEMORY_REMOVE_ASK, name = name);
                        control.lock().question = Some(Question { line, name });
                        presenter.show_notice(&question, Role::StatusPending);
                        true
                    }
                    Err(text) => {
                        presenter.show_notice(&text, Role::StatusFailure);
                        record(line, true, Some(text))
                    }
                }
            }
            // The model learns of it from the record, as of `/remember`
            // (AGL-060, D6).
            Next::Memory(line, MemoryAction::Remove { name, yes: true }) => {
                said(&ask, &record, &presenter, Command::Remove { name }, line, lang)
            }
            Next::Memory(line, action) => {
                said(&ask, &record, &presenter, Command::LookAt(action), line, lang)
            }
            Next::Compact => {
                let reasoning = control.lock().reasoning;
                let done = ask(Command::Compact { reasoning }, &mut |sent| {
                    if let Sent::Report(report) = sent {
                        show(&mut presenter, &control, report);
                    }
                });
                control.lock().running = false;
                let kept = match done.and_then(|done| done.said) {
                    Some(Ok(said)) => {
                        let (failed, output) = (said.failed, said.content.clone());
                        show_said(&presenter, &said, lang);
                        record("/compact".into(), failed, output)
                    }
                    Some(Err(error)) => {
                        presenter.show_notice(&error.message(lang), Role::StatusFailure);
                        true
                    }
                    None => false,
                };
                presenter.set_ready();
                kept
            }
            Next::Message(Queued {
                text: message,
                pending,
            }) => {
                dock.set_queued(control.lock().queue.len());
                presenter.begin_turn();
                presenter.show_user(&message);
                // The running turn keeps the level it started with (AGL-013).
                let reasoning = control.lock().reasoning;
                let done = ask(
                    Command::Say {
                        message,
                        attached: pending,
                        reasoning,
                        observer: None,
                    },
                    &mut |sent| {
                        sent.give(&mut ReplTurn {
                            presenter: &mut presenter,
                            control: &control,
                            dock: &dock,
                        })
                    },
                );
                let Some(done) = done else {
                    gone = true;
                    break;
                };
                let mut failed = None;
                match done.turn.expect("a message's turn") {
                    Ok(TurnEnd::Answer(_)) => {}
                    Ok(TurnEnd::Stopped(StopReason::Interrupted)) => {
                        presenter
                            .show_notice(tr!(lang, m::INTERRUPTED_CLEARED), Role::StatusPending);
                    }
                    Ok(TurnEnd::Stopped(_)) => failed = done.why,
                    Err(error) => failed = Some(error.message(lang)),
                }
                let mut inner = control.lock();
                inner.running = false;
                inner.info.state = None;
                inner.info.unfinished = session.borrow().unfinished.clone();
                // A turn that failed leaves no queue behind (D127).
                let cleared = failed.is_some() && !inner.queue.is_empty();
                if failed.is_some() {
                    inner.queue.clear();
                }
                drop(inner);
                if let Some(text) = failed {
                    presenter.show_notice(&text, Role::StatusFailure);
                }
                if cleared {
                    dock.set_queued(0);
                    presenter.show_notice(tr!(lang, m::ERROR_CLEARED), Role::Muted);
                }
                presenter.set_ready();
                true
            }
        };
        if !kept {
            gone = true;
            break;
        }
    }

    let commands = take_commands(&control);
    if !gone && !commands.is_empty() {
        ask(Command::Append(commands), &mut drop);
    }
    done.store(true, Ordering::SeqCst);
    let _ = reader.join();
    let _ = ticker.join();
    // A recording that goes keeps nothing, and ffmpeg is stopped (AGL-016).
    voice.cancel();
    dock.finish();
    if gone {
        return Err(Error::PlaneGone);
    }
    Ok(end)
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::agent::Compaction;
    use crate::status::compact_notice;
    use crate::function::Function;

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct Fixture {
        input: Input,
        sink: Sink,
        stops: Arc<AtomicUsize>,
        exits: Arc<AtomicUsize>,
        inspects: Arc<AtomicUsize>,
        clip: Arc<Mutex<Clip>>,
        fake: Arc<Fake>,
        /// The fixture's temporary directory, the catalog's own (AGL-102).
        dir: std::path::PathBuf,
    }

    /// The recorder and whisper of the tests (AGL-016).
    #[derive(Default)]
    struct Fake {
        log: Mutex<Vec<String>>,
        /// The recording ended by itself, as at the limit.
        ended: std::sync::atomic::AtomicBool,
        record_error: Mutex<Option<String>>,
        not_ready: std::sync::atomic::AtomicBool,
        text: Mutex<String>,
        /// While true the transcription waits.
        hold: std::sync::atomic::AtomicBool,
    }

    impl Fake {
        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    struct FakeRecording(Arc<Fake>);

    impl crate::record::Recording for FakeRecording {
        fn finished(&mut self) -> bool {
            self.0.ended.load(Ordering::SeqCst)
        }
        fn stop(self: Box<Self>) -> Result<Vec<u8>, Error> {
            self.0.log.lock().unwrap().push("stop".into());
            Ok(b"WAV".to_vec())
        }
        fn cancel(self: Box<Self>) {
            self.0.log.lock().unwrap().push("cancel".into());
        }
    }

    struct FakeDictation(Arc<Fake>);

    impl crate::whisper::Dictation for FakeDictation {
        fn ready(&self) -> Result<(), Error> {
            if self.0.not_ready.load(Ordering::SeqCst) {
                return Err(Error::WhisperNotSet {
                    user_path: "/c/settings.nix".into(),
                    project_path: "/d/settings.nix".into(),
                });
            }
            Ok(())
        }
        fn warm_up(&self, _: &mut dyn FnMut(&str)) -> Result<(), Error> {
            self.0.log.lock().unwrap().push("warm".into());
            Ok(())
        }
        fn transcribe(&self, wav: &[u8], _: &mut dyn FnMut(&str)) -> Result<String, Error> {
            while self.0.hold.load(Ordering::SeqCst) {
                thread::sleep(std::time::Duration::from_millis(5));
            }
            self.0
                .log
                .lock()
                .unwrap()
                .push(format!("transcribe {} bytes", wav.len()));
            Ok(self.0.text.lock().unwrap().clone())
        }
    }

    fn fixture() -> Fixture {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let sink = Sink::default();
        let commands = COMMANDS
            .iter()
            .map(|name| MenuItem {
                name,
                description: String::new(),
            })
            .collect();
        let dock = Arc::new(
            Dock::new(
                Box::new(sink.clone()),
                Theme::plain(),
                Lang::En,
                "minimal".into(),
                "~/project".into(),
                Editor::new(Vec::new(), commands),
                (40, 20),
            )
            .with_context(16384),
        );
        let stops = Arc::new(AtomicUsize::new(0));
        let exits = Arc::new(AtomicUsize::new(0));
        let inspects = Arc::new(AtomicUsize::new(0));
        let clip = Arc::new(Mutex::new(Clip::Empty));
        let fake = Arc::new(Fake::default());
        *fake.text.lock().unwrap() = "hello world".into();
        let voice = Arc::new(VoiceInput::new(
            dock.clone(),
            Arc::new(FakeDictation(fake.clone())),
            {
                let fake = fake.clone();
                Box::new(move |limit| {
                    if let Some(detail) = fake.record_error.lock().unwrap().clone() {
                        return Err(Error::Ffmpeg { detail });
                    }
                    fake.log.lock().unwrap().push(format!("start {limit}"));
                    Ok(Box::new(FakeRecording(fake.clone())))
                })
            },
            60,
            Lang::En,
            Theme::plain(),
        ));
        let dir = std::env::temp_dir().join(format!(
            "agentLIBRE-repl-{}-{n}",
            std::process::id()
        ));
        let input = Input {
            dock,
            control: Arc::new(Control::new(
                Info {
                    session: "s1".into(),
                    context: Some(16384),
                    ..Info::default()
                },
                None,
            )),
            facts: Facts {
                function: "minimal".into(),
                workspace: "~/project".into(),
                plane: None,
                server: "http://127.0.0.1:18080".into(),
            },
            catalog: Arc::new(Catalog {
                plane_dir: None,
                workspace_dir: dir.join("functions"),
                user_dir: dir.join("user"),
                settings: crate::settings::ServerFile::default(),
                current: "minimal".into(),
            }),
            theme: Theme::plain(),
            lang: Lang::En,
            history_file: dir.join("history.jsonl"),
            request_stop: {
                let stops = stops.clone();
                Box::new(move || stops.fetch_add(1, Ordering::SeqCst) > 0)
            },
            exit_now: {
                let exits = exits.clone();
                Box::new(move || {
                    exits.fetch_add(1, Ordering::SeqCst);
                })
            },
            open_inspect: {
                let inspects = inspects.clone();
                Box::new(move || {
                    inspects.fetch_add(1, Ordering::SeqCst);
                    None
                })
            },
            open_browser: None,
            paths: Paths::from_env(|var| {
                (var == "HOME").then(|| std::ffi::OsString::from(&dir))
            })
            .unwrap(),
            workspace: dir.clone(),
            clipboard: {
                let clip = clip.clone();
                Box::new(move || clip.lock().unwrap().clone())
            },
            voice,
            said: Mutex::new(None),
            };
        Fixture {
            input,
            sink,
            stops,
            exits,
            inspects,
            clip,
            fake,
            dir,
        }
    }

    impl Fixture {
        fn output(&self) -> String {
            String::from_utf8(self.sink.0.lock().unwrap().clone()).unwrap()
        }

        fn type_text(&self, text: &str) {
            for ch in text.chars() {
                self.input.key(Key::Char(ch));
            }
        }

        fn inner(&self) -> MutexGuard<'_, Inner> {
            self.input.control.lock()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(self.input.history_file.parent().unwrap());
        }
    }

    #[test]
    fn messages_wait_in_a_queue() {
        let f = fixture();
        f.type_text("hello");
        f.input.key(Key::Enter);
        assert_eq!(f.inner().texts(), ["hello"]);
        assert!(f.output().contains("queued 1"));
        // Messages sent during a turn queue up in order.
        f.inner().running = true;
        f.type_text("second");
        f.input.key(Key::Enter);
        assert_eq!(f.inner().texts(), ["hello", "second"]);
        // The history file holds what was sent.
        let history = editor::load_history(&f.input.history_file);
        assert_eq!(history, ["hello", "second"]);
    }

    /// The text of the line being written.
    fn line(f: &Fixture) -> String {
        f.input.dock.input_text()
    }

    #[test]
    fn ctrl_o_opens_the_inspect_mode() {
        let f = fixture();
        assert_eq!(f.inspects.load(Ordering::SeqCst), 0);
        // Ctrl-O takes the alternate screen; it does not go to the editor.
        f.input.key(Key::Ctrl('o'));
        assert_eq!(f.inspects.load(Ordering::SeqCst), 1);
        assert_eq!(line(&f), "");
    }

    #[test]
    fn ctrl_v_puts_a_placeholder_for_a_picture_and_text_for_text() {
        let f = fixture();
        *f.clip.lock().unwrap() = Clip::Image(b"PNG".to_vec());
        f.type_text("look ");
        f.input.key(Key::Ctrl('v'));
        assert_eq!(line(&f), "look [Image#1] ");
        // A second one is the second image; the first stays waiting.
        f.input.key(Key::Ctrl('v'));
        assert_eq!(line(&f), "look [Image#1] [Image#2] ");
        assert_eq!(f.inner().pending.len(), 2);
        assert_eq!(f.inner().pending[1].number, 2);
        assert_eq!(f.inner().pending[0].source.bytes, b"PNG");
        // Text from the clipboard is pasted as text.
        *f.clip.lock().unwrap() = Clip::Text("words".into());
        f.input.key(Key::Ctrl('v'));
        assert!(line(&f).ends_with("[Image#2] words"));
        // Nothing in the clipboard: said so, the line is as it was.
        *f.clip.lock().unwrap() = Clip::Empty;
        let before = line(&f);
        f.input.key(Key::Ctrl('v'));
        assert_eq!(line(&f), before);
        assert!(f.output().contains("the clipboard holds nothing to paste"));
    }

    #[test]
    fn the_attached_media_go_with_the_message_that_is_sent() {
        let f = fixture();
        *f.clip.lock().unwrap() = Clip::Image(b"PNG".to_vec());
        f.input.key(Key::Ctrl('v'));
        f.type_text("what is this?");
        f.input.key(Key::Enter);
        {
            let inner = f.inner();
            assert_eq!(inner.texts(), ["[Image#1] what is this?"]);
            assert_eq!(inner.queue[0].pending.len(), 1);
            assert_eq!(inner.queue[0].pending[0].placeholder(), "[Image#1]");
            assert!(inner.pending.is_empty());
        }
        // The next picture is numbered after the one that is queued only
        // when the session holds it; before that it is #1 again (the
        // numbers come from the session and the line being written).
        f.inner().media_counts = [1, 0, 0];
        f.input.key(Key::Ctrl('v'));
        assert_eq!(line(&f), "[Image#2] ");
    }

    #[test]
    fn a_deleted_placeholder_leaves_its_medium_unsent_but_waiting_in_the_message() {
        let f = fixture();
        *f.clip.lock().unwrap() = Clip::Image(b"PNG".to_vec());
        f.input.key(Key::Ctrl('v'));
        // Backspace over the space and the placeholder.
        for _ in 0.."[Image#1] ".len() {
            f.input.key(Key::Backspace);
        }
        f.type_text("plain");
        f.input.key(Key::Enter);
        let inner = f.inner();
        // The medium travels with the message, and `attach::store` drops
        // it because the text does not name it (D21).
        assert_eq!(inner.texts(), ["plain"]);
        assert_eq!(inner.queue[0].pending.len(), 1);
    }

    #[test]
    fn attach_takes_a_file_and_puts_its_placeholder_in_the_line() {
        let f = fixture();
        let dir = std::env::temp_dir().join(format!("agentLIBRE-attach-repl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("voice note.wav");
        std::fs::write(&path, b"RIFF").unwrap();
        f.type_text(&format!("/attach \"{}\"", path.display()));
        f.input.key(Key::Enter);
        assert_eq!(line(&f), "[Audio#1] ");
        assert!(f.output().contains("attached [Audio#1] (voice note.wav)"));
        assert!(f.inner().queue.is_empty());
        // A file that is not a medium, and a missing one, are told; the
        // line is cleared first, the commands are read at its start.
        f.input.key(Key::Ctrl('u'));
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        f.type_text(&format!("/attach {}", dir.join("a.txt").display()));
        f.input.key(Key::Enter);
        // The notice wraps at the width of the dock; its last word is whole.
        assert!(f.output().contains("audio"), "{}", f.output());
        f.type_text("/attach");
        f.input.key(Key::Enter);
        assert!(f.output().contains("usage: /attach"));
        assert_eq!(f.inner().pending.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_drops_the_waiting_media() {
        let f = fixture();
        *f.clip.lock().unwrap() = Clip::Image(b"PNG".to_vec());
        f.input.key(Key::Ctrl('v'));
        assert_eq!(f.inner().pending.len(), 1);
        f.input.key(Key::Ctrl('u'));
        f.type_text("/new");
        f.input.key(Key::Enter);
        assert!(f.inner().requests.contains(&Request::New));
        assert!(f.inner().pending.is_empty());
    }

    /// Waits until `condition` holds, for the threads of the voice input.
    fn eventually(what: &str, condition: impl Fn() -> bool) {
        let started = std::time::Instant::now();
        while !condition() {
            assert!(started.elapsed().as_secs() < 5, "{what}");
            thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn ctrl_r_records_and_the_second_press_puts_the_text_in_the_line() {
        let f = fixture();
        f.type_text("say ");
        f.input.key(Key::Ctrl('r'));
        // The limit is the settings' (D5; AGL-039, D20); the whisper unit is started
        // while the human speaks (D6); the dock says so (D9).
        assert_eq!(f.fake.log()[0], "start 60");
        eventually("warm up", || f.fake.log().contains(&"warm".to_string()));
        // The test dock is 40 columns wide: the label is cut at its end.
        assert!(f.output().contains("Recording 0:00"), "{}", f.output());
        assert!(f.input.voice.recording());
        f.input.key(Key::Ctrl('r'));
        eventually("the text", || f.input.dock.input_text() == "say hello world ");
        assert_eq!(f.fake.log().last().unwrap(), "transcribe 3 bytes");
        assert!(f.fake.log().contains(&"stop".to_string()));
        assert!(f.output().contains("Transcribing…"));
        assert!(!f.input.voice.recording());
        // Nothing was queued: the human sends the line when it is read.
        assert!(f.inner().queue.is_empty());
        // And it can be done again.
        f.input.key(Key::Ctrl('r'));
        assert!(f.input.voice.recording());
    }

    #[test]
    fn escape_and_ctrl_c_cancel_the_recording_and_keep_nothing() {
        let f = fixture();
        for key in [Key::Escape, Key::Ctrl('c')] {
            f.input.key(Key::Ctrl('r'));
            f.input.key(key);
            assert!(!f.input.voice.recording());
        }
        let log = f.fake.log();
        assert_eq!(log.iter().filter(|l| *l == "cancel").count(), 2);
        assert!(!log.iter().any(|l| l.starts_with("transcribe")));
        assert!(f.input.dock.input_is_empty());
        // Ctrl-C at a recording is not a stop of the turn (D2).
        assert_eq!(f.stops.load(Ordering::SeqCst), 0);
        // Without a recording, Escape does nothing to the voice.
        f.input.key(Key::Escape);
        assert_eq!(f.fake.log().iter().filter(|l| *l == "cancel").count(), 2);
    }

    #[test]
    fn the_shortcut_works_while_a_turn_runs() {
        let f = fixture();
        f.inner().running = true;
        f.input.key(Key::Ctrl('r'));
        f.input.key(Key::Ctrl('r'));
        eventually("the text", || f.input.dock.input_text() == "hello world ");
        // The turn went on: it was not interrupted, nothing was queued.
        assert!(f.inner().running);
        assert_eq!(f.stops.load(Ordering::SeqCst), 0);
        // Ctrl-C with no recording is the usual stop of the turn.
        f.input.key(Key::Ctrl('c'));
        assert_eq!(f.stops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn whisper_that_is_not_set_is_told_before_the_human_speaks() {
        let f = fixture();
        f.fake.not_ready.store(true, Ordering::SeqCst);
        f.input.key(Key::Ctrl('r'));
        assert!(!f.input.voice.recording());
        assert!(f.fake.log().is_empty());
        assert!(f.output().contains("whisper"), "{}", f.output());
        // A recorder that fails (no microphone) is told too, and the next
        // press tries again.
        f.fake.not_ready.store(false, Ordering::SeqCst);
        *f.fake.record_error.lock().unwrap() = Some("default: Connection refused".into());
        f.input.key(Key::Ctrl('r'));
        assert!(!f.input.voice.recording());
        assert!(f.output().contains("Connection"), "{}", f.output());
        *f.fake.record_error.lock().unwrap() = None;
        f.input.key(Key::Ctrl('r'));
        assert!(f.input.voice.recording());
    }

    #[test]
    fn a_recording_that_ends_at_the_limit_is_transcribed_by_itself() {
        let f = fixture();
        f.input.key(Key::Ctrl('r'));
        f.fake.ended.store(true, Ordering::SeqCst);
        eventually("the text", || f.input.dock.input_text() == "hello world ");
        assert!(!f.input.voice.recording());
        assert!(f.fake.log().contains(&"stop".to_string()));
    }

    #[test]
    fn silence_is_told_and_writes_nothing() {
        let f = fixture();
        *f.fake.text.lock().unwrap() = "  ".into();
        f.input.key(Key::Ctrl('r'));
        f.input.key(Key::Ctrl('r'));
        eventually("the notice", || f.output().contains("nothing was heard"));
        assert!(f.input.dock.input_is_empty());
    }

    #[test]
    fn a_press_while_the_text_is_made_does_nothing_and_the_end_keeps_the_recording_apart() {
        let f = fixture();
        f.fake.hold.store(true, Ordering::SeqCst);
        f.input.key(Key::Ctrl('r'));
        f.input.key(Key::Ctrl('r'));
        eventually("stop", || f.fake.log().contains(&"stop".to_string()));
        // Transcribing: another press starts nothing.
        f.input.key(Key::Ctrl('r'));
        assert_eq!(f.fake.log().iter().filter(|l| l.starts_with("start")).count(), 1);
        f.input.voice.cancel();
        f.fake.hold.store(false, Ordering::SeqCst);
        eventually("the text", || f.input.dock.input_text() == "hello world ");
    }

    #[test]
    fn the_end_of_the_session_cancels_a_recording() {
        let f = fixture();
        f.input.key(Key::Ctrl('r'));
        // The warm-up runs on its own thread; it logs before the cancel
        // is looked at, or it would race with it (seen in the parallel
        // run of the Nix build, AGL-040).
        eventually("warm up", || f.fake.log().contains(&"warm".to_string()));
        f.input.voice.cancel();
        assert_eq!(f.fake.log().last().unwrap(), "cancel");
        assert!(!f.input.voice.recording());
    }

    #[test]
    fn remember_with_a_name_is_for_the_main_thread_and_waits_for_no_turn() {
        let f = fixture();
        f.type_text("/remember Build and test | cargo build / cargo test");
        f.input.key(Key::Enter);
        assert_eq!(
            f.inner().requests.remember(),
            Some(("Build and test".into(), "cargo build / cargo test".into()))
        );
        assert!(f.inner().queue.is_empty());
        // The main thread takes it before any message.
        f.inner().queue.push_back("later".into());
        assert!(matches!(
            wait_for_next(&f.input.control),
            Next::Remember(name, text, _) if name == "Build and test" && text == "cargo build / cargo test"
        ));
        assert!(f.inner().requests.remember().is_none());
        // During a turn it is refused, as `/new` is, and nothing is kept.
        f.inner().running = true;
        f.type_text("/remember Name | text");
        f.input.key(Key::Enter);
        assert!(f.output().contains("/remember is not available during a turn"));
        assert!(f.inner().requests.remember().is_none());
    }

    #[test]
    fn remember_without_a_name_is_an_ordinary_message_even_during_a_turn() {
        let f = fixture();
        f.inner().running = true;
        f.type_text("/remember cargo build / cargo test");
        f.input.key(Key::Enter);
        assert_eq!(f.inner().texts(), ["remember: cargo build / cargo test"]);
        assert!(f.inner().requests.remember().is_none());
    }

    #[test]
    fn remember_alone_or_with_an_empty_name_or_text_tells_the_usage() {
        let f = fixture();
        for line in ["/remember", "/remember | text", "/remember name |"] {
            f.type_text(line);
            f.input.key(Key::Enter);
        }
        assert!(f.output().contains("usage: /remember"));
        assert!(f.inner().requests.remember().is_none());
        assert!(f.inner().queue.is_empty());
    }

    #[test]
    fn rename_is_for_the_main_thread_and_waits_for_no_turn() {
        // AGL-062, D7.
        let f = fixture();
        f.type_text("/rename  gemma grammar ");
        f.input.key(Key::Enter);
        assert_eq!(f.inner().requests.rename().as_deref(), Some("gemma grammar"));
        assert!(f.inner().queue.is_empty());
        // The main thread takes it before any message.
        f.inner().queue.push_back("later".into());
        assert!(matches!(
            wait_for_next(&f.input.control),
            Next::Rename(name) if name == "gemma grammar"
        ));
        assert!(f.inner().requests.rename().is_none());
        f.inner().queue.clear();
        // During a turn it is refused, and nothing is kept.
        f.inner().running = true;
        f.type_text("/rename other");
        f.input.key(Key::Enter);
        assert!(f.output().contains("/rename is not available during a turn"));
        assert!(f.inner().requests.rename().is_none());
    }

    #[test]
    fn rename_without_a_name_tells_the_usage() {
        // AGL-062, D7.
        let f = fixture();
        f.type_text("/rename");
        f.input.key(Key::Enter);
        assert!(f.output().contains("usage: /rename <name>"));
        assert!(f.inner().requests.rename().is_none());
        assert!(f.inner().queue.is_empty());
    }

    #[test]
    fn memory_is_for_the_main_thread_and_waits_for_no_turn() {
        // AGL-060, D5.
        let f = fixture();
        f.type_text("/memory list");
        f.input.key(Key::Enter);
        assert_eq!(
            f.inner().requests.memory(),
            Some(("/memory list".into(), MemoryAction::List))
        );
        f.inner().queue.push_back("later".into());
        assert!(matches!(
            wait_for_next(&f.input.control),
            Next::Memory(line, MemoryAction::List) if line == "/memory list"
        ));
        f.inner().queue.clear();
        // During a turn it is refused, and nothing is kept.
        f.inner().running = true;
        f.type_text("/memory remove tea");
        f.input.key(Key::Enter);
        assert!(f.output().contains("/memory is not available during a turn"));
        assert!(f.inner().requests.memory().is_none());
        f.inner().running = false;
        // A wrong form tells the usage, recorded as a failure.
        std::mem::take(&mut f.inner().commands);
        f.type_text("/memory add x");
        f.input.key(Key::Enter);
        assert!(f.output().contains("usage: /memory"));
        assert!(f.inner().requests.memory().is_none());
        assert!(f.inner().commands[0].failed);
    }

    /// A fixture with the question of `/memory remove tea` waiting.
    fn asked() -> Fixture {
        let f = fixture();
        f.inner().question = Some(Question {
            line: "/memory remove tea".into(),
            name: "tea".into(),
        });
        f
    }

    #[test]
    fn the_answer_yes_goes_to_the_main_thread_and_any_other_line_does_not() {
        // AGL-060, D7.
        for answer in ["y", "YES"] {
            let f = asked();
            f.type_text(answer);
            f.input.key(Key::Enter);
            assert!(f.inner().question.is_none());
            assert_eq!(
                f.inner().requests.memory(),
                Some((
                    "/memory remove tea".into(),
                    MemoryAction::Remove { name: "tea".into(), yes: true }
                ))
            );
            assert!(f.inner().commands.is_empty());
        }
        // Enter alone, another word, or a message: no, and no turn from it.
        for answer in ["", "n", "yes please", "/quit"] {
            let f = asked();
            f.type_text(answer);
            f.input.key(Key::Enter);
            assert!(f.inner().question.is_none(), "{answer:?}");
            assert!(f.inner().requests.memory().is_none(), "{answer:?}");
            assert!(f.inner().queue.is_empty(), "{answer:?}");
            assert!(!f.inner().requests.quit(), "{answer:?}");
            assert!(f.output().contains("not removed: tea"), "{answer:?}");
            let commands = std::mem::take(&mut f.inner().commands);
            assert_eq!(
                commands,
                [CommandRun {
                    line: "/memory remove tea".into(),
                    failed: false,
                    output: Some("not removed: tea".into()),
                }]
            );
        }
    }

    #[test]
    fn ctrl_c_at_the_question_is_no_and_does_not_leave() {
        // AGL-060, D7.
        let f = asked();
        f.type_text("y");
        f.input.key(Key::Ctrl('c'));
        assert!(f.inner().question.is_none());
        assert!(f.inner().requests.memory().is_none());
        assert!(!f.inner().requests.quit());
        assert!(f.input.dock.input_is_empty());
        assert!(f.output().contains("not removed: tea"));
    }

    #[test]
    fn no_turn_starts_while_the_question_waits() {
        // AGL-060, D7: a line typed before the question was shown stays queued.
        let f = asked();
        f.inner().queue.push_back("typed before".into());
        let control = f.input.control.clone();
        let waiter = thread::spawn(move || wait_for_next(&control));
        thread::sleep(std::time::Duration::from_millis(50));
        assert!(!waiter.is_finished());
        f.type_text("y");
        f.input.key(Key::Enter);
        assert!(matches!(waiter.join().unwrap(), Next::Memory(_, _)));
        assert_eq!(f.inner().texts(), ["typed before"]);
    }

    #[test]
    fn what_a_command_says_goes_to_the_main_thread_to_record() {
        // AGL-047, D5.
        let f = fixture();
        for line in ["/status", "/think nonsense", "/attach /no/such/file", "/nope"] {
            f.type_text(line);
            f.input.key(Key::Enter);
        }
        let commands = std::mem::take(&mut f.inner().commands);
        let lines: Vec<(&str, bool)> = commands
            .iter()
            .map(|run| (run.line.as_str(), run.failed))
            .collect();
        assert_eq!(
            lines,
            [
                ("/status", false),
                ("/think nonsense", true),
                ("/attach /no/such/file", true),
                ("/nope", true),
            ]
        );
        assert!(commands[0].output.is_some());
        // What only the main thread can run is recorded there.
        f.type_text("/new");
        f.input.key(Key::Enter);
        assert!(f.inner().commands.is_empty());
    }

    #[test]
    fn a_silent_command_is_recorded_too() {
        // AGL-102: silence is a result too — a `/remember <text>` that
        // turns into a message leaves its line in the session; what the
        // main thread runs is recorded there, not twice.
        let f = fixture();
        f.type_text("/remember what he wants");
        f.input.key(Key::Enter);
        let commands = std::mem::take(&mut f.inner().commands);
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].line, "/remember what he wants");
        assert_eq!(commands[0].output, None);
        std::mem::take(&mut f.inner().queue);
        f.type_text("/rename gemma grammar");
        f.input.key(Key::Enter);
        assert!(f.inner().commands.is_empty(), "the main thread records it");
    }

    #[test]
    fn commands_run_at_once_or_wait_for_the_turn() {
        let f = fixture();
        f.type_text("/help");
        f.input.key(Key::Enter);
        assert!(f.output().contains("/quit"));
        assert!(f.output().contains("/think    the reasoning level"));
        assert!(f.output().contains("/compact  replace the history"));
        assert!(f.output().contains("Shift-Enter"));
        f.type_text("/status");
        f.input.key(Key::Enter);
        assert!(f.output().contains("session: s1"));
        assert!(f.output().contains("context: 16384 tokens"));
        assert!(f.output().contains("state: waiting for a message"));
        f.type_text("/nope");
        f.input.key(Key::Enter);
        assert!(f.output().contains("unknown command /nope"));
        assert!(f.inner().queue.is_empty());

        // During a turn /new, /compact and /quit are refused (D134, D140).
        f.inner().running = true;
        for command in ["/new", "/compact", "/quit", "/exit"] {
            f.type_text(command);
            f.input.key(Key::Enter);
        }
        assert!(f.output().contains("/new is not available during a turn"));
        assert!(
            f.output()
                .contains("/compact is not available during a turn")
        );
        assert!(f.inner().requests.0.is_empty());
        // /status still works and shows the state.
        f.inner().info.state = Some("verify".into());
        f.type_text("/status");
        f.input.key(Key::Enter);
        assert!(f.output().contains("state: verify"));

        f.inner().running = false;
        f.type_text("/new");
        f.input.key(Key::Enter);
        assert!(f.inner().requests.contains(&Request::New));
        f.inner().requests = Requests::default();
        f.type_text("/compact");
        f.input.key(Key::Enter);
        assert!(f.inner().requests.contains(&Request::Compact));
        f.type_text("/exit");
        f.input.key(Key::Enter);
        assert!(f.inner().requests.quit());
    }

    #[test]
    fn function_asks_the_main_thread_for_the_list_or_the_switch() {
        // AGL-036, D4, D9: the main thread shows the list or checks the
        // Function; not during a turn.
        let f = fixture();
        f.inner().running = true;
        f.type_text("/function other");
        f.input.key(Key::Enter);
        assert!(f.output().contains("/function is not available during a turn"));
        assert!(f.inner().requests.0.is_empty());
        f.inner().running = false;
        f.type_text("/function");
        f.input.key(Key::Enter);
        assert!(f.inner().requests.contains(&Request::Functions));
        f.inner().requests = Requests::default();
        f.type_text("/function other");
        f.input.key(Key::Enter);
        assert!(f.inner().requests.contains(&Request::Switch("other".into())));
    }

    #[test]
    fn plane_asks_the_main_thread_and_waits_for_the_turn() {
        // AGL-077, D12: `/plane` says the planes, `/plane <name>` switches.
        let f = fixture();
        f.inner().running = true;
        f.type_text("/plane work");
        f.input.key(Key::Enter);
        assert!(f.output().contains("/plane is not available during a turn"));
        assert!(f.inner().requests.0.is_empty());
        f.inner().running = false;
        f.type_text("/plane");
        f.input.key(Key::Enter);
        assert!(f.inner().requests.contains(&Request::Planes));
        f.inner().requests = Requests::default();
        f.type_text("/plane work");
        f.input.key(Key::Enter);
        assert!(f.inner().requests.contains(&Request::PlaneSwitch("work".into())));
    }

    #[test]
    fn think_shows_and_sets_the_reasoning_level() {
        let f = fixture();
        // Alone it shows the current level: the template's default
        // while unset (AGL-013).
        f.type_text("/think");
        f.input.key(Key::Enter);
        assert!(f.output().contains("reasoning: default"));
        // With a value it sets it and tells it.
        f.type_text("/think low");
        f.input.key(Key::Enter);
        assert!(f.output().contains("reasoning: low"));
        assert_eq!(f.inner().reasoning, Some(ReasoningLevel::Low));
        // A value outside the four is told; the level stays.
        f.type_text("/think high");
        f.input.key(Key::Enter);
        assert!(f.output().contains("invalid reasoning level `high`"));
        assert_eq!(f.inner().reasoning, Some(ReasoningLevel::Low));
        // /status carries the line (the default when unset).
        f.type_text("/status");
        f.input.key(Key::Enter);
        assert!(f.output().contains("reasoning: low"));
        f.inner().reasoning = None;
        f.type_text("/status");
        f.input.key(Key::Enter);
        assert!(f.output().contains("reasoning: default"));
        // It works during a turn, as /status does.
        f.inner().running = true;
        f.type_text("/think off");
        f.input.key(Key::Enter);
        assert_eq!(f.inner().reasoning, Some(ReasoningLevel::Off));
    }

    #[test]
    fn ctrl_c_during_a_turn_stops_it_and_clears_the_queue() {
        let f = fixture();
        f.inner().running = true;
        f.inner().queue.push_back("later".into());
        f.input.key(Key::Ctrl('c'));
        assert_eq!(f.stops.load(Ordering::SeqCst), 1);
        assert!(f.inner().queue.is_empty());
        assert_eq!(f.exits.load(Ordering::SeqCst), 0);
        // A second Ctrl-C during the same turn leaves at once (D120).
        f.input.key(Key::Ctrl('c'));
        assert_eq!(f.exits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn ctrl_c_at_the_prompt_clears_the_line_or_quits() {
        let f = fixture();
        f.type_text("draft");
        f.input.key(Key::Ctrl('c'));
        assert!(f.input.dock.input_is_empty());
        assert!(!f.inner().requests.quit());
        assert_eq!(f.stops.load(Ordering::SeqCst), 0);
        f.input.key(Key::Ctrl('c'));
        assert!(f.inner().requests.quit());
    }

    #[test]
    fn ctrl_d_quits_only_when_idle() {
        let f = fixture();
        f.inner().running = true;
        f.input.key(Key::Ctrl('d'));
        assert!(!f.inner().requests.quit());
        assert!(f.output().contains("Ctrl-D is not available during a turn"));
        f.inner().running = false;
        f.input.key(Key::Ctrl('d'));
        assert!(f.inner().requests.quit());
    }

    #[test]
    fn what_compact_tells() {
        let none = Path::new("/none");
        let function = Function::load(&[none, none], "fixture-minimal").unwrap();
        let notice = |result| compact_notice(result, &function, Lang::En);
        assert_eq!(notice(Ok(Compaction::Done)), None);
        assert_eq!(
            notice(Ok(Compaction::Nothing)),
            Some(("Nothing to compact.".into(), Role::Muted))
        );
        let (text, role) = notice(Ok(Compaction::Impossible { limit: 4096 })).unwrap();
        assert!(text.starts_with("cannot compact: the history and max_compact_tokens (4096)"));
        assert_eq!(role, Role::StatusFailure);
        assert_eq!(
            notice(Err(Error::Interrupted)),
            Some(("interrupted".into(), Role::StatusPending))
        );
    }

    #[test]
    fn the_main_thread_takes_what_the_user_sent() {
        let control = Control::new(Info::default(), None);
        control.lock().queue.push_back("one".into());
        assert!(matches!(wait_for_next(&control), Next::Message(m) if m.text == "one"));
        assert!(control.lock().running);
        control.lock().running = false;
        control.lock().requests.push(Request::New);
        assert!(matches!(wait_for_next(&control), Next::New));
        // `/function` (AGL-036, D4, D9).
        control.lock().requests.push(Request::Functions);
        assert!(matches!(wait_for_next(&control), Next::Functions));
        control.lock().requests.push(Request::Switch("other".into()));
        assert!(matches!(wait_for_next(&control), Next::Switch(name) if name == "other"));
        assert!(!control.lock().running);
        // A compaction counts as running, so that Ctrl-C can stop it.
        control.lock().requests.push(Request::Compact);
        assert!(matches!(wait_for_next(&control), Next::Compact));
        assert!(control.lock().running);
        control.lock().requests.push(Request::Quit);
        assert!(matches!(wait_for_next(&control), Next::Quit));
    }

    #[derive(Default)]
    struct Lines(Arc<Mutex<Vec<String>>>);

    impl Screen for Lines {
        fn columns(&self) -> usize {
            40
        }
        fn room(&self) -> usize {
            16
        }
        fn print(&self, lines: &[String]) {
            self.0.lock().unwrap().extend_from_slice(lines);
        }
        fn live(&self, _lines: &[String]) {}
        fn erase(&self, _count: usize) -> bool {
            true
        }
        fn activity(&self, _activity: crate::phase::Phase) {}
    }

    #[test]
    fn the_reporter_forwards_the_decoded_tokens() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let screen: Arc<dyn Screen> = Arc::new(Lines(lines.clone()));
        let mut presenter = Presenter::new(
            screen,
            Theme::plain(),
            Lang::En,
            crate::settings::ReplSettings::default(),
        );
        presenter.begin_turn();
        let control = Control::new(Info::default(), None);
        show(&mut presenter, &control, Report::Decoded(30));
        show(&mut presenter, &control, Report::ActionText("Done.".into()));
        show(&mut presenter, &control, Report::Event(Event::Answer("Done.".into())));
        assert!(
            lines
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("tok/s"))
        );
    }

    /// AGL-102, D2: `/function show` prints the source and the file a run
    /// loads it from; without a name it is the usage.
    #[test]
    fn function_show_prints_the_source_and_its_file() {
        let f = fixture();
        let dir = f.dir.join("functions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mine.nix"), "{ agl, ... }: { }\n").unwrap();
        f.type_text("/function show mine");
        f.input.key(Key::Enter);
        assert!(f.output().contains("{ agl, ... }: { }"));
        f.type_text("/function show");
        f.input.key(Key::Enter);
        assert!(f.output().contains("usage: /function show <name>"));
        // The command and what it said stand in the session (AGL-101):
        // the path, unwrapped, and the usage's failure.
        let inner = f.inner();
        assert_eq!(inner.commands.len(), 2);
        assert!(!inner.commands[0].failed);
        assert!(inner.commands[0]
            .output
            .as_deref()
            .is_some_and(|o| o.contains(&dir.join("mine.nix").display().to_string())));
        assert!(inner.commands[1].failed);
    }

    /// AGL-102, D3: `/function new` copies to the workspace's `functions/`,
    /// refuses an existing name, and leaves nothing of a text that does
    /// not load.
    #[test]
    fn function_new_copies_and_refuses() {
        const GOOD: &str = r#"{ agl, ... }: { policy = { system = "s"; initial = "work"; state.work.answer = true; }; model = { format = agl.formats."qwen3.8"; weights = "p.gguf"; }; }"#;
        let f = fixture();
        let dir = f.dir.join("functions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mine.nix"), GOOD).unwrap();
        f.type_text("/function new twin mine");
        f.input.key(Key::Enter);
        assert_eq!(std::fs::read_to_string(dir.join("twin.nix")).unwrap(), GOOD);
        assert!(f.output().contains("a copy of mine"));
        f.type_text("/function new twin mine");
        f.input.key(Key::Enter);
        assert!(f.output().contains("already exists"));
        // A source that does not load leaves no half-written file.
        std::fs::write(dir.join("broken.nix"), "{ agl, ... }: { nope").unwrap();
        f.type_text("/function new copy broken");
        f.input.key(Key::Enter);
        assert!(!dir.join("copy.nix").exists());
        // The source by default is the current Function — "minimal"
        // here, and there is none of that name.
        f.type_text("/function new solo");
        f.input.key(Key::Enter);
        assert!(f.output().contains("not found"));
    }

}
