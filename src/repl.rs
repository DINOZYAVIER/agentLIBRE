//! The interactive session (D34, D115-D138): a line editor and a dock in
//! the terminal while the turn runs, or lines on stdin without a terminal.

mod dock;
mod editor;
mod markdown;
mod present;
mod text;

use std::collections::VecDeque;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;

use dock::Dock;
use editor::{Editor, MenuItem, Outcome, save_history};
use present::{Presenter, Screen, notice_lines};

use crate::agent::{Agent, Report, TurnEnd};
use crate::backend::{Backend, ToolCall};
use crate::error::Error;
use crate::function::Function;
use crate::i18n::{self, Lang, repl as texts};
use crate::interrupt;
use crate::report::StderrReport;
use crate::session::{Session, StopReason};
use crate::settings::ReplSettings;
use crate::term::{self, Key, Parser, RawGuard};
use crate::theme::{Role, Theme};
use crate::tools::ToolResult;

/// The `/` commands (D118).
const COMMANDS: [&str; 5] = ["/quit", "/exit", "/help", "/status", "/new"];

/// Everything a session needs.
pub struct Setup<'a> {
    pub function: &'a Function,
    pub function_name: &'a str,
    pub backend: &'a dyn Backend,
    pub session: Session,
    /// Whether the session existed before this run (D126).
    pub resumed: bool,
    pub sessions_dir: &'a Path,
    pub context: usize,
    pub server_url: &'a str,
    pub theme: Theme,
    pub repl: ReplSettings,
    pub lang: Lang,
    pub history_file: PathBuf,
}

/// What `/status` tells besides the session (D134).
#[derive(Clone)]
struct Facts {
    function: String,
    workspace: String,
    server: String,
    context: usize,
}

/// What changes during the session.
#[derive(Clone, Default)]
struct Info {
    session: String,
    /// The state of the last visit.
    state: Option<String>,
    /// The state of an interrupted turn.
    unfinished: Option<String>,
}

#[derive(Default)]
struct Inner {
    queue: VecDeque<String>,
    /// A turn is running.
    running: bool,
    quit: bool,
    new_session: bool,
    info: Info,
}

/// What the input thread and the turn thread share (D119).
struct Control {
    inner: Mutex<Inner>,
    wake: Condvar,
}

