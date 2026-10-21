//! The interactive session (D34, D115-D140): a line editor and a dock in
//! the terminal while the turn runs, or lines on stdin without a terminal.

mod dock;
mod editor;
mod markdown;
mod transcript;
mod text;
pub(crate) mod term;

use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;

use dock::Dock;
use editor::{Editor, MenuItem, Outcome, save_history};
use term::{Key, Parser, RawGuard};
use transcript::{Presenter, Screen, notice_lines};

use crate::Setup;
use crate::agent::{Agent, Report, TurnEnd};
use crate::backend::{ReasoningLevel, ToolCall};
use crate::cli::serve::Others;
use crate::error::Error;
use crate::i18n::{Lang, m, repl as texts};
use crate::interrupt;
use crate::session::{ForgeKind, Session, StopReason};
use crate::status::{COMMANDS, Facts, Info, compact_notice, display_path, help_text, status_text};
use crate::theme::{Role, Theme};
use crate::tools::ToolResult;
use crate::tr;

#[derive(Default)]
struct Inner {
    queue: VecDeque<String>,
    /// A turn is running.
    running: bool,
    quit: bool,
    new_session: bool,
    compact: bool,
    info: Info,
    /// The model's reasoning level (AGL-013); it survives `/new`.
    reasoning: Option<ReasoningLevel>,
    /// A yes/no question to the human: `Some(None)` is asked, `Some(Some(a))`
    /// answered (AGL-015, D17).
    question: Option<Option<bool>>,
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

    fn set_state(&self, state: Option<String>) {
        self.lock().info.state = state;
    }

    /// Asks the human a yes/no question in the dock and waits for the
    /// answer (AGL-015, D17); `no` when the session ends meanwhile.
    fn ask(&self, dock: &Dock, question: &str) -> bool {
        self.lock().question = Some(None);
        dock.set_question(Some(question));
        let mut inner = self.lock();
        let answer = loop {
            if let Some(Some(answer)) = inner.question {
                break answer;
            }
            if inner.quit {
                break false;
            }
            inner = self.wake.wait(inner).unwrap_or_else(|e| e.into_inner());
        };
        inner.question = None;
        drop(inner);
        dock.set_question(None);
        answer
    }
}

