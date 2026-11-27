//! `agl-chat` (AGL-073): agentLIBRE's chat, a window drawn by katarsa and a
//! client of `agl daemon` (D5), with the REPL's features (AGL-128). It opens a
//! session in the current directory as `agl` does, the directory's sessions at
//! its left; it draws the records the REPL draws, as they come; its field takes
//! the REPL's commands, offered by a menu, and its keys; a plane's yes or no
//! question has two buttons (D11).

use std::cell::RefCell;
use std::collections::HashMap;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;

mod chat;
mod look;
mod slash;
mod voice;

use agentlibre_client::{Client, Start};
use agentlibre_common::i18n::{Lang, m};
use agentlibre_common::media::{Pending, Source};
use agentlibre_common::reasoning::ReasoningLevel;
use agentlibre_common::paths::Paths;
use agentlibre_common::settings::{Detail, ReplSettings, UserFile};
use agentlibre_common::tr;
use agentlibre_common::style::Role;
use agentlibre_proto::{self as proto, Ask, Phase, Reply, Report, Request, Sent};
use agentlibre_surface::records::{self, Record};
use agentlibre_surface::{attach, words};
use chat::Chat;
use katarsa_app::App;
use katarsa_reactive::signal;
use katarsa_ui::reactive::{Cx, Pasted, Ui, prop};
use katarsa_ui::{Key, KeyInput};
use katarsa_ui::retained::{Sty, Tone};
use katarsa_wayland::WindowConfig;


const HELP_EN: &str = "\
Usage: agl-chat [--resume [<id|name|last>]] [--plane <name>] [--function <name>]
                [--think <level>] [--yes]

agentLIBRE's chat: a window on a session of agl daemon, in the current
directory, as agl opens one; the sessions of the directory are at its left.
The daemon is started when none runs.

Keys of the field: Enter sends, Shift+Enter breaks a line; / opens the menu
of the commands, the arrows pick one, Enter takes it; Up at the first line
recalls the messages sent; Ctrl-R records the microphone and sends the text,
Alt-R puts it in the line; Ctrl-V attaches a picture or files; Escape stops
the turn. The row under the field says the Function, the context used and
its size, the phase and its time, and the speed; its button goes round the
levels of detail: off, flexible, full.

  --resume [<id|name|last>]  go on with a session; with no id, list them
  --plane <name>             the session in that plane
  --function <name>          a new session with that Function
  --think <level>            the reasoning level: off, low, medium or xhigh
  --yes                      agree to restart the server without asking
  -h, --help                 this help";

const HELP_UK: &str = "\
Використання: agl-chat [--resume [<id|назва|last>]] [--plane <назва>] [--function <назва>]
                       [--think <рівень>] [--yes]

Чат agentLIBRE: вікно на сесію agl daemon у поточній теці, як її відкриває
agl; сесії теки зліва. Демон запускається, коли жоден не працює.

Клавіші поля: Enter надсилає, Shift+Enter переносить рядок; / відкриває меню
команд, стрілки вибирають, Enter бере; Up на першому рядку повертає надіслані
повідомлення; Ctrl-R записує мікрофон і надсилає текст, Alt-R ставить його в
рядок; Ctrl-V прикріплює картинку чи файли; Escape зупиняє хід. Рядок під
полем каже Function, використаний контекст і його розмір, фазу з її часом та
швидкість; його кнопка ходить рівнями деталей: off, flexible, full.

  --resume [<id|назва|last>]  продовжити сесію; без id — список
  --plane <назва>             сесія в цьому плані
  --function <назва>          нова сесія з цією Function
  --think <рівень>            рівень міркувань: off, low, medium або xhigh
  --yes                       погодитися на перезапуск сервера без питання
  -h, --help                  ця довідка";

/// What the command line asks: the REPL's launch flags (AGL-128, D2).
#[derive(Debug, Default, PartialEq)]
struct Args {
    resume: Option<String>,
    /// `--resume` with no id: the latest sessions are listed.
    list: bool,
    plane: Option<String>,
    function: Option<String>,
    /// `--think <level>`: the reasoning level of the messages.
    think: Option<String>,
    /// `--yes`: a restart of the server is agreed without asking.
    yes: bool,
}

/// The arguments; `None` for the help.
fn parse(args: &[String]) -> Result<Option<Args>, String> {
    let mut parsed = Args::default();
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        // `--resume` with nothing after it, or another option, lists.
        let bare = args.peek().is_none_or(|next| next.starts_with("--"));
        let mut value = |name: &str| {
            args.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            // With no id, the latest are listed, as `agl --resume` does.
            "--resume" if bare => parsed.list = true,
            "--resume" => parsed.resume = Some(value(arg)?),
            "--plane" => parsed.plane = Some(value(arg)?),
            "--function" => parsed.function = Some(value(arg)?),
            "--think" => {
                let level = value(arg)?;
                if ReasoningLevel::parse(&level).is_none() {
                    return Err(format!("--think takes off, low, medium or xhigh, not {level:?}"));
                }
                parsed.think = Some(level);
            }
            "--yes" => parsed.yes = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Some(parsed))
}

/// The language of the environment: Ukrainian or English.
fn lang() -> Lang {
    let ukrainian = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.is_empty())
        .is_some_and(|value| value.starts_with("uk"));
    if ukrainian { Lang::Uk } else { Lang::En }
}

/// What a reply of the daemon does.
type OnReply = Box<dyn FnMut(Reply)>;

/// Ends the ticks of a dictation's draft.
fn stop_ticking(ticker: Signal<Option<std::sync::Arc<std::sync::atomic::AtomicBool>>>) {
    if let Some(ticking) = ticker.get() {
        ticking.store(false, std::sync::atomic::Ordering::Relaxed);
    }
    ticker.set(None);
}

/// What a tick of a dictation's draft does.
type OnTick = Box<dyn FnMut()>;

/// What the text of a dictation does, once the daemon gave it.
type OnVoice = Box<dyn FnMut(Result<String, String>, voice::Mode)>;

thread_local! {
    /// What a reply of the daemon does, on the loop's thread.
    static ON_REPLY: RefCell<Option<OnReply>> = const { RefCell::new(None) };
    /// What a dictation's text does, on the loop's thread.
    static ON_VOICE: RefCell<Option<OnVoice>> = const { RefCell::new(None) };
    /// What a tick of a dictation's draft does, on the loop's thread.
    static ON_TICK: RefCell<Option<OnTick>> = const { RefCell::new(None) };
    /// What a second of the row's clock does, on the loop's thread.
    static ON_SECOND: RefCell<Option<OnTick>> = const { RefCell::new(None) };
}

