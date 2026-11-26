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

use agentlibre_client::{Client, Start};
use agentlibre_common::i18n::Lang;
use agentlibre_common::style::Role;
use agentlibre_proto::{self as proto, Ask, Phase, Reply, Report, Request, Sent};
use agentlibre_surface::records::{self, Record};
use chat::Chat;
use katarsa_app::App;
use katarsa_reactive::signal;
use katarsa_ui::reactive::{Ui, prop};
use katarsa_ui::retained::{Sty, Tone};
use katarsa_ui::style::{Keys, Sheet, Style, StyleBuilder};
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

/// What the command line asks.
#[derive(Debug, Default, PartialEq)]
struct Args {
    resume: Option<String>,
    plane: Option<String>,
    function: Option<String>,
}

/// The arguments; `None` for the help.
fn parse(args: &[String]) -> Result<Option<Args>, String> {
    let mut parsed = Args::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--resume" => parsed.resume = Some(value(arg)?),
            "--plane" => parsed.plane = Some(value(arg)?),
            "--function" => parsed.function = Some(value(arg)?),
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

/// The window's styles (KS-032), logical pixels.
fn styles() -> Sheet {
    let control = |style: StyleBuilder| style.pad_xy(10.0, 6.0);
    Sheet::new()
        .define(
            "chat",
            Style::builder()
                .fill("oklch(0.19 0.01 260)")
                .text_size(15.0),
        )
        .define("chat.field", control(Style::builder()))
        .define("chat.button", control(Style::builder()))
}

/// What a reply of the daemon does.
type OnReply = Box<dyn FnMut(Reply)>;

thread_local! {
    /// What a reply of the daemon does, on the loop's thread.
    static ON_REPLY: RefCell<Option<OnReply>> = const { RefCell::new(None) };
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
    let chat = signal_chat(lang);
    let session = signal(None::<String>);
    let busy = signal(false);
    let question = signal(None::<(u64, String)>);
    let prompt = signal(String::new());
    let past = signal(Vec::<proto::PastSession>::new());
    let sidebar = signal(true);
    let place = Place {
        dir: dir.to_string_lossy().into_owned(),
        plane: args.plane.clone(),
    };

    let mut app = App::new("agl-chat")?;
    {
        let out = Arc::clone(&out);
        let place = place.clone();
        let on_reply = move |reply: Reply| {
            let ours = |of: &str| session.with(|session| session.as_deref() == Some(of));
            match reply {
                Reply::Opened(opened) => {
                    let id = opened.session.id;
                    session.set(Some(id.clone()));
                    send(&out, &Request::History { session: id });
                    send(&out, &place.past());
                }
                Reply::Past { sessions } => past.set(sessions),
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
                    chat.update(|chat| chat.note(&error, Role::StatusFailure));
                }
                _ => {}
            }
        };
        ON_REPLY.with(|slot| *slot.borrow_mut() = Some(Box::new(on_reply)));
    }
    // The daemon's lines, read apart and handed to the loop.
    let handle = app.handle();
    let client = Arc::clone(&out);
    std::thread::spawn(move || {
        let post = |reply: Reply| {
            handle.post(move || {
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
    send(
        &out,
        &Request::Open(proto::Open {
            dir: dir.to_string_lossy().into_owned(),
            plane: args.plane,
            resume: args.resume,
            function: args.function,
            ..proto::Open::default()
        }),
    );

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
    plane: Option<String>,
}

/// How many past sessions the list shows, as `agl --resume` lists them.
const PAST: usize = 20;

impl Place {
    /// The request of the sessions of the directory's project, or of its
    /// plane (AGL-126, D9; AGL-128, D6).
    fn past(&self) -> Request {
        match &self.plane {
            Some(plane) => Request::Past { dir: None, plane: Some(plane.clone()), limit: PAST },
            None => Request::Past { dir: Some(self.dir.clone()), plane: None, limit: PAST },
        }
    }

    /// The request that opens the session `resume`, or a new one.
    fn open(&self, resume: Option<String>) -> Request {
        Request::Open(proto::Open {
            dir: self.dir.clone(),
            plane: self.plane.clone(),
            resume,
            ..proto::Open::default()
        })
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
    } = state;
    let ui = Ui::new("chat");
    ui.styles(styles()).expect("the chat's styles work out");
    ui.set_root_style(Sty::row().gap(14.0).pad(14.0));
    // Another session of the list: this one closed, that one opened; its
    // past comes with its `opened`.
    let switch = {
        let out = Arc::clone(out);
        let place = place.clone();
        move |resume: Option<String>| {
            if busy.get() {
                return;
            }
            if let Some(id) = session.get() {
                send(&out, &Request::Close { session: id });
            }
            session.set(None);
            chat.update(|chat| chat.history(&[], &[]));
            send(&out, &place.open(resume));
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
    ui.on_cancel(stop.clone());
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
    let submit = {
        let out = Arc::clone(out);
        move |text: String| {
            let text = text.trim().to_owned();
            let Some(id) = session.get() else { return };
            if text.is_empty() {
                return;
            }
            send(
                &out,
                &Request::Say {
                    session: id,
                    message: text.clone(),
                    attached: Vec::new(),
                    reasoning: None,
                },
            );
            chat.update(|chat| chat.said(&text));
            prompt.set(String::new());
            busy.set(true);
        }
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
        });
        let area = cx.scroll_at_end(Sty::column().gap(8.0).grow(1.0), |_| {});
        let items = move || chat.with(|chat| (0..chat.records().len()).collect::<Vec<_>>());
        let copier = copier.clone();
        cx.keyed_in(area, items, |at| *at, move |cx, &at| record(cx, chat, at, &copier));
        // What the step is writing now: its thinking, its text.
        cx.show(
            Sty::column().gap(6.0),
            move || chat.with(|chat| chat.thinking.is_some()),
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
        let submit = submit.clone();
        cx.area(
            "prompt",
            "Message",
            prompt,
            100_000,
            8,
            move |text| prompt.set(text),
            submit,
        );
        });
    });
    ui
}

/// The record `at`, as the transcript draws it (AGL-066): a message, a
/// line, a call folded with its result (KS-042), an answer in Markdown with a
/// button that copies its source (KS-041 D3).
fn record(cx: &mut katarsa_ui::reactive::Cx, chat: Signal<Chat>, at: usize, copier: &Ui) {
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
        3 => tool(cx, chat, at),
        4 => {
            if record_at(chat, at, |record| matches!(record, Record::Answer { thinking: Some(_), .. })) == Some(true) {
                cx.fold("thinking", "thinking", move |cx| {
                    cx.paragraph(read(|record| match record {
                        Record::Answer { thinking, .. } => thinking.clone().unwrap_or_default(),
                        _ => String::new(),
                    }), Tone::Dim);
                });
            }
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
            let tone = match record_at(chat, at, |record| match record {
                Record::Notice { role, .. } => *role,
                _ => Role::Muted,
            }) {
                Some(Role::StatusFailure) => Tone::Normal,
                _ => Tone::Dim,
            };
            cx.paragraph(read(|record| match record {
                Record::Notice { line, .. } => line.clone(),
                _ => String::new(),
            }), tone);
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
        let thinking = part(|record| match record {
            Record::Tool { thinking, .. } => thinking.clone(),
            _ => None,
        });
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
        let thinking_shown = thinking.clone();
        cx.show(Sty::column(), move || !thinking_shown().is_empty(), move |cx| {
            cx.paragraph(prop(thinking.clone()), Tone::Dim);
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
            args(&["--resume", "last", "--plane", "work", "--function", "f"]),
            Ok(Some(Args {
                resume: Some("last".into()),
                plane: Some("work".into()),
                function: Some("f".into()),
            }))
        );
        assert_eq!(args(&["--help"]), Ok(None));
        assert!(args(&["--resume"]).is_err());
        assert!(args(&["--what"]).is_err());
    }
}