impl Inner {
    /// A turn runs or messages wait for one.
    fn busy(&self) -> bool {
        self.running || !self.queue.is_empty()
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
            "/help" => self.notice(&help_text(self.lang, true), Role::Muted),
            "/status" => {
                let (info, reasoning) = {
                    let inner = self.control.lock();
                    (inner.info.clone(), inner.reasoning)
                };
                self.notice(
                    &status_text(&self.facts, &info, reasoning, self.lang),
                    Role::Muted,
                );
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
            }
            "/quit" | "/exit" | "/new" | "/compact" => {
                let mut inner = self.control.lock();
                if inner.busy() {
                    drop(inner);
                    self.notice(&tr!(self.lang, m::BUSY, name = name), Role::StatusPending);
                    return;
                }
                if name == "/new" {
                    inner.new_session = true;
                } else if name == "/compact" {
                    inner.compact = true;
                } else {
                    inner.quit = true;
                }
                drop(inner);
                self.control.wake.notify_all();
            }
            _ => self.notice(
                &tr!(self.lang, m::UNKNOWN_COMMAND, name = name),
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

    /// While a question is open the keys answer it: `y` is yes, `n`, Enter,
    /// Escape, Ctrl-C and Ctrl-D are no, the others are ignored
    /// (AGL-015, D17). Whether the key was taken.
    fn answer_question(&self, key: &Key) -> bool {
        let mut inner = self.control.lock();
        if !matches!(inner.question, Some(None)) {
            return false;
        }
        let answer = match key {
            Key::Char('y' | 'Y') => Some(true),
            Key::Char('n' | 'N') | Key::Enter | Key::Escape | Key::Ctrl('c' | 'd') => Some(false),
            _ => None,
        };
        if answer.is_some() {
            inner.question = Some(answer);
            drop(inner);
            self.control.wake.notify_all();
        }
        true
    }

    fn key(&self, key: Key) {
        if self.answer_question(&key) {
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
    fn forge(&mut self, name: &str, kind: ForgeKind, section: Option<&str>) {
        self.presenter.forge(name, kind, section);
    }
    fn compacting(&mut self) {
        self.presenter.compacting();
    }
    fn compacted(&mut self) {
        self.presenter.compacted();
    }
    fn cut_off(&mut self) {
        self.presenter.cut_off();
    }
    fn context_used(&mut self, tokens: usize) {
        self.presenter.context_used(tokens);
    }
    fn decoded(&mut self, tokens: usize) {
        self.presenter.decoded(tokens);
    }
}

/// What the main thread waits for.
enum Next {
    Message(String),
    New,
    Compact,
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
        if inner.compact {
            inner.compact = false;
            // Ctrl-C stops the compaction as it stops a turn.
            inner.running = true;
            interrupt::reset();
            return Next::Compact;
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
        forge,
        history_file,
        reasoning,
        autostart,
        image_tokens,
        ..
    } = setup;
    let workspace = session.workspace.clone();
    let facts = Facts {
        function: function_name.into(),
        workspace: display_path(&workspace),
        server: server_url.into(),
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
            session: session.id.clone(),
            context,
            ..Info::default()
        },
        reasoning,
    ));
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
    presenter.show_notice(&tr!(lang, m::SESSION, id = session.id), Role::Muted);
    if resumed {
        presenter.replay(&session.events);
        if let Some(state) = session.unfinished().and_then(|turn| turn.state) {
            control.lock().info.unfinished = Some(state.clone());
            presenter.show_notice(&tr!(lang, m::UNFINISHED, state = state), Role::Muted);
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
                        context: inner.info.context,
                        ..Info::default()
                    };
                    drop(inner);
                    presenter.show_notice(&tr!(lang, m::NEW_SESSION, id = session.id), Role::Muted);
                }
                Err(error) => presenter.show_notice(&error.message(lang), Role::StatusFailure),
            },
            Next::Compact => {
                let reasoning = control.lock().reasoning;
                let result = backend.context_size().and_then(|context| {
                    Agent {
                        function,
                        function_name,
                        backend,
                        session: &mut session,
                        workspace: &workspace,
                        context,
                        forge,
                        reasoning,
                        image_tokens,
                    }
                    .compact(&mut Reporter {
                        presenter: &mut presenter,
                        control: &control,
                    })
                });
                control.lock().running = false;
                if let Some((text, role)) = compact_notice(result, function, lang) {
                    presenter.show_notice(&text, role);
                }
                presenter.set_ready();
            }
            Next::Message(message) => {
                dock.set_queued(control.lock().queue.len());
                presenter.begin_turn();
                presenter.show_user(&message);
                // The running turn keeps the level it started with (AGL-013).
                let reasoning = control.lock().reasoning;
                // The server is started here when it does not answer, at
                // the first request, and its context size read (AGL-015).
                let ready = {
                    let ask = |units: &[String]| {
                        control.ask(&dock, &crate::i18n::server::others_running(units, lang))
                    };
                    autostart
                        .ensure(&Others::Ask(&ask), &mut |line| {
                            presenter.show_notice(line, Role::Muted)
                        })
                        .and_then(|()| backend.context_size())
                };
                let result = ready.and_then(|context| {
                    dock.set_context_size(context);
                    control.lock().info.context = Some(context);
                    Agent {
                        function,
                        function_name,
                        backend,
                        session: &mut session,
                        workspace: &workspace,
                        context,
                        forge,
                        reasoning,
                        image_tokens,
                    }
                    .turn(
                        &message,
                        &mut Reporter {
                            presenter: &mut presenter,
                            control: &control,
                        },
                    )
                });
                presenter.close();
                let mut failed = None;
                match result {
                    Ok(TurnEnd::Answer(_)) => {}
                    Ok(TurnEnd::Stopped(StopReason::Interrupted)) => {
                        presenter
                            .show_notice(tr!(lang, m::INTERRUPTED_CLEARED), Role::StatusPending);
                    }
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
                    presenter.show_notice(tr!(lang, m::ERROR_CLEARED), Role::Muted);
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

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::agent::Compaction;
    use crate::function::Function;
    use crate::settings::ReplSettings;

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
                "default".into(),
                "~/project".into(),
                Editor::new(Vec::new(), commands),
                (40, 20),
            )
            .with_context(16384),
        );
        let stops = Arc::new(AtomicUsize::new(0));
        let exits = Arc::new(AtomicUsize::new(0));
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
                function: "default".into(),
                workspace: "~/project".into(),
                server: "http://127.0.0.1:18080".into(),
            },
            theme: Theme::plain(),
            lang: Lang::En,
            history_file: std::env::temp_dir().join(format!(
                "agentLIBRE-repl-{}-{n}/history.jsonl",
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

    /// Asks `question` on another thread, as the turn thread does, and
    /// waits until it is open.
    fn ask_on_a_thread(f: &Fixture, question: &str) -> thread::JoinHandle<bool> {
        let (control, dock) = (f.input.control.clone(), f.input.dock.clone());
        let question = question.to_owned();
        let handle = thread::spawn(move || control.ask(&dock, &question));
        let started = std::time::Instant::now();
        while f.inner().question != Some(None) {
            assert!(started.elapsed().as_secs() < 5, "the question did not open");
            thread::sleep(std::time::Duration::from_millis(5));
        }
        handle
    }

    #[test]
    fn a_question_in_the_dock_is_answered_with_y_or_n() {
        let f = fixture();
        let handle = ask_on_a_thread(&f, "A server runs. Start one more? [y/N]");
        // The question is a row of the dock.
        assert!(f.output().contains("A server runs. Start one more? [y/N]"));
        // Other keys are ignored and do not reach the editor; Ctrl-C
        // does not stop a turn but answers no.
        f.type_text("x");
        f.input.key(Key::Left);
        assert!(f.input.dock.input_is_empty());
        assert_eq!(f.inner().question, Some(None));
        f.input.key(Key::Char('y'));
        assert!(handle.join().unwrap());
        assert_eq!(f.inner().question, None);
        // The question is gone from the dock, and typing works again.
        f.type_text("hi");
        assert!(!f.input.dock.input_is_empty());

        for key in [
            Key::Char('n'),
            Key::Char('N'),
            Key::Enter,
            Key::Escape,
            Key::Ctrl('c'),
            Key::Ctrl('d'),
        ] {
            let handle = ask_on_a_thread(&f, "Start one more? [y/N]");
            f.input.key(key.clone());
            assert!(!handle.join().unwrap(), "{key:?}");
        }
        // Ctrl-C at the question is not an interrupt of a turn.
        assert_eq!(f.stops.load(Ordering::SeqCst), 0);
        assert_eq!(f.exits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_question_is_no_when_the_session_ends() {
        let f = fixture();
        let handle = ask_on_a_thread(&f, "Start one more? [y/N]");
        f.inner().quit = true;
        f.input.control.wake.notify_all();
        assert!(!handle.join().unwrap());
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
        assert!(!f.inner().quit && !f.inner().new_session && !f.inner().compact);
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
        f.type_text("/compact");
        f.input.key(Key::Enter);
        assert!(f.inner().compact);
        f.type_text("/exit");
        f.input.key(Key::Enter);
        assert!(f.inner().quit);
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
    fn what_compact_tells() {
        let none = Path::new("/none");
        let function = Function::load(none, none, "default").unwrap();
        let notice = |result| compact_notice(result, &function, Lang::En);
        assert_eq!(notice(Ok(Compaction::Done)), None);
        assert_eq!(
            notice(Ok(Compaction::Nothing)),
            Some(("Nothing to compact.".into(), Role::Muted))
        );
        let (text, role) = notice(Ok(Compaction::Impossible)).unwrap();
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
        assert!(matches!(wait_for_next(&control), Next::Message(m) if m == "one"));
        assert!(control.lock().running);
        control.lock().running = false;
        control.lock().new_session = true;
        assert!(matches!(wait_for_next(&control), Next::New));
        // A compaction counts as running, so that Ctrl-C can stop it.
        control.lock().compact = true;
        assert!(matches!(wait_for_next(&control), Next::Compact));
        assert!(control.lock().running);
        control.lock().quit = true;
        assert!(matches!(wait_for_next(&control), Next::Quit));
    }

    #[derive(Default)]
    struct Lines(Arc<Mutex<Vec<String>>>);

    impl Screen for Lines {
        fn columns(&self) -> usize {
            40
        }
        fn print(&self, lines: &[String]) {
            self.0.lock().unwrap().extend_from_slice(lines);
        }
        fn live(&self, _text: &str) {}
        fn erase(&self, _count: usize) -> bool {
            true
        }
        fn activity(&self, _activity: transcript::Activity) {}
    }

    #[test]
    fn the_reporter_forwards_the_decoded_tokens() {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let screen: Arc<dyn Screen> = Arc::new(Lines(lines.clone()));
        let settings = ReplSettings {
            tool_lines: 2,
            tool_chars: 20,
            frame: true,
        };
        let mut presenter = Presenter::new(screen, Theme::plain(), settings, Lang::En);
        presenter.begin_turn();
        let control = Control::new(Info::default(), None);
        let mut reporter = Reporter {
            presenter: &mut presenter,
            control: &control,
        };
        reporter.decoded(30);
        reporter.action_text("Done.");
        reporter.answer("Done.");
        assert!(
            lines
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("tok/s"))
        );
    }
}