/// Sends `request` to the daemon; a daemon gone is told by the reader.
fn send(client: &Client, request: &Request) {
    let _ = client.send(request);
}

fn main() -> ExitCode {
    let lang = lang();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse(&args) {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!("{}", if lang == Lang::Uk { HELP_UK } else { HELP_EN });
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("agl-chat: {error}");
            return ExitCode::FAILURE;
        }
    };
    match run(args, lang) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("agl-chat: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args, lang: Lang) -> Result<bool, Box<dyn std::error::Error>> {
    let dir = std::env::current_dir()?;
    // The daemon's socket and its start are the clients' crate's (AGL-121, D29).
    let socket = agentlibre_client::socket_of_env(|name| std::env::var_os(name))
        .ok_or("XDG_RUNTIME_DIR is not set, so the daemon has no place for its socket")?;
    let words = if lang == Lang::Uk { "uk" } else { "en" };
    let out = Arc::new(Client::connect(&socket, true, words, Start::OnPath)?);
    // `--resume` with no id: the latest sessions, as `agl --resume` lists
    // them, and no window.
    if args.list {
        let request = match &args.plane {
            Some(plane) => Request::Past { dir: None, plane: Some(plane.clone()), limit: PAST },
            None => Request::Past { dir: Some(dir.to_string_lossy().into_owned()), plane: None, limit: PAST },
        };
        send(&out, &request);
        loop {
            match out.next()? {
                Reply::Past { sessions } => {
                    if sessions.is_empty() {
                        println!("{}", tr!(lang, m::NO_SESSIONS));
                    }
                    for session in sessions {
                        println!("{}  {}  {}", session.id, session.time, session.name.unwrap_or(session.first));
                    }
                    return Ok(true);
                }
                Reply::Refused { error } => return Err(error.into()),
                _ => {}
            }
        }
    }
    let chat = signal_chat(lang);
    let session = signal(None::<String>);
    let busy = signal(false);
    let question = signal(None::<(u64, String)>);
    let prompt = signal(String::new());
    let past = signal(Vec::<proto::PastSession>::new());
    let sidebar = signal(true);
    let place = Place {
        dir: dir.to_string_lossy().into_owned(),
        plane: signal(args.plane.clone()),
    };
    let facts = signal(None::<words::Facts>);
    let info = signal(words::Info::default());
    let reasoning = signal(None::<ReasoningLevel>);
    let pending = signal(Vec::<Pending>::new());
    let counts = signal([0u32; 3]);
    // Whether the daemon has a draft server for the dictation (AGL-127, D4).
    let drafts = signal(false);
    let confirm = signal(None::<String>);
    let switching = signal(None::<slash::Switch>);
    // `/function new`: the name and the Function it copies, until the
    // daemon answers.
    let making = signal(None::<(String, String)>);
    // The blocks the human opened or closed by hand (AGL-139, D5): a
    // session's own, forgotten with its past.
    let clicks = signal(HashMap::<(usize, char), bool>::new());

    // The user's settings, as the REPL reads them: the thinking, the longest
    // recording, the colors (D4, D8). A file that does not load is said, and
    // the defaults hold.
    let user = Paths::from_env(|name| std::env::var_os(name))
        .map_err(|error| error.message(lang))
        .and_then(|paths| UserFile::load(&paths.settings_file()).map_err(|error| error.message(lang)));
    let user = match user {
        Ok(user) => user,
        Err(error) => {
            chat.update(|chat| chat.note(&error, Role::StatusFailure));
            UserFile::default()
        }
    };
    let repl = ReplSettings::of(&user);

    let mut app = App::new("agl-chat")?;
    let quit = app.quitter();
    let handle = app.handle();
    {
        let out = Arc::clone(&out);
        let place = place.clone();
        let on_reply = move |reply: Reply| {
            let ours = |of: &str| session.with(|session| session.as_deref() == Some(of));
            match reply {
                Reply::Opened(opened) => {
                    facts.set(Some(words::Facts {
                        function: opened.function.clone(),
                        workspace: opened.workspace.clone(),
                        plane: opened.session.plane.clone(),
                        server: opened.server_url.clone(),
                    }));
                    info.set(words::Info {
                        session: opened.session.id.clone(),
                        unfinished: opened.session.unfinished.clone(),
                        context: opened.context,
                        ..words::Info::default()
                    });
                    if let Some(level) = opened.reasoning.as_deref().and_then(ReasoningLevel::parse) {
                        reasoning.set(Some(level));
                    }
                    counts.set(opened.session.media);
                    drafts.set(opened.drafts);
                    chat.update(|chat| chat.opened(&opened));
                    pending.set(Vec::new());
                    let id = opened.session.id;
                    session.set(Some(id.clone()));
                    send(&out, &Request::History { session: id });
                    send(&out, &place.past());
                }
                Reply::Past { sessions } => past.set(sessions),
                // `/function` with no name (AGL-036, D3).
                Reply::Functions { functions } => {
                    let lines = records::function_lines(&functions, lang).join("\n");
                    chat.update(|chat| chat.note(&lines, Role::Muted));
                    record_command(&out, session, "/function", false, Some(lines));
                }
                // `/plane`'s list, or the check of `/plane <name>` (AGL-077, D12).
                Reply::Planes { planes } => match switching.get() {
                    Some(slash::Switch::Plane(name)) => {
                        switching.set(None);
                        let line = tr!(lang, m::PLANE_SWITCH, name = name);
                        chat.update(|chat| chat.note(&line, Role::Muted));
                        record_command(&out, session, &format!("/plane {name}"), false, None);
                        place.plane.set(Some(name));
                        reopen(&out, &place, session, chat, None, None);
                    }
                    _ => {
                        let none = "\u{2014}".to_owned();
                        let text = tr!(
                            lang,
                            m::PLANE_HERE,
                            plane = place.plane.get().unwrap_or_else(|| none.clone()),
                            planes = if planes.is_empty() { none.clone() } else { planes.join(", ") }
                        );
                        chat.update(|chat| chat.note(&text, Role::Muted));
                        record_command(&out, session, "/plane", false, Some(text));
                    }
                },
                // `/function show`: its source and its file (AGL-102, D2).
                Reply::Function(view) if view.text.is_some() => {
                    let text = format!("{}\n{}", view.text.unwrap_or_default(), view.path);
                    chat.update(|chat| chat.note(&text, Role::Muted));
                    record_command(&out, session, &format!("/function show {}", view.name), false, Some(text));
                }
                // `/function new`: the copy and where it is (AGL-102, D3).
                Reply::Function(view) if view.load.is_none() && making.with(Option::is_some) => {
                    let (name, source) = making.get().unwrap_or_default();
                    making.set(None);
                    let text = tr!(lang, m::FUNCTION_NEW, name = name, source = source, path = view.path);
                    chat.update(|chat| chat.note(&text, Role::Muted));
                    record_command(&out, session, &format!("/function new {name} {source}"), false, Some(text));
                }
                // The check of `/function <name>` (AGL-036, D4).
                Reply::Function(_) => {
                    if let Some(slash::Switch::Function(name)) = switching.get() {
                        switching.set(None);
                        let line = tr!(lang, m::FUNCTION_SWITCH, name = name);
                        chat.update(|chat| chat.note(&line, Role::Muted));
                        record_command(&out, session, &format!("/function {name}"), false, None);
                        reopen(&out, &place, session, chat, None, Some(name));
                    }
                }
                Reply::History {
                    events, transitions, ..
                } => {
                    clicks.set(HashMap::new());
                    chat.update(|chat| chat.history(&events, &transitions));
                }
                Reply::Sent { session: of, sent } if ours(&of) => {
                    if let Sent::Report(Report::Transition(transition)) = &sent {
                        busy.set(transition.to != Phase::Ready);
                    }
                    chat.update(|chat| chat.sent(&sent));
                }
                Reply::Done {
                    session: of, done, ..
                } if ours(&of) => {
                    // A session's first message or a rename shows in the list.
                    send(&out, &place.past());
                    counts.set(done.session.media);
                    // `/new` gives the session the client goes on with.
                    if !done.session.id.is_empty() && !ours(&done.session.id) {
                        session.set(Some(done.session.id.clone()));
                        info.update(|info| info.session = done.session.id.clone());
                        clicks.set(HashMap::new());
                        chat.update(Chat::fresh);
                    }
                    if done.turn.is_some() {
                        busy.set(false);
                    }
                    chat.update(|chat| chat.done(&done));
                }
                Reply::Question {
                    question: id,
                    ask: Ask::Yes,
                    text,
                    ..
                } => question.set(Some((id, text))),
                // A folder of weights is asked in the terminal (D11).
                Reply::Question { question: id, .. } => {
                    send(
                        &out,
                        &Request::Answer {
                            question: id,
                            answer: None,
                        },
                    );
                    chat.update(|chat| {
                        chat.note(
                            "a question for a line was refused: answer it with agl in a terminal",
                            Role::StatusFailure,
                        );
                    });
                }
                Reply::Refused { error } => {
                    busy.set(false);
                    switching.set(None);
                    making.set(None);
                    chat.update(|chat| chat.note(&error, Role::StatusFailure));
                }
                _ => {}
            }
        };
        ON_REPLY.with(|slot| *slot.borrow_mut() = Some(Box::new(on_reply)));
    }
    // The daemon's lines, read apart and handed to the loop.
    let reader = handle.clone();
    let client = Arc::clone(&out);
    std::thread::spawn(move || {
        let post = |reply: Reply| {
            reader.post(move || {
                ON_REPLY.with(|slot| {
                    if let Some(on_reply) = slot.borrow_mut().as_mut() {
                        on_reply(reply);
                    }
                });
            })
        };
        while let Ok(reply) = client.next() {
            if !post(reply) {
                return;
            }
        }
        post(Reply::Refused {
            error: "the daemon went".into(),
        });
    });
    // `--think` holds for the messages; `--yes` agrees to a restart.
    if let Some(level) = args.think.as_deref().and_then(ReasoningLevel::parse) {
        reasoning.set(Some(level));
    }
    let mut first = place.open(args.resume, args.function);
    if let Request::Open(open) = &mut first {
        open.reasoning = args.think.clone();
        open.yes = args.yes;
        open.start_server = repl.start_server;
    }
    send(&out, &first);

    let title = dir.file_name().map_or_else(
        || dir.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    app.window(
        WindowConfig {
            title: format!("agl-chat · {title}"),
            app_id: "agl-chat".into(),
            size: [900, 720],
            min_size: Some([420, 320]),
        },
        move |_| {
            build(
                &out,
                State {
                    chat,
                    session,
                    busy,
                    question,
                    prompt,
                    past,
                    sidebar,
                    place: place.clone(),
                    lang,
                    facts,
                    info,
                    reasoning,
                    pending,
                    counts,
                    confirm,
                    switching,
                    making,
                    clicks,
                    drafts,
                    quit: quit.clone(),
                    repl,
                    handle: handle.clone(),
                    colors: user.colors.clone(),
                },
            )
        },
    );
    Ok(app.run()?.clean)
}

/// Where the chat opens its sessions: the directory, and the plane of
/// `--plane` (AGL-128, D6).
#[derive(Clone)]
struct Place {
    dir: String,
    /// The plane of the session; `/plane <name>` moves it (AGL-077, D12).
    plane: Signal<Option<String>>,
}

/// How many past sessions the list shows, as `agl --resume` lists them.
const PAST: usize = 20;

impl Place {
    /// The request of the sessions of the directory's project, or of its
    /// plane (AGL-126, D9; AGL-128, D6).
    fn past(&self) -> Request {
        match self.plane.get() {
            Some(plane) => Request::Past { dir: None, plane: Some(plane), limit: PAST },
            None => Request::Past { dir: Some(self.dir.clone()), plane: None, limit: PAST },
        }
    }

    /// The request that opens the session `resume`, or a new one, with
    /// `function`.
    fn open(&self, resume: Option<String>, function: Option<String>) -> Request {
        Request::Open(proto::Open {
            dir: self.dir.clone(),
            plane: self.plane.get(),
            resume,
            function,
            ..proto::Open::default()
        })
    }
}

/// Leaves the session for `resume`, or a new one with `function`: this one
/// closed, the chat emptied, the other's past coming with its `opened`.
fn reopen(
    out: &Client,
    place: &Place,
    session: Signal<Option<String>>,
    chat: Signal<Chat>,
    resume: Option<String>,
    function: Option<String>,
) {
    if let Some(id) = session.get() {
        send(out, &Request::Close { session: id });
    }
    session.set(None);
    chat.update(|chat| chat.history(&[], &[]));
    send(out, &place.open(resume, function));
}

/// A command's line in the session's record (AGL-047, D5).
fn record_command(out: &Client, session: Signal<Option<String>>, line: &str, failed: bool, output: Option<String>) {
    if let Some(id) = session.get() {
        send(out, &slash::record(&id, line, failed, output));
    }
}

/// The signals of the window.
#[derive(Clone)]
struct State {
    chat: Signal<Chat>,
    session: Signal<Option<String>>,
    busy: Signal<bool>,
    question: Signal<Option<(u64, String)>>,
    prompt: Signal<String>,
    /// The sessions at the left (D1, D6).
    past: Signal<Vec<proto::PastSession>>,
    /// Whether the list of sessions shows (D1).
    sidebar: Signal<bool>,
    place: Place,
    lang: Lang,
    /// What `/status` says (D134).
    facts: Signal<Option<words::Facts>>,
    info: Signal<words::Info>,
    /// The reasoning level of the next messages (`/think`).
    reasoning: Signal<Option<ReasoningLevel>>,
    /// The media waiting for the next message (AGL-014, D18).
    pending: Signal<Vec<Pending>>,
    /// The media the session holds, of each kind.
    counts: Signal<[u32; 3]>,
    /// `/memory remove <name>`, asked first.
    confirm: Signal<Option<String>>,
    /// A `/function` or `/plane` switch being checked.
    switching: Signal<Option<slash::Switch>>,
    /// A `/function new` being made.
    making: Signal<Option<(String, String)>>,
    /// The blocks the human opened or closed by hand (AGL-139, D5).
    clicks: Signal<HashMap<(usize, char), bool>>,
    /// Whether the daemon drafts a dictation.
    drafts: Signal<bool>,
    quit: katarsa_app::Quit,
    /// The user's `repl` settings (D4).
    repl: ReplSettings,
    /// What brings a dictation's text back to the loop.
    handle: katarsa_app::Handle,
    /// The user's `colors` (D8).
    colors: Vec<(Role, agentlibre_common::style::Style)>,
}

/// What the line over the field says while the microphone records, in
/// `mode`: Ctrl-R ends it, Alt-R flips what its text does (AGL-123, D11).
fn recording_line(mode: voice::Mode, lang: Lang) -> String {
    match (lang, mode) {
        (Lang::Uk, voice::Mode::Send) => "запис… Ctrl-R завершить і надішле; Alt-R — у рядок".into(),
        (Lang::Uk, voice::Mode::Ask) => "запис… Ctrl-R завершить, текст піде в рядок; Alt-R — надіслати".into(),
        (_, voice::Mode::Send) => "recording… Ctrl-R ends and sends; Alt-R: into the line".into(),
        (_, voice::Mode::Ask) => "recording… Ctrl-R ends, the text goes into the line; Alt-R: send".into(),
    }
}

/// What it says while the daemon transcribes.
fn transcribing_line(lang: Lang) -> String {
    match lang {
        Lang::Uk => "розпізнаю…".into(),
        _ => "transcribing…".into(),
    }
}

/// The chat, as a signal.
fn signal_chat(lang: Lang) -> Signal<Chat> {
    signal(Chat::new(lang))
}

type Signal<T> = katarsa_reactive::Signal<T>;

/// The record `at`, read from the chat.
fn record_at<T>(chat: Signal<Chat>, at: usize, read: impl Fn(&Record) -> T) -> Option<T> {
    chat.with(|chat| chat.records().get(at).map(&read))
}

/// The window's UI.
fn build(out: &Arc<Client>, state: State) -> Ui {
    let State {
        chat,
        session,
        busy,
        question,
        prompt,
        past,
        sidebar,
        place,
        lang,
        facts,
        info,
        reasoning,
        pending,
        counts,
        confirm,
        switching,
        making,
        clicks,
        drafts,
        quit,
        repl,
        handle,
        colors,
    } = state;
    let ui = Ui::new("chat");
    ui.styles(look::styles(&colors)).expect("the chat's styles work out");
    ui.set_root_style(Sty::row().gap(14.0).pad(14.0));
    // Another session of the list: this one closed, that one opened; its
    // past comes with its `opened`.
    let switch = {
        let out = Arc::clone(out);
        let place = place.clone();
        move |resume: Option<String>| {
            if !busy.get() {
                reopen(&out, &place, session, chat, resume, None);
            }
        }
    };
    let stop = {
        let out = Arc::clone(out);
        move || {
            if busy.get()
                && let Some(id) = session.get()
            {
                send(&out, &Request::Stop { session: id });
            }
        }
    };
    // Escape stops the turn.
    {
        let stop = stop.clone();
        ui.on_cancel(stop);
    }
    let answer = {
        let out = Arc::clone(out);
        move |yes: bool| {
            if let Some((id, _)) = question.get() {
                let answer = Some(if yes { "yes" } else { "no" }.to_owned());
                send(
                    &out,
                    &Request::Answer {
                        question: id,
                        answer,
                    },
                );
                question.set(None);
            }
        }
    };
    // The messages sent from this window, for Up at the field's first line
    // (D2), and the one recalled.
    let sent = signal(Vec::<String>::new());
    let recalled = signal(None::<usize>);
    // What the microphone is doing, while it does; the field as it was when
    // the dictation began; what keeps the draft's ticks going.
    let heard = signal(None::<String>);
    let before = signal(String::new());
    let ticker = signal(None::<std::sync::Arc<std::sync::atomic::AtomicBool>>);
    // A file attached to the next message: its placeholder goes into the
    // field, numbered after the session's media (AGL-014, D18).
    let attach_file = move |path: std::path::PathBuf| match Source::from_file(&path) {
        Ok(source) => {
            let name = source.name.clone();
            let mut placeholder = String::new();
            pending.update(|pending| placeholder = attach::add_counted(&counts.get(), pending, source));
            let line = tr!(lang, m::ATTACHED, placeholder = placeholder, name = name);
            chat.update(|chat| chat.note(&line, Role::Muted));
            prompt.update(|prompt| {
                prompt.push_str(&placeholder);
                prompt.push(' ');
            });
        }
        Err(error) => chat.update(|chat| chat.note(&error.message(lang), Role::StatusFailure)),
    };
    // A `/` line: what the REPL does with it (D5).
    let command = {
        let out = Arc::clone(out);
        let place = place.clone();
        move |line: &str| {
            let Some(id) = session.get() else { return };
            let (dir, plane) = (place.dir.clone(), place.plane.get());
            let facts = facts.get();
            let info = info.get();
            let here = slash::Here {
                lang,
                session: &id,
                dir: &dir,
                plane: plane.as_deref(),
                busy: busy.get(),
                reasoning: reasoning.get(),
                facts: facts.as_ref(),
                info: &info,
            };
            match slash::slash(line, &here) {
                slash::Slash::Say { text, failed, line } => {
                    let role = if failed { Role::StatusFailure } else { Role::Muted };
                    chat.update(|chat| chat.note(&text, role));
                    send(&out, &slash::record(&id, &line, failed, Some(text)));
                }
                slash::Slash::Ask(request) => send(&out, &request),
                slash::Slash::Think(level) => {
                    reasoning.set(Some(level));
                    let shown = words::reasoning_level(Some(level), lang);
                    let text = tr!(lang, m::THINK_LEVEL, level = shown);
                    chat.update(|chat| chat.note(&text, Role::Muted));
                }
                slash::Slash::Attach(path) => attach_file(path),
                slash::Slash::Confirm { name } => confirm.set(Some(name)),
                slash::Slash::List(request) => send(&out, &request),
                slash::Slash::Switch(switch) => {
                    let request = slash::check(&switch, &dir, plane.as_deref());
                    switching.set(Some(switch));
                    send(&out, &request);
                }
                slash::Slash::Function { request, made } => {
                    making.set(made);
                    send(&out, &request);
                }
                slash::Slash::New => send(&out, &Request::New { session: id }),
                slash::Slash::Quit => quit.quit(),
            }
        }
    };
    let submit = {
        let out = Arc::clone(out);
        let command = command.clone();
        move |text: String| {
            let text = text.trim().to_owned();
            let Some(id) = session.get() else { return };
            if text.is_empty() {
                return;
            }
            prompt.set(String::new());
            if text.starts_with('/') {
                command(&text);
                return;
            }
            // Every pending medium goes; one whose placeholder the human
            // deleted is dropped by the daemon (AGL-014, D21).
            let attached = pending.with(|pending| pending.iter().map(attach::attached).collect());
            pending.set(Vec::new());
            send(
                &out,
                &Request::Say {
                    session: id,
                    message: text.clone(),
                    attached,
                    reasoning: reasoning.get().map(|level| level.name().to_owned()),
                },
            );
            chat.update(|chat| chat.said(&text));
            sent.update(|sent| sent.push(text.clone()));
            recalled.set(None);
            busy.set(true);
        }
    };
    // A dictation's text, once the daemon gave it: sent, or put in the
    // field (AGL-123).
    {
        let submit = submit.clone();
        let on_voice = move |text: Result<String, String>, mode: voice::Mode| {
            heard.set(None);
            // The field holds what was typed before the dictation began.
            let base = before.get();
            match (text, mode) {
                (Ok(text), voice::Mode::Send) if !text.trim().is_empty() => {
                    submit(text);
                    prompt.set(base);
                }
                (Ok(text), _) => prompt.set(format!("{base}{}", text.trim())),
                (Err(error), _) => {
                    prompt.set(base);
                    chat.update(|chat| chat.note(&error, Role::StatusFailure));
                }
            }
        };
        ON_VOICE.with(|slot| *slot.borrow_mut() = Some(Box::new(on_voice)));
    }
    let microphone = voice::Voice::default();
    // The draft grows in the field while the human speaks (AGL-122): the
    // loop looks at the recording's chunks ten times a second.
    {
        let microphone = microphone.clone();
        let on_tick = move || {
            if let Some(draft) = microphone.draft() {
                prompt.set(format!("{}{draft}", before.get()));
            }
        };
        ON_TICK.with(|slot| *slot.borrow_mut() = Some(Box::new(on_tick)));
    }
    // Ctrl-R or Alt-R (D2): a press records; over a recording Alt-R flips
    // what its text does and Ctrl-R ends it, the daemon's whisper making the
    // text (AGL-123, D11).
    let press = {
        let microphone = microphone.clone();
        let handle = handle.clone();
        move |alt: bool| {
            let draft = (drafts.get() && !microphone.recording())
                .then(|| session.get())
                .flatten()
                .map(|id| (id, lang));
            match microphone.press(alt, repl.max_record_seconds, draft, lang) {
            voice::Press::Started(mode) => {
                before.set(prompt.get());
                heard.set(Some(recording_line(mode, lang)));
                let handle = handle.clone();
                let ticking = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
                ticker.set(Some(std::sync::Arc::clone(&ticking)));
                std::thread::spawn(move || {
                    while ticking.load(std::sync::atomic::Ordering::Relaxed) {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        let posted = handle.post(|| {
                            ON_TICK.with(|slot| {
                                if let Some(tick) = slot.borrow_mut().as_mut() {
                                    tick();
                                }
                            });
                        });
                        if !posted {
                            return;
                        }
                    }
                });
            }
            voice::Press::Flipped(mode) => heard.set(Some(recording_line(mode, lang))),
            voice::Press::Failed(error) => {
                stop_ticking(ticker);
                heard.set(None);
                chat.update(|chat| chat.note(&error, Role::StatusFailure));
            }
            voice::Press::Stopped(wav, mode) => {
                stop_ticking(ticker);
                // The draft gives way to the whole recording's text.
                prompt.set(before.get());
                let Some(id) = session.get() else {
                    heard.set(None);
                    return;
                };
                heard.set(Some(transcribing_line(lang)));
                let handle = handle.clone();
                std::thread::spawn(move || {
                    let text = voice::transcribe(&wav, id, lang);
                    handle.post(move || {
                        ON_VOICE.with(|slot| {
                            if let Some(on_voice) = slot.borrow_mut().as_mut() {
                                on_voice(text, mode);
                            }
                        });
                    });
                });
            }
            }
        }
    };
    // `/memory remove <name>`, once agreed.
    let remove = {
        let out = Arc::clone(out);
        move |yes: bool| {
            if let (Some(name), Some(id), true) = (confirm.get(), session.get(), yes) {
                send(&out, &Request::Remove { session: id, name });
            }
            confirm.set(None);
        }
    };
    // The `/` menu over the field (D5; KS-050): open while the field holds
    // a `/` and a name being typed, narrowed to the names it starts.
    let menu_open = signal(false);
    let menu_pick = signal(0usize);
    let menu = slash::menu(lang);
    let shown = {
        let menu = menu.clone();
        katarsa_reactive::memo(move || {
            let typed = prompt.get();
            menu.iter()
                .enumerate()
                .filter(|(_, (name, _))| name.starts_with(typed.trim_end()))
                .map(|(at, _)| at)
                .collect::<Vec<_>>()
        })
    };
    // The level of detail the whole chat shows (AGL-139, D5): it starts at
    // `repl.thinking` and the button under the field goes round the three;
    // what the human opens by hand goes against it, and a new level forgets
    // those clicks.
    let level = signal(repl.thinking);
    // The phase's time counts on while the phase lasts (AGL-139, D1), as
    // the REPL's dock ticks: a second of the clock redraws the row, when a
    // phase runs.
    let clock = signal(0u64);
    ON_SECOND.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            if chat.with(|chat| chat.dock().phase != Phase::Ready) {
                clock.update(|second| *second += 1);
            }
        }));
    });
    {
        let handle = handle.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let posted = handle.post(|| {
                    ON_SECOND.with(|slot| {
                        if let Some(second) = slot.borrow_mut().as_mut() {
                            second();
                        }
                    });
                });
                if !posted {
                    return;
                }
            }
        });
    }
    // What the copy buttons copy through (KS-041 D3).
    let copier = ui.clone();
    ui.mount(move |cx| {
        // The sessions of the directory's project or plane, at the left (D1,
        // D6): a click opens one.
        let switch = switch.clone();
        cx.show(Sty::column().width(240.0).gap(4.0), move || sidebar.get(), move |cx| {
            let switch = switch.clone();
            let new = switch.clone();
            cx.button("new-session", "+ new", move || new(None));
            let rows = move || past.with(|past| past.iter().map(|session| session.id.clone()).collect::<Vec<_>>());
            let switch = switch.clone();
            cx.keyed(Sty::column().gap(2.0), rows, |id| id.clone(), move |cx, id| {
                let id = id.clone();
                let label = {
                    let id = id.clone();
                    prop(move || {
                        past.with(|past| {
                            past.iter()
                                .find(|session| session.id == id)
                                // A session with no name and no message yet: its time.
                                .map(|session| {
                                    session
                                        .name
                                        .clone()
                                        .filter(|name| !name.trim().is_empty())
                                        .or_else(|| Some(session.first.clone()).filter(|first| !first.trim().is_empty()))
                                        .unwrap_or_else(|| session.time.replace('T', " ").trim_end_matches('Z').to_owned())
                                })
                                .unwrap_or_default()
                        })
                    })
                };
                let current = {
                    let id = id.clone();
                    prop(move || session.with(|session| session.as_deref() == Some(id.as_str())))
                };
                let switch = switch.clone();
                let key: &'static str = Box::leak(format!("session-{id}").into_boxed_str());
                cx.item(key, label, current, move || switch(Some(id.clone())));
            });
        });
        cx.group(Sty::column().gap(10.0).grow(1.0), move |cx| {
        cx.group(Sty::row(), move |cx| {
            // The font has no menu glyph: a word.
            let button = cx.button("sessions", "sessions", move || sidebar.set(!sidebar.get()));
            cx.class(button, "fold");
        });
        let items = move || chat.with(|chat| (0..chat.records().len()).collect::<Vec<_>>());
        let draw = Draw { chat, copier: copier.clone(), level, clicks, lang };
        let area = cx.scroll_at_end(Sty::column().gap(8.0).grow(1.0), |_| {});
        cx.keyed_in(area, items, |at| *at, move |cx, &at| record(cx, at, draw.clone()));
        // What the step is writing now: its thinking, its text.
        cx.show(
            Sty::column().gap(6.0),
            // `off` shows none of it (D5).
            move || level.get() != Detail::Off && chat.with(|chat| chat.thinking.is_some()),
            move |cx| {
                let text = prop(move || chat.with(|chat| chat.thinking.clone().unwrap_or_default()));
                cx.paragraph(text, Tone::Dim);
            },
        );
        cx.show(
            Sty::column().gap(6.0),
            move || chat.with(|chat| chat.writing.is_some()),
            move |cx| {
                let text = prop(move || chat.with(|chat| chat.writing.clone().unwrap_or_default()));
                cx.markdown(Sty::column().gap(6.0), text);
            },
        );
        let answer_yes = answer.clone();
        cx.show(
            Sty::row().gap(8.0).centre_across(),
            move || question.with(Option::is_some),
            move |cx| {
                let text = prop(move || {
                    question.with(|question| {
                        question
                            .as_ref()
                            .map(|(_, text)| text.clone())
                            .unwrap_or_default()
                    })
                });
                cx.paragraph(text, Tone::Normal);
                let (yes, no) = (answer_yes.clone(), answer_yes.clone());
                cx.button("yes", "Yes", move || yes(true));
                cx.button("no", "No", move || no(false));
            },
        );
        let stop = stop.clone();
        cx.show(
            Sty::row(),
            move || busy.get(),
            move |cx| {
                let stop = stop.clone();
                cx.button("stop", "Stop", stop);
            },
        );
        cx.show(
            Sty::row(),
            move || heard.with(Option::is_some),
            move |cx| {
                cx.paragraph(prop(move || heard.get().unwrap_or_default()), Tone::Dim);
            },
        );
        let remove = remove.clone();
        cx.show(
            Sty::row().gap(8.0).centre_across(),
            move || confirm.with(Option::is_some),
            move |cx| {
                let text = prop(move || {
                    // The buttons answer: the terminal's `[y/N]` is not shown.
                    confirm
                        .get()
                        .map(|name| tr!(lang, m::MEMORY_REMOVE_ASK, name = name).trim_end_matches(" [y/N]").to_owned())
                        .unwrap_or_default()
                });
                cx.paragraph(text, Tone::Normal);
                let (yes, no) = (remove.clone(), remove.clone());
                cx.button("remove-yes", "Yes", move || yes(true));
                cx.button("remove-no", "No", move || no(false));
            },
        );
        let submit = submit.clone();
        let field = cx.area(
            "prompt",
            "Message",
            prompt,
            100_000,
            8,
            move |text: String| {
                menu_open.set(text.starts_with('/') && !text.contains(char::is_whitespace));
                menu_pick.set(0);
                prompt.set(text);
            },
            submit,
        );
        // The row under the field, as the REPL's dock (AGL-139, D1): what
        // runs the session, the phase and its time, the speed of what is
        // being decoded, and the button of the level of detail (D5).
        cx.group(Sty::row().gap(12.0), move |cx| {
            let read = |what: fn(&Chat) -> String| prop(move || chat.with(what));
            let node = cx.label(read(|chat| chat.dock().function.clone()), Tone::Normal);
            cx.class(node, "chat.dock");
            let node = cx.label(read(|chat| chat.dock().usage()), Tone::Normal);
            cx.class(node, "chat.usage");
            let node = cx.label(
                prop(move || {
                    clock.get();
                    chat.with(|chat| {
                        let dock = chat.dock();
                        let label = dock.phase_label(lang);
                        match dock.phase_time().filter(|_| dock.phase != Phase::Ready) {
                            Some(time) => format!("{label} \u{b7} {}", words::duration(time.as_secs())),
                            None => label,
                        }
                    })
                }),
                Tone::Normal,
            );
            cx.class(node, "chat.activity");
            let node = cx.label(
                prop(move || {
                    chat.with(|chat| chat.dock().speed().map_or_else(String::new, |speed| words::speed(speed, lang)))
                }),
                Tone::Dim,
            );
            cx.class(node, "chat.usage");
            let button = cx.button("detail", prop(move || level_word(level.get()).to_owned()), move || {
                level.update(|level| *level = next_level(*level));
                clicks.set(HashMap::new());
            });
            cx.class(button, "fold");
        });
        let pick = {
            let menu = menu.clone();
            move || shown.get().get(menu_pick.get()).map(|&at| menu[at].0)
        };
        let take = pick.clone();
        let press = press.clone();
        cx.on_field_key(field, move |key| {
            // Ctrl-R and Alt-R dictate (D2).
            if let KeyInput::Shortcut { letter: 'r', ctrl, alt, .. } = key.input
                && (ctrl || alt)
            {
                press(alt);
                return true;
            }
            if !menu_open.get() {
                // Up at the first line: the message before (D2).
                if key.line == 0
                    && let KeyInput::Press { key: Key::Up, shift: false, ctrl: false } = key.input
                {
                    let count = sent.with(Vec::len);
                    if count == 0 {
                        return false;
                    }
                    let at = recalled.get().map_or(count - 1, |at| at.saturating_sub(1));
                    recalled.set(Some(at));
                    prompt.set(sent.with(|sent| sent[at].clone()));
                    return true;
                }
                return false;
            }
            let KeyInput::Press { key, .. } = &key.input else {
                return false;
            };
            let count = shown.get().len();
            match key {
                Key::Up => menu_pick.set(menu_pick.get().saturating_sub(1)),
                Key::Down => menu_pick.set((menu_pick.get() + 1).min(count.saturating_sub(1))),
                Key::Enter => {
                    if let Some(name) = take() {
                        prompt.set(format!("{name} "));
                    }
                    menu_open.set(false);
                }
                Key::Escape => menu_open.set(false),
                _ => return false,
            }
            true
        });
        // Ctrl-V of a picture or of files attaches them (D2; KS-052).
        let attach_file = attach_file;
        cx.on_paste(field, move |pasted| match pasted {
            Pasted::Image { mime, bytes } => {
                let ext = mime.strip_prefix("image/").unwrap_or("png").to_owned();
                let source = Source {
                    kind: agentlibre_common::media::Kind::Image,
                    bytes,
                    name: format!("clipboard.{ext}"),
                    ext,
                };
                let mut placeholder = String::new();
                pending.update(|pending| placeholder = attach::add_counted(&counts.get(), pending, source));
                prompt.update(|prompt| {
                    prompt.push_str(&placeholder);
                    prompt.push(' ');
                });
            }
            Pasted::Files(paths) => {
                for path in paths {
                    attach_file(path);
                }
            }
        });
        let rows = menu.clone();
        cx.popup_over(
            "commands",
            field,
            move || shown.get().iter().fold(0u64, |key, &at| key * 31 + at as u64 + 1),
            menu_open,
            move |cx| {
                for (row, &at) in shown.get().iter().enumerate() {
                    let (name, what) = rows[at];
                    cx.item(
                        name,
                        format!("{name}  {what}"),
                        prop(move || menu_pick.get() == row),
                        move || {
                            prompt.set(format!("{name} "));
                            menu_open.set(false);
                        },
                    );
                }
            },
        );
        });
    });
    ui
}

