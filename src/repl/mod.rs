//! The interactive session (D34, D115-D140): a line editor and a dock in
//! the terminal while the turn runs, or lines on stdin without a terminal.

mod dock;
mod editor;
mod markdown;
mod transcript;
mod text;
pub(crate) mod term;
mod voice;

use std::collections::VecDeque;
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
use transcript::{Presenter, Screen, notice_lines};

use crate::Setup;
use crate::agent::{Agent, Report, TurnEnd};
use crate::attach::{self, Pending};
use crate::backend::{ReasoningLevel, ToolCall};
use crate::error::Error;
use crate::i18n::{Lang, m, repl as texts};
use crate::interrupt;
use crate::media::{self, Clip, Kind, Source};
use crate::session::{Event, ForgeKind, Session, StopReason};
use crate::status::{
    COMMANDS, Facts, Info, Remember, compact_notice, display_path, help_text, parse_remember,
    status_text,
};
use crate::theme::{Role, Theme};
use crate::tools::ToolResult;
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
    /// A turn is running.
    running: bool,
    quit: bool,
    new_session: bool,
    compact: bool,
    /// `/remember name | text`, for the main thread to write (AGL-006).
    remember: Option<(String, String)>,
    /// `/function`: the list of the Functions, for the main thread to show
    /// (AGL-036, D9).
    functions: bool,
    /// `/function <name>`: the switch, for the main thread to check
    /// (AGL-036, D4).
    switch: Option<String>,
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
    /// What Ctrl-V reads (AGL-014, D21).
    clipboard: Box<dyn Fn() -> Clip + Send + Sync>,
    /// Ctrl-R records the voice (AGL-016).
    voice: Arc<VoiceInput>,
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
            }
            "/remember" => match parse_remember(trimmed["/remember".len()..].trim()) {
                Remember::Usage => {
                    self.notice(tr!(self.lang, m::REMEMBER_USAGE), Role::StatusPending);
                }
                // The entry is written by the main thread, which holds the
                // session: not while a turn runs (as `/new`).
                Remember::Entry { name, text } => {
                    let mut inner = self.control.lock();
                    if inner.busy() {
                        drop(inner);
                        self.notice(
                            &tr!(self.lang, m::BUSY, name = "/remember"),
                            Role::StatusPending,
                        );
                    } else {
                        inner.remember = Some((name, text));
                        drop(inner);
                        self.control.wake.notify_all();
                    }
                }
                // An ordinary message: the model names the entry (M37).
                Remember::Turn(message) => self.submitted(message),
            },
            // `/function [<name>]` (AGL-036, D3, D4, D9): the main thread
            // shows the list, or checks the Function and leaves the session
            // for a new one with it — not while a turn runs (as `/new`).
            "/function" => {
                let function = trimmed.split_whitespace().nth(1).map(str::to_owned);
                let mut inner = self.control.lock();
                if inner.busy() {
                    drop(inner);
                    self.notice(&tr!(self.lang, m::BUSY, name = name), Role::StatusPending);
                    return;
                }
                match function {
                    Some(function) => inner.switch = Some(function),
                    None => inner.functions = true,
                }
                drop(inner);
                self.control.wake.notify_all();
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
                    inner.pending.clear();
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
    fn memory(&mut self, name: &str, kind: crate::memory::Kind, text: Option<&str>) {
        self.presenter.show_memory(name, kind, text);
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
    Message(Queued),
    New,
    Compact,
    Remember(String, String),
    /// `/function` (AGL-036, D9).
    Functions,
    /// `/function <name>` (AGL-036, D4).
    Switch(String),
    Quit,
}

/// How the interactive session ends (AGL-036, D4): the human left, or a
/// `/function <name>` asks for a new session with that Function at the
/// reasoning level of this one.
pub enum End {
    Exit(ExitCode),
    Function {
        name: String,
        reasoning: Option<ReasoningLevel>,
    },
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
        if inner.functions {
            inner.functions = false;
            return Next::Functions;
        }
        if let Some(name) = inner.switch.take() {
            return Next::Switch(name);
        }
        if let Some((name, text)) = inner.remember.take() {
            return Next::Remember(name, text);
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
pub fn run(setup: Setup) -> Result<End, Error> {
    let Setup {
        function,
        function_name,
        backend,
        stirred,
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
        search,
        image_tokens,
        memory,
        catalog,
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
    control.set_media_counts(&session);
    let voice = Arc::new(VoiceInput::new(
        dock.clone(),
        Arc::new(crate::cli::whisper::Voice::of(autostart)),
        Box::new(crate::record::start),
        function.max_record_seconds,
        lang,
        theme.clone(),
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
        clipboard: Box::new(media::clipboard),
        voice: voice.clone(),
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

    let mut end = End::Exit(ExitCode::SUCCESS);
    loop {
        match wait_for_next(&control) {
            Next::Quit => break,
            Next::Functions => {
                presenter.show_notice(&catalog.lines(lang).join("\n"), Role::Muted);
            }
            // The Function must load and fit before the session is left
            // (AGL-036, D4): a refusal keeps this session as it is.
            Next::Switch(name) => match catalog.check(&name) {
                Ok(()) => {
                    presenter.show_notice(&tr!(lang, m::FUNCTION_SWITCH, name = name), Role::Muted);
                    end = End::Function {
                        name,
                        reasoning: control.lock().reasoning,
                    };
                    break;
                }
                Err(error) => presenter.show_notice(&error.message(lang), Role::StatusFailure),
            },
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
                    control.set_media_counts(&session);
                    presenter.show_notice(&tr!(lang, m::NEW_SESSION, id = session.id), Role::Muted);
                }
                Err(error) => presenter.show_notice(&error.message(lang), Role::StatusFailure),
            },
            Next::Remember(name, text) => {
                // `/remember name | text` (AGL-006, M34, M37): the program
                // writes the entry, no turn.
                match memory.write() {
                    Ok((dir, lines)) => {
                        for line in lines {
                            presenter.show_notice(&line, Role::Muted);
                        }
                        let (result, write) = crate::memory::remember(&dir, &name, &text);
                        match write {
                            Some(write) => {
                                if let Err(error) = session.append(Event::Memory {
                                    name: write.name.clone(),
                                    kind: write.kind,
                                    text: write.text.clone(),
                                }) {
                                    presenter.show_notice(&error.message(lang), Role::StatusFailure);
                                }
                                presenter.show_memory(&write.name, write.kind, write.text.as_deref());
                            }
                            None => presenter.show_notice(
                                &tr!(lang, m::REMEMBER_FAILED, reason = result.content),
                                Role::StatusFailure,
                            ),
                        }
                    }
                    Err(error) => presenter.show_notice(
                        &tr!(lang, m::REMEMBER_FAILED, reason = error.message(lang)),
                        Role::StatusFailure,
                    ),
                }
            }
            Next::Compact => {
                let reasoning = control.lock().reasoning;
                let result = backend.context_size().and_then(|context| {
                    Agent {
                        function,
                        function_name,
                        backend,
                        stirred,
                        session: &mut session,
                        workspace: &workspace,
                        context,
                        forge,
                        reasoning,
                        image_tokens,
                        whisper: autostart,
                        search,
                        memory,
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
            Next::Message(Queued {
                text: message,
                pending,
            }) => {
                dock.set_queued(control.lock().queue.len());
                presenter.begin_turn();
                presenter.show_user(&message);
                // The running turn keeps the level it started with (AGL-013).
                let reasoning = control.lock().reasoning;
                // The server is started here when it does not answer, at
                // the first request, and its context size read (AGL-015,
                // AGL-024).
                let ready = autostart
                    .ensure(&mut |line| {
                        presenter.show_notice(line, Role::Muted)
                    })
                    .and_then(|()| backend.context_size());
                let result = ready.and_then(|context| {
                    dock.set_context_size(context);
                    control.lock().info.context = Some(context);
                    // The media the message names are stored, an audio is
                    // transcribed (AGL-014).
                    attach::store(
                        &mut session,
                        pending,
                        &message,
                        backend,
                        autostart,
                        &mut |line| presenter.show_notice(line, Role::Muted),
                    )?;
                    control.set_media_counts(&session);
                    Agent {
                        function,
                        function_name,
                        backend,
                        stirred,
                        session: &mut session,
                        workspace: &workspace,
                        context,
                        forge,
                        reasoning,
                        image_tokens,
                        whisper: autostart,
                        search,
                        memory,
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
    // A recording that goes keeps nothing, and ffmpeg is stopped (AGL-016).
    voice.cancel();
    dock.finish();
    Ok(end)
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
        clip: Arc<Mutex<Clip>>,
        fake: Arc<Fake>,
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

    impl crate::cli::whisper::Dictation for FakeDictation {
        fn ready(&self) -> Result<(), Error> {
            if self.0.not_ready.load(Ordering::SeqCst) {
                return Err(Error::WhisperNotSet {
                    user_path: "/c/agentLIBRE.toml".into(),
                    project_path: "/d/agentLIBRE.toml".into(),
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
            clipboard: {
                let clip = clip.clone();
                Box::new(move || clip.lock().unwrap().clone())
            },
            voice,
        };
        Fixture {
            input,
            sink,
            stops,
            exits,
            clip,
            fake,
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
        assert!(f.inner().new_session);
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
        // The limit is the Function's (D5); the whisper unit is started
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
            f.inner().remember,
            Some(("Build and test".into(), "cargo build / cargo test".into()))
        );
        assert!(f.inner().queue.is_empty());
        // The main thread takes it before any message.
        f.inner().queue.push_back("later".into());
        assert!(matches!(
            wait_for_next(&f.input.control),
            Next::Remember(name, text) if name == "Build and test" && text == "cargo build / cargo test"
        ));
        assert!(f.inner().remember.is_none());
        // During a turn it is refused, as `/new` is, and nothing is kept.
        f.inner().running = true;
        f.type_text("/remember Name | text");
        f.input.key(Key::Enter);
        assert!(f.output().contains("/remember is not available during a turn"));
        assert!(f.inner().remember.is_none());
    }

    #[test]
    fn remember_without_a_name_is_an_ordinary_message_even_during_a_turn() {
        let f = fixture();
        f.inner().running = true;
        f.type_text("/remember cargo build / cargo test");
        f.input.key(Key::Enter);
        assert_eq!(f.inner().texts(), ["remember: cargo build / cargo test"]);
        assert!(f.inner().remember.is_none());
    }

    #[test]
    fn remember_alone_or_with_an_empty_name_or_text_tells_the_usage() {
        let f = fixture();
        for line in ["/remember", "/remember | text", "/remember name |"] {
            f.type_text(line);
            f.input.key(Key::Enter);
        }
        assert!(f.output().contains("usage: /remember"));
        assert!(f.inner().remember.is_none());
        assert!(f.inner().queue.is_empty());
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
    fn function_asks_the_main_thread_for_the_list_or_the_switch() {
        // AGL-036, D4, D9: the main thread shows the list or checks the
        // Function; not during a turn.
        let f = fixture();
        f.inner().running = true;
        f.type_text("/function other");
        f.input.key(Key::Enter);
        assert!(f.output().contains("/function is not available during a turn"));
        assert!(f.inner().switch.is_none());
        f.inner().running = false;
        f.type_text("/function");
        f.input.key(Key::Enter);
        assert!(f.inner().functions);
        f.inner().functions = false;
        f.type_text("/function other");
        f.input.key(Key::Enter);
        assert_eq!(f.inner().switch.as_deref(), Some("other"));
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
        let function = Function::load(none, none, none, "minimal").unwrap();
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
        assert!(matches!(wait_for_next(&control), Next::Message(m) if m.text == "one"));
        assert!(control.lock().running);
        control.lock().running = false;
        control.lock().new_session = true;
        assert!(matches!(wait_for_next(&control), Next::New));
        // `/function` (AGL-036, D4, D9).
        control.lock().functions = true;
        assert!(matches!(wait_for_next(&control), Next::Functions));
        control.lock().switch = Some("other".into());
        assert!(matches!(wait_for_next(&control), Next::Switch(name) if name == "other"));
        assert!(!control.lock().running);
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
