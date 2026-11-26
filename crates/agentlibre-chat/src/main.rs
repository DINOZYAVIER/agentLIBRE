//! `agl-chat` (AGL-073): agentLIBRE's chat, a window drawn by katarsa and a
//! client of `agl daemon` (D5). It opens a session in the current directory
//! as `agl` does, shows its past (D10), and its turns as they go: the answer
//! as it arrives, a tool call or a stop a dim line (D9). Enter sends,
//! Shift+Enter breaks a line, Escape or Stop stops the turn; a plane's yes
//! or no question has two buttons (D11).

use std::cell::RefCell;
use std::process::ExitCode;
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
use katarsa_ui::reactive::{Pasted, Ui, prop};
use katarsa_ui::{Key, KeyInput};
use katarsa_ui::retained::{Sty, Tone};
use katarsa_wayland::WindowConfig;


const HELP_EN: &str = "\
Usage: agl-chat [--resume <id|name|last>] [--plane <name>] [--function <name>]

agentLIBRE's chat: a window on a session of agl daemon, in the current
directory, as agl opens one. Enter sends, Shift+Enter breaks a line, Escape
or Stop stops the turn. The daemon is started when none runs.

  --resume <id|name|last>  go on with a session; its past shows
  --plane <name>           the session in that plane
  --function <name>        a new session with that Function
  -h, --help               this help";

const HELP_UK: &str = "\
Використання: agl-chat [--resume <id|назва|last>] [--plane <назва>] [--function <назва>]

Чат agentLIBRE: вікно на сесію agl daemon у поточній теці, як її відкриває
agl. Enter надсилає, Shift+Enter переносить рядок, Escape або Зупинити
зупиняє хід. Демон запускається, коли жоден не працює.

  --resume <id|назва|last>  продовжити сесію; її минуле показується
  --plane <назва>           сесія в цьому плані
  --function <назва>        нова сесія з цією Function
  -h, --help                ця довідка";

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

/// What the text of a dictation does, once the daemon gave it.
type OnVoice = Box<dyn FnMut(Result<String, String>, voice::Mode)>;

thread_local! {
    /// What a reply of the daemon does, on the loop's thread.
    static ON_REPLY: RefCell<Option<OnReply>> = const { RefCell::new(None) };
    /// What a dictation's text does, on the loop's thread.
    static ON_VOICE: RefCell<Option<OnVoice>> = const { RefCell::new(None) };
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
    let confirm = signal(None::<String>);
    let switching = signal(None::<slash::Switch>);

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
                } => chat.update(|chat| chat.history(&events, &transitions)),
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
                        chat.update(|chat| chat.history(&[], &[]));
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
    quit: katarsa_app::Quit,
    /// The user's `repl` settings (D4).
    repl: ReplSettings,
    /// What brings a dictation's text back to the loop.
    handle: katarsa_app::Handle,
    /// The user's `colors` (D8).
    colors: Vec<(Role, agentlibre_common::style::Style)>,
}