/// What drawing a record needs (AGL-139, D4, D5).
#[derive(Clone)]
struct Draw {
    chat: Signal<Chat>,
    /// What the copy buttons copy through (KS-041 D3).
    copier: Ui,
    /// The level of detail the whole chat shows, from `repl.thinking` (D5).
    level: Signal<Detail>,
    /// The blocks the human opened or closed by hand, by their key: until a
    /// block is here, its state follows the level (D5).
    clicks: Signal<HashMap<(usize, char), bool>>,
    lang: Lang,
}

/// The word of a level of detail (AGL-139, D5): the name `repl.thinking`
/// takes, the same in either language.
fn level_word(level: Detail) -> &'static str {
    match level {
        Detail::Off => "off",
        Detail::Flexible => "flexible",
        Detail::Full => "full",
    }
}

/// The level a button going round the three gives after this one (AGL-139,
/// D5).
fn next_level(level: Detail) -> Detail {
    match level {
        Detail::Off => Detail::Flexible,
        Detail::Flexible => Detail::Full,
        Detail::Full => Detail::Off,
    }
}

/// A block that opens and closes (AGL-139, D5): katarsa's fold, but its
/// state is the level's until the human clicks its header, and the click
/// opens or folds at any level.
fn block(
    cx: &mut Cx,
    id: &'static str,
    key: (usize, char),
    header: impl Fn() -> String + 'static,
    d: Draw,
    follows: impl Fn() -> bool + 'static,
    build: impl Fn(&mut Cx) + 'static,
) {
    let open = {
        let d = d.clone();
        Rc::new(move || d.clicks.get().get(&key).copied().unwrap_or_else(&follows))
    };
    let press = {
        let open = Rc::clone(&open);
        move || {
            let now = !open();
            d.clicks.update(|clicks| {
                clicks.insert(key, now);
            });
        }
    };
    let text = {
        let open = Rc::clone(&open);
        prop(move || {
            let marker = if open() { "\u{25be}" } else { "\u{25b8}" };
            format!("{marker} {}", header())
        })
    };
    let node = cx.button(id, text, press);
    cx.class(node, "fold");
    cx.show(Sty::column().gap(6.0), move || open(), build);
}