impl Control {
    fn new(info: Info) -> Control {
        Control {
            inner: Mutex::new(Inner {
                info,
                ..Inner::default()
            }),
            wake: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_state(&self, state: Option<String>) {
        self.lock().info.state = state;
    }
}

impl Inner {
    /// A turn runs or messages wait for one.
    fn busy(&self) -> bool {
        self.running || !self.queue.is_empty()
    }
}

/// `/help` (D134).
fn help_text(lang: Lang) -> String {
    let mut text = String::new();
    for name in COMMANDS {
        text.push_str(&format!("{name:<9} {}\n", texts::command(name, lang)));
    }
    text.push_str(texts::keys(lang));
    text
}

/// `/status` (D134).
fn status_text(facts: &Facts, info: &Info, lang: Lang) -> String {
    let state = match (&info.unfinished, &info.state) {
        (Some(state), _) => texts::unfinished(state, lang),
        (None, Some(state)) => state.clone(),
        (None, None) => texts::idle(lang).to_owned(),
    };
    [
        texts::status_line("session", &info.session, lang),
        texts::status_line("function", &facts.function, lang),
        texts::status_line("workspace", &facts.workspace, lang),
        texts::status_line("server", &facts.server, lang),
        texts::status_line("context", &texts::tokens(facts.context, lang), lang),
        texts::status_line("state", &state, lang),
    ]
    .join("\n")
}

fn display_path(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Reacts to what the user does at the prompt (D118, D120).
struct Input {
    dock: Arc<Dock>,
    control: Arc<Control>,
    facts: Facts,
    theme: Theme,
    lang: Lang,
    history_file: PathBuf,
    /// Asks the running turn to stop; true when it was already asked.
    request_stop: Box<dyn Fn() -> bool + Send + Sync>,
    /// Leaves at once, after giving the terminal back (D120, D135).
    exit_now: Box<dyn Fn() + Send + Sync>,
}

impl Input {
    fn notice(&self, text: &str, role: Role) {
        let columns = self.dock.columns();
        self.dock
            .print(&notice_lines(&self.theme, text, columns, role));
    }

    /// Enter.
    fn submitted(&self, message: String) {
        let _ = save_history(&self.history_file, &self.dock.history());
        let trimmed = message.trim();
        if trimmed.is_empty() {
            return;
        }
        if !trimmed.starts_with('/') {
            let mut inner = self.control.lock();
            inner.queue.push_back(message);
            let queued = inner.queue.len();
            drop(inner);
            self.dock.set_queued(queued);
            self.control.wake.notify_all();
            return;
        }
        let name = trimmed.split_whitespace().next().unwrap_or(trimmed);
        match name {
            "/help" => self.notice(&help_text(self.lang), Role::Muted),
            "/status" => {
                let info = self.control.lock().info.clone();
                self.notice(&status_text(&self.facts, &info, self.lang), Role::Muted);
            }
            "/quit" | "/exit" | "/new" => {
                let mut inner = self.control.lock();
                if inner.busy() {
                    drop(inner);
                    self.notice(&texts::busy(name, self.lang), Role::StatusPending);
                    return;
                }
                if name == "/new" {
                    inner.new_session = true;
                } else {
                    inner.quit = true;
                }
                drop(inner);
                self.control.wake.notify_all();
            }
            _ => self.notice(
                &texts::unknown_command(name, self.lang),
                Role::StatusPending,
            ),
        }
    }

    /// Ctrl-C (D120).
    fn interrupt(&self) {
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
            self.notice(texts::interrupted_cleared(self.lang), Role::StatusPending);
        } else if self.dock.input_is_empty() {
            inner.quit = true;
            drop(inner);
            self.control.wake.notify_all();
        } else {
            drop(inner);
            self.dock.clear_input();
        }
    }

    /// Ctrl-D on an empty prompt, or the end of input.
    fn end_of_input(&self) {
        self.control.lock().quit = true;
        self.control.wake.notify_all();
    }

    fn key(&self, key: Key) {
        match self.dock.key(key) {
            Outcome::Edited => {}
            Outcome::Submit(message) => self.submitted(message),
            Outcome::Interrupt => self.interrupt(),
            Outcome::Quit => {
                if self.control.lock().busy() {
                    self.notice(&texts::busy("Ctrl-D", self.lang), Role::StatusPending);
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

/// The report of a turn: the presenter, and what `/status` shows.
struct Reporter<'a> {
    presenter: &'a mut Presenter,
    control: &'a Control,
}

impl Report for Reporter<'_> {
    fn state(&mut self, state: &str) {
        self.control.set_state(Some(state.into()));
        self.presenter.state(state);
    }
    fn thinking(&mut self, piece: &str) {
        self.presenter.thinking(piece);
    }
    fn thinking_done(&mut self) {
        self.presenter.thinking_done();
    }
    fn action_text(&mut self, piece: &str) {
        self.presenter.action_text(piece);
    }
    fn text(&mut self, text: &str) {
        self.presenter.text(text);
    }
    fn answer(&mut self, text: &str) {
        self.presenter.answer(text);
    }
    fn call(&mut self, call: &ToolCall) {
        self.presenter.call(call);
    }
    fn result(&mut self, call: &ToolCall, result: &ToolResult) {
        self.presenter.result(call, result);
    }
}

/// What the main thread waits for.
enum Next {
    Message(String),
    New,
    Quit,
}

fn wait_for_next(control: &Control) -> Next {
    let mut inner = control.lock();
    loop {
        if inner.quit {
            return Next::Quit;
        }
        if inner.new_session {
            inner.new_session = false;
            return Next::New;
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
pub fn run(setup: Setup) -> Result<ExitCode, Error> {
    let Setup {
        function,
        function_name,
        backend,
        mut session,
        resumed,
        sessions_dir,
        context,
        server_url,
        theme,
        repl,
        lang,
        history_file,
    } = setup;
    let workspace = session.workspace.clone();
    let facts = Facts {
        function: function_name.into(),
        workspace: display_path(&workspace),
        server: server_url.into(),
        context,
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
    let dock = Arc::new(Dock::new(
        Box::new(io::stdout()),
        theme.clone(),
        lang,
        facts.function.clone(),
        facts.workspace.clone(),
        editor,
        term::size(),
    ));
    let control = Arc::new(Control::new(Info {
        session: session.id.clone(),
        ..Info::default()
    }));
    let screen: Arc<dyn Screen> = dock.clone();
    let mut presenter = Presenter::new(screen, theme.clone(), repl, lang);

    interrupt::install_request_only();
    // Only this thread gets the signal that wakes a blocked read (D131).
    term::block_sigint();
    let done = Arc::new(AtomicBool::new(false));
    let input = Input {
        dock: dock.clone(),
        control: control.clone(),
        facts: facts.clone(),
        theme: theme.clone(),
        lang,
        history_file,
        request_stop: Box::new(|| interrupt::request(true)),
        exit_now: Box::new(|| {
            term::restore();
            unsafe { libc::_exit(i32::from(interrupt::EXIT_CODE)) }
        }),
    };
    let reader = {
        let done = done.clone();
        thread::spawn(move || input_loop(input, done))
    };
    term::unblock_sigint();

    dock.start();
    presenter.show_notice(&i18n::session(&session.id, lang), Role::Muted);
    if resumed {
        presenter.replay(&session.events);
        if let Some(state) = session.unfinished().and_then(|turn| turn.state) {
            control.lock().info.unfinished = Some(state.clone());
            presenter.show_notice(&texts::unfinished(&state, lang), Role::Muted);
        }
    }

    loop {
        match wait_for_next(&control) {
            Next::Quit => break,
            Next::New => match Session::create(sessions_dir, function_name, &workspace) {
                Ok(new) => {
                    session = new;
                    let mut inner = control.lock();
                    inner.info = Info {
                        session: session.id.clone(),
                        ..Info::default()
                    };
                    drop(inner);
                    presenter.show_notice(&texts::new_session(&session.id, lang), Role::Muted);
                }
                Err(error) => presenter.show_notice(&error.message(lang), Role::StatusFailure),
            },
            Next::Message(message) => {
                dock.set_queued(control.lock().queue.len());
                presenter.begin_turn();
                presenter.show_user(&message);
                let result = Agent {
                    function,
                    function_name,
                    backend,
                    session: &mut session,
                    workspace: &workspace,
                    context,
                }
                .turn(
                    &message,
                    &mut Reporter {
                        presenter: &mut presenter,
                        control: &control,
                    },
                );
                presenter.close();
                let mut failed = None;
                match result {
                    Ok(TurnEnd::Answer(_)) => {}
                    Ok(TurnEnd::Stopped(StopReason::Interrupted)) => {}
                    Ok(TurnEnd::Stopped(reason)) => {
                        failed = Some(texts::stopped(reason, function, lang));
                    }
                    Err(error) => failed = Some(error.message(lang)),
                }
                let mut inner = control.lock();
                inner.running = false;
                inner.info.state = None;
                inner.info.unfinished = session.unfinished().and_then(|turn| turn.state);
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
                    presenter.show_notice(texts::error_cleared(lang), Role::Muted);
                }
                presenter.set_ready();
            }
        }
    }

    done.store(true, Ordering::SeqCst);
    let _ = reader.join();
    dock.finish();
    Ok(ExitCode::SUCCESS)
}

/// The session without a terminal (D128): every line of stdin is a
/// message; the answers go to stdout and the progress to stderr, as with
/// `-p` (D109).
pub fn run_lines(setup: Setup) -> Result<ExitCode, Error> {
    let Setup {
        function,
        function_name,
        backend,
        mut session,
        sessions_dir,
        context,
        server_url,
        lang,
        ..
    } = setup;
    let workspace = session.workspace.clone();
    let facts = Facts {
        function: function_name.into(),
        workspace: display_path(&workspace),
        server: server_url.into(),
        context,
    };
    interrupt::install();
    eprintln!("{}", i18n::session(&session.id, lang));
    let mut state: Option<String> = None;
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let message = line.trim();
        if message.is_empty() {
            continue;
        }
        if message.starts_with('/') {
            let name = message.split_whitespace().next().unwrap_or(message);
            match name {
                "/quit" | "/exit" => break,
                "/help" => println!("{}", help_text(lang)),
                "/status" => {
                    let info = Info {
                        session: session.id.clone(),
                        state: state.clone(),
                        unfinished: session.unfinished().and_then(|turn| turn.state),
                    };
                    println!("{}", status_text(&facts, &info, lang));
                }
                "/new" => {
                    session = Session::create(sessions_dir, function_name, &workspace)?;
                    state = None;
                    eprintln!("{}", texts::new_session(&session.id, lang));
                }
                _ => eprintln!("{}", texts::unknown_command(name, lang)),
            }
            continue;
        }
        let mut report = StderrReport::new(lang);
        let end = Agent {
            function,
            function_name,
            backend,
            session: &mut session,
            workspace: &workspace,
            context,
        }
        .turn(message, &mut report);
        state = None;
        match end {
            Ok(TurnEnd::Answer(text)) => println!("{text}"),
            Ok(TurnEnd::Stopped(StopReason::Interrupted)) => {
                eprintln!(
                    "{}",
                    texts::stopped(StopReason::Interrupted, function, lang)
                );
                return Ok(ExitCode::from(interrupt::EXIT_CODE));
            }
            Ok(TurnEnd::Stopped(reason)) => {
                eprintln!("agl-lite: {}", texts::stopped(reason, function, lang));
            }
            Err(error) => eprintln!("agl-lite: {}", error.message(lang)),
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

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
    }

    fn fixture() -> Fixture {
        let sink = Sink::default();
        let commands = COMMANDS
            .iter()
            .map(|name| MenuItem {
                name,
                description: String::new(),
            })
            .collect();
        let dock = Arc::new(Dock::new(
            Box::new(sink.clone()),
            Theme::plain(),
            Lang::En,
            "default".into(),
            "~/project".into(),
            Editor::new(Vec::new(), commands),
            (40, 20),
        ));
        let stops = Arc::new(AtomicUsize::new(0));
        let exits = Arc::new(AtomicUsize::new(0));
        let input = Input {
            dock,
            control: Arc::new(Control::new(Info {
                session: "s1".into(),
                ..Info::default()
            })),
            facts: Facts {
                function: "default".into(),
                workspace: "~/project".into(),
                server: "http://127.0.0.1:18080".into(),
                context: 16384,
            },
            theme: Theme::plain(),
            lang: Lang::En,
            history_file: std::env::temp_dir().join(format!(
                "agl-lite-repl-{}/history.jsonl",
                std::process::id()
            )),
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
        };
        Fixture {
            input,
            sink,
            stops,
            exits,
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
        assert_eq!(f.inner().queue, ["hello"]);
        assert!(f.output().contains("queued 1"));
        // Messages sent during a turn queue up in order.
        f.inner().running = true;
        f.type_text("second");
        f.input.key(Key::Enter);
        assert_eq!(f.inner().queue, ["hello", "second"]);
        // The history file holds what was sent.
        let history = editor::load_history(&f.input.history_file);
        assert_eq!(history, ["hello", "second"]);
    }

    #[test]
    fn commands_run_at_once_or_wait_for_the_turn() {
        let f = fixture();
        f.type_text("/help");
        f.input.key(Key::Enter);
        assert!(f.output().contains("/quit"));
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

        // During a turn /new and /quit are refused (D134).
        f.inner().running = true;
        for command in ["/new", "/quit", "/exit"] {
            f.type_text(command);
            f.input.key(Key::Enter);
        }
        assert!(f.output().contains("/new is not available during a turn"));
        assert!(!f.inner().quit && !f.inner().new_session);
        // /status still works and shows the state.
        f.inner().info.state = Some("verify".into());
        f.type_text("/status");
        f.input.key(Key::Enter);
        assert!(f.output().contains("state: verify"));

        f.inner().running = false;
        f.type_text("/new");
        f.input.key(Key::Enter);
        assert!(f.inner().new_session);
        f.inner().new_session = false;
        f.type_text("/exit");
        f.input.key(Key::Enter);
        assert!(f.inner().quit);
    }

    #[test]
    fn ctrl_c_during_a_turn_stops_it_and_clears_the_queue() {
        let f = fixture();
        f.inner().running = true;
        f.inner().queue.push_back("later".into());
        f.input.key(Key::Ctrl('c'));
        assert_eq!(f.stops.load(Ordering::SeqCst), 1);
        assert!(f.inner().queue.is_empty());
        assert!(f.output().contains("interrupted; queued input cleared"));
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
        assert!(!f.inner().quit);
        assert_eq!(f.stops.load(Ordering::SeqCst), 0);
        f.input.key(Key::Ctrl('c'));
        assert!(f.inner().quit);
    }

    #[test]
    fn ctrl_d_quits_only_when_idle() {
        let f = fixture();
        f.inner().running = true;
        f.input.key(Key::Ctrl('d'));
        assert!(!f.inner().quit);
        assert!(f.output().contains("Ctrl-D is not available during a turn"));
        f.inner().running = false;
        f.input.key(Key::Ctrl('d'));
        assert!(f.inner().quit);
    }

    #[test]
    fn the_main_thread_takes_what_the_user_sent() {
        let control = Control::new(Info::default());
        control.lock().queue.push_back("one".into());
        assert!(matches!(wait_for_next(&control), Next::Message(m) if m == "one"));
        assert!(control.lock().running);
        control.lock().new_session = true;
        assert!(matches!(wait_for_next(&control), Next::New));
        control.lock().quit = true;
        assert!(matches!(wait_for_next(&control), Next::Quit));
    }
}