/// What the line over the field says while the microphone records.
fn recording_line(lang: Lang) -> String {
    match lang {
        Lang::Uk => "запис… Ctrl-R надсилає, Alt-R ставить у рядок".into(),
        _ => "recording… Ctrl-R sends, Alt-R puts it in the line".into(),
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
    // Escape leaves the inspect view first, then stops the turn.
    let inspect = signal(false);
    {
        let stop = stop.clone();
        ui.on_cancel(move || {
            if inspect.get() {
                inspect.set(false);
            } else {
                stop();
            }
        });
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
    // What the microphone is doing, while it does.
    let heard = signal(None::<String>);
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
            match (text, mode) {
                (Ok(text), voice::Mode::Send) if !text.trim().is_empty() => submit(text),
                (Ok(text), _) => prompt.update(|prompt| prompt.push_str(text.trim())),
                (Err(error), _) => chat.update(|chat| chat.note(&error, Role::StatusFailure)),
            }
        };
        ON_VOICE.with(|slot| *slot.borrow_mut() = Some(Box::new(on_voice)));
    }
    let microphone = voice::Voice::default();
    // Ctrl-R or Alt-R (D2): the first press records, the second stops and
    // hands the recording to the daemon's whisper.
    let press = {
        let microphone = microphone.clone();
        move |alt: bool| match microphone.press(alt, repl.max_record_seconds, lang) {
            voice::Press::Started => heard.set(Some(recording_line(lang))),
            voice::Press::Failed(error) => {
                heard.set(None);
                chat.update(|chat| chat.note(&error, Role::StatusFailure));
            }
            voice::Press::Stopped(wav, mode) => {
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
                                .map(|session| session.name.clone().unwrap_or_else(|| session.first.clone()))
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
            // The REPL's Ctrl-O (D2): the session's records in full.
            let button = cx.button("inspect", "inspect", move || inspect.set(!inspect.get()));
            cx.class(button, "fold");
        });
        let items = move || chat.with(|chat| (0..chat.records().len()).collect::<Vec<_>>());
        let copier = copier.clone();
        let detail = repl.thinking;
        cx.show(Sty::column().grow(1.0), move || !inspect.get(), move |cx| {
            let area = cx.scroll_at_end(Sty::column().gap(8.0).grow(1.0), |_| {});
            let copier = copier.clone();
            cx.keyed_in(area, items, |at| *at, move |cx, &at| record(cx, chat, at, &copier, detail, lang));
        });
        cx.show(Sty::column().grow(1.0), move || inspect.get(), move |cx| {
            let area = cx.scroll_at_end(Sty::column().gap(10.0).grow(1.0), |_| {});
            cx.keyed_in(area, items, |at| *at, move |cx, &at| inspected(cx, chat, at));
        });
        // What the step is writing now: its thinking, its text.
        cx.show(
            Sty::column().gap(6.0),
            // `off` shows none of it (D4).
            move || repl.thinking != Detail::Off && chat.with(|chat| chat.thinking.is_some()),
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
        let pick = {
            let menu = menu.clone();
            move || shown.get().get(menu_pick.get()).map(|&at| menu[at].0)
        };
        let take = pick.clone();
        let press = press.clone();
        cx.on_field_key(field, move |key| {
            // Ctrl-O: the inspect view (D2).
            if let KeyInput::Shortcut { letter: 'o', ctrl: true, alt: false, .. } = key.input {
                inspect.set(!inspect.get());
                return true;
            }
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
        let attach_file = attach_file.clone();
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

/// The record `at`, as the transcript draws it (AGL-066): a message, a
/// line, a call folded with its result (KS-042), an answer in Markdown with a
/// button that copies its source (KS-041 D3).
fn record(cx: &mut katarsa_ui::reactive::Cx, chat: Signal<Chat>, at: usize, copier: &Ui, detail: Detail, lang: Lang) {
    let Some(kind) = record_at(chat, at, |record| match record {
        Record::User(_) => 0,
        Record::Wake { .. } => 1,
        Record::State(_) => 2,
        Record::Tool { .. } => 3,
        Record::Answer { .. } => 4,
        Record::Notice { .. } => 5,
    }) else {
        return;
    };
    let read = move |read: fn(&Record) -> String| prop(move || record_at(chat, at, read).unwrap_or_default());
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
            thinking(cx, chat, at, detail, lang);
            tool(cx, chat, at);
        }
        4 => {
            thinking(cx, chat, at, detail, lang);
            let text = read(|record| match record {
                Record::Answer { text, .. } => text.clone(),
                _ => String::new(),
            });
            cx.markdown(Sty::column().gap(6.0), text);
            let ui = copier.clone();
            cx.group(Sty::row(), move |cx| {
                let ui = ui.clone();
                let button = cx.button("copy", "copy", move || {
                    if let Some(source) = record_at(chat, at, |record| match record {
                        Record::Answer { text, .. } => text.clone(),
                        _ => String::new(),
                    }) {
                        ui.copy(source);
                    }
                });
                // A quiet button, as a fold's header.
                cx.class(button, "fold");
            });
        }
        _ => {
            let failed = record_at(chat, at, |record| {
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

/// The thinking of the step of the record `at`, as `repl.thinking` says
/// (D4, D7): `off` none of it, `flexible` folded to its time, `full` open.
fn thinking(cx: &mut katarsa_ui::reactive::Cx, chat: Signal<Chat>, at: usize, detail: Detail, lang: Lang) {
    let text = move || {
        record_at(chat, at, |record| match record {
            Record::Answer { thinking, .. } | Record::Tool { thinking, .. } => thinking.clone(),
            _ => None,
        })
        .flatten()
    };
    if detail == Detail::Off || text().is_none() {
        return;
    }
    let header = prop(move || {
        let time = chat.with(|chat| chat.thinking_time(at));
        let word = if lang == Lang::Uk { "думав" } else { "thought" };
        match time {
            Some(time) => format!("{word} {}", words::duration(time.as_secs().max(1))),
            None => word.to_owned(),
        }
    });
    let body = move || prop(move || text().unwrap_or_default());
    if detail == Detail::Full {
        cx.label(header, Tone::Dim);
        cx.paragraph(body(), Tone::Dim);
    } else {
        cx.fold("thinking", header, move |cx| {
            cx.paragraph(body(), Tone::Dim);
        });
    }
}

/// The record `at` in full, as the REPL's inspect mode shows it (AGL-066,
/// D4): every part, the call's phases and their times.
fn inspected(cx: &mut katarsa_ui::reactive::Cx, chat: Signal<Chat>, at: usize) {
    let parts = record_at(chat, at, |record| -> Vec<(String, bool)> {
        // Each part: its text, and whether it is a heading.
        let mut parts = Vec::new();
        match record {
            Record::User(text) => parts.push((format!("\u{203a} {text}"), false)),
            Record::Wake { flow, text } => parts.push((format!("wake {flow}: {text}"), false)),
            Record::State(state) => parts.push((format!("[{state}]"), true)),
            Record::Tool { call, result, phases, thinking, text, .. } => {
                if let Some(thinking) = thinking {
                    parts.push(("THINKING".into(), true));
                    parts.push((thinking.clone(), false));
                }
                if let Some(text) = text.as_ref().filter(|text| !text.trim().is_empty()) {
                    parts.push((text.clone(), false));
                }
                let (status, _) = records::status(result.as_ref().map(|result| result.outcome));
                let times = records::phase_items(phases).join(" · ");
                parts.push((format!("TOOL {} · {status} {times}", call.name), true));
                parts.push((records::call_input(call), false));
                if let Some(result) = result {
                    parts.push(("RESULT".into(), true));
                    parts.push((result.content.clone(), false));
                }
            }
            Record::Answer { text, thinking, .. } => {
                if let Some(thinking) = thinking {
                    parts.push(("THINKING".into(), true));
                    parts.push((thinking.clone(), false));
                }
                parts.push(("ANSWER".into(), true));
                parts.push((text.clone(), false));
            }
            Record::Notice { line, detail, .. } => {
                parts.push((line.clone(), true));
                if let Some(detail) = detail {
                    parts.push((detail.clone(), false));
                }
            }
        }
        parts
    })
    .unwrap_or_default();
    for (text, heading) in parts {
        if heading {
            cx.label(text, Tone::Dim);
        } else {
            let node = cx.group(Sty::column(), move |cx| {
                cx.paragraph(text.clone(), Tone::Normal);
            });
            cx.class(node, "markdown.code");
        }
    }
}

/// A call as a fold (KS-042): its header the outcome and the line of what it
/// did, its body the thinking and the text before it, its input in full and
/// its result.
fn tool(cx: &mut katarsa_ui::reactive::Cx, chat: Signal<Chat>, at: usize) {
    let header = prop(move || {
        record_at(chat, at, |record| match record {
            Record::Tool { call, result, hint, .. } => {
                let (status, _) = records::status(result.as_ref().map(|result| result.outcome));
                let what = if hint.is_empty() { call.name.clone() } else { format!("{} {hint}", call.name) };
                format!("{what} · {status}")
            }
            _ => String::new(),
        })
        .unwrap_or_default()
    });
    cx.fold("call", header, move |cx| {
        let part = move |read: fn(&Record) -> Option<String>| {
            move || record_at(chat, at, read).flatten().unwrap_or_default()
        };
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
        let text_shown = text.clone();
        cx.show(Sty::column(), move || !text_shown().is_empty(), move |cx| {
            cx.markdown(Sty::column().gap(6.0), prop(text.clone()));
        });
        let node = cx.group(Sty::column(), move |cx| {
            cx.paragraph(prop(input.clone()), Tone::Normal);
        });
        cx.class(node, "markdown.code");
        let output_shown = output.clone();
        cx.show(Sty::column(), move || !output_shown().is_empty(), move |cx| {
            let node = cx.group(Sty::column(), |cx| {
                cx.paragraph(prop(output.clone()), Tone::Normal);
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