/// The record `at`, as the block the human chose (AGL-139, D4): the message,
/// the thinking behind a "thought Ns" button, the answer in Markdown with its
/// copy button and its row, a call behind "TOOL name · status · time".
fn record(cx: &mut Cx, at: usize, d: Draw) {
    let Some(kind) = record_at(d.chat, at, |record| match record {
        Record::User(_) => 0,
        Record::Wake { .. } => 1,
        Record::State(_) => 2,
        Record::Tool { .. } => 3,
        Record::Answer { .. } => 4,
        Record::Notice { .. } => 5,
    }) else {
        return;
    };
    let read = move |read: fn(&Record) -> String| prop(move || record_at(d.chat, at, read).unwrap_or_default());
    match kind {
        0 => {
            cx.label("you", Tone::Dim);
            cx.paragraph(read(|record| match record {
                Record::User(text) => text.clone(),
                _ => String::new(),
            }), Tone::Normal);
        }
        1 => {
            cx.label(read(|record| match record {
                Record::Wake { flow, .. } => format!("wake {flow}"),
                _ => String::new(),
            }), Tone::Dim);
            cx.paragraph(read(|record| match record {
                Record::Wake { text, .. } => text.clone(),
                _ => String::new(),
            }), Tone::Normal);
        }
        2 => {
            cx.label(read(|record| match record {
                Record::State(state) => format!("[{state}]"),
                _ => String::new(),
            }), Tone::Dim);
        }
        3 => {
            thinking(cx, at, d.clone());
            tool(cx, at, d.clone());
        }
        4 => {
            thinking(cx, at, d.clone());
            let text = read(|record| match record {
                Record::Answer { text, .. } => text.clone(),
                _ => String::new(),
            });
            cx.markdown(Sty::column().gap(6.0), text);
            let ui = d.copier.clone();
            cx.group(Sty::row(), move |cx| {
                let ui = ui.clone();
                let button = cx.button("copy", "copy", move || {
                    if let Some(source) = record_at(d.chat, at, |record| match record {
                        Record::Answer { text, .. } => text.clone(),
                        _ => String::new(),
                    }) {
                        ui.copy(source);
                    }
                });
                // A quiet button, as a fold's header.
                cx.class(button, "fold");
            });
            // The row under the answer (AGL-139, D6): its turn's phases and
            // their times, the time it took, its tokens and their speed.
            let chat = d.chat;
            cx.show(
                Sty::row(),
                move || chat.with(|chat| chat.turn_row(at).is_some()),
                move |cx| {
                    cx.label(prop(move || chat.with(|chat| chat.turn_row(at).unwrap_or_default())), Tone::Dim);
                },
            );
        }
        _ => {
            let failed = record_at(d.chat, at, |record| {
                matches!(record, Record::Notice { role: Role::StatusFailure, .. })
            }) == Some(true);
            let node = cx.paragraph(read(|record| match record {
                Record::Notice { line, .. } => line.clone(),
                _ => String::new(),
            }), if failed { Tone::Normal } else { Tone::Dim });
            if failed {
                cx.class(node, "chat.failure");
            }
        }
    }
}

/// The thinking of the step of the record `at` (AGL-139, D4, D5): `off` none
/// of it, else a "thought Ns" button that opens it, open at `full` and
/// folded to its time at `flexible` until the human clicks it.
fn thinking(cx: &mut Cx, at: usize, d: Draw) {
    let text = move || {
        record_at(d.chat, at, |record| match record {
            Record::Answer { thinking, .. } | Record::Tool { thinking, .. } => thinking.clone(),
            _ => None,
        })
        .flatten()
    };
    let level = d.level;
    cx.show(
        Sty::column(),
        move || level.get() != Detail::Off && text().is_some(),
        move |cx| {
            let chat = d.chat;
            let lang = d.lang;
            let header = move || {
                let time = chat.with(|chat| chat.thinking_time(at));
                let word = tr!(lang, m::THOUGHT);
                match time {
                    Some(time) => format!("{word} {}", words::duration(time.as_secs().max(1))),
                    None => word.to_owned(),
                }
            };
            let level = d.level;
            block(cx, "thinking", (at, 't'), header, d.clone(), move || level.get() == Detail::Full, move |cx| {
                cx.paragraph(prop(move || text().unwrap_or_default()), Tone::Dim);
            });
        },
    );
}

/// A call as a block (AGL-139, D4): "TOOL name · status · time", opening on
/// what the model wrote before it, on its input and on its result.
fn tool(cx: &mut Cx, at: usize, d: Draw) {
    let header = {
        let chat = d.chat;
        move || record_at(chat, at, |record| match record {
            Record::Tool { call, result, phases, .. } => {
                let (status, _) = records::status(result.as_ref().map(|result| result.outcome));
                let times = records::phase_items(phases).join(" \u{b7} ");
                format!("TOOL {} \u{b7} {status} {times}", call.name)
            }
            _ => String::new(),
        })
        .unwrap_or_default()
    };
    let level = d.level;
    block(cx, "call", (at, 'c'), header, d.clone(), move || level.get() == Detail::Full, move |cx| {
        let chat = d.chat;
        let part = move |read: fn(&Record) -> Option<String>| move || record_at(chat, at, read).flatten().unwrap_or_default();
        let text = part(|record| match record {
            Record::Tool { text, .. } => text.clone(),
            _ => None,
        });
        let input = part(|record| match record {
            Record::Tool { call, .. } => Some(records::call_input(call)),
            _ => None,
        });
        let output = part(|record| match record {
            Record::Tool { result, .. } => result.as_ref().map(|result| result.content.clone()),
            _ => None,
        });
        let text_shown = text;
        cx.show(Sty::column(), move || !text_shown().is_empty(), move |cx| {
            cx.markdown(Sty::column().gap(6.0), prop(text));
        });
        let node = cx.group(Sty::column(), move |cx| {
            cx.paragraph(prop(input), Tone::Normal);
        });
        cx.class(node, "markdown.code");
        let output_shown = output;
        cx.show(Sty::column(), move || !output_shown().is_empty(), move |cx| {
            // A paragraph, not a label: a result's line may be many.
            cx.paragraph("RESULT", Tone::Dim);
            let node = cx.group(Sty::column(), |cx| {
                cx.paragraph(prop(output), Tone::Normal);
            });
            cx.class(node, "markdown.code");
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Result<Option<Args>, String> {
        parse(&list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn the_command_line_names_a_session_a_plane_and_a_function() {
        assert_eq!(args(&[]), Ok(Some(Args::default())));
        assert_eq!(
            args(&["--resume", "last", "--plane", "work", "--function", "f", "--think", "low", "--yes"]),
            Ok(Some(Args {
                resume: Some("last".into()),
                plane: Some("work".into()),
                function: Some("f".into()),
                think: Some("low".into()),
                yes: true,
                ..Args::default()
            }))
        );
        // With no id it lists.
        assert_eq!(args(&["--resume"]), Ok(Some(Args { list: true, ..Args::default() })));
        assert_eq!(
            args(&["--resume", "--plane", "p"]),
            Ok(Some(Args { list: true, plane: Some("p".into()), ..Args::default() }))
        );
        assert!(args(&["--think", "much"]).is_err());
        assert_eq!(args(&["--help"]), Ok(None));
        assert!(args(&["--what"]).is_err());
    }
}
