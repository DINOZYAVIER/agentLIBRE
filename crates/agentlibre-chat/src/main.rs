//! `agl-chat` (AGL-073): agentLIBRE's chat, a window drawn by katarsa and a
//! client of `agl daemon` (D5). It opens a session in the current directory
//! as `agl` does, shows its past (D10), and its turns as they go: the answer
//! as it arrives, a tool call or a stop a dim line (D9). Enter sends,
//! Shift+Enter breaks a line, Escape or Stop stops the turn; a plane's yes
//! or no question has two buttons (D11).

mod daemon;
mod model;

use std::cell::RefCell;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::rc::Rc;

use agentlibre_proto::{self as proto, Ask, Phase, Read, Reply, Report, Request, Sent};
use katarsa_app::App;
use katarsa_reactive::signal;
use katarsa_ui::reactive::{Ui, prop};
use katarsa_ui::retained::{Sty, Tone};
use katarsa_ui::style::{Keys, Sheet, Style, StyleBuilder};
use katarsa_wayland::WindowConfig;

use model::{Conversation, Who};

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
fn lang() -> &'static str {
    let ukrainian = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.is_empty())
        .is_some_and(|value| value.starts_with("uk"));
    if ukrainian { "uk" } else { "en" }
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
fn send(out: &Rc<RefCell<UnixStream>>, request: &Request) {
    let _ = proto::send(&mut *out.borrow_mut(), request);
}

fn main() -> ExitCode {
    let lang = lang();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse(&args) {
        Ok(Some(args)) => args,
        Ok(None) => {
            println!("{}", if lang == "uk" { HELP_UK } else { HELP_EN });
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

fn run(args: Args, lang: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let dir = std::env::current_dir()?;
    let (out, mut lines) = daemon::connect(lang)?;
    let out = Rc::new(RefCell::new(out));
    let conversation = signal(Conversation::default());
    let session = signal(None::<String>);
    let busy = signal(false);
    let question = signal(None::<(u64, String)>);
    let prompt = signal(String::new());

    let mut app = App::new("agl-chat")?;
    {
        let out = Rc::clone(&out);
        let on_reply = move |reply: Reply| {
            let ours = |of: &str| session.with(|session| session.as_deref() == Some(of));
            match reply {
                Reply::Opened(opened) => {
                    let id = opened.session.id;
                    session.set(Some(id.clone()));
                    send(&out, &Request::History { session: id });
                }
                Reply::History { events, .. } => conversation.update(|conversation| {
                    if conversation.rows.is_empty() {
                        *conversation = Conversation::of_history(&events);
                    }
                }),
                Reply::Sent { session: of, sent } if ours(&of) => {
                    if let Sent::Report(Report::Transition(transition)) = &sent {
                        busy.set(transition.to != Phase::Ready);
                    }
                    conversation.update(|conversation| conversation.sent(&sent));
                }
                Reply::Done {
                    session: of, done, ..
                } if ours(&of) => {
                    if done.turn.is_some() {
                        busy.set(false);
                    }
                    conversation.update(|conversation| conversation.done(&done));
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
                    conversation.update(|conversation| {
                        conversation.note(
                            "a question for a line was refused: answer it with agl in a terminal",
                        );
                    });
                }
                Reply::Refused { error } => {
                    busy.set(false);
                    conversation.update(|conversation| conversation.note(&error));
                }
                _ => {}
            }
        };
        ON_REPLY.with(|slot| *slot.borrow_mut() = Some(Box::new(on_reply)));
    }
    // The daemon's lines, read apart and handed to the loop.
    let handle = app.handle();
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
        while let Ok(Read::Message(reply)) = lines.read::<Reply>() {
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
        move |_| build(&out, conversation, session, busy, question, prompt),
    );
    Ok(app.run()?.clean)
}

/// The window's UI.
fn build(
    out: &Rc<RefCell<UnixStream>>,
    conversation: katarsa_reactive::Signal<Conversation>,
    session: katarsa_reactive::Signal<Option<String>>,
    busy: katarsa_reactive::Signal<bool>,
    question: katarsa_reactive::Signal<Option<(u64, String)>>,
    prompt: katarsa_reactive::Signal<String>,
) -> Ui {
    let ui = Ui::new("chat");
    ui.styles(styles()).expect("the chat's styles work out");
    ui.set_root_style(Sty::column().gap(10.0).pad(14.0));
    let stop = {
        let out = Rc::clone(out);
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
        let out = Rc::clone(out);
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
        let out = Rc::clone(out);
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
            conversation.update(|conversation| conversation.said(&text));
            prompt.set(String::new());
            busy.set(true);
        }
    };
    // What the copy buttons copy through (KS-041 D3).
    let copier = ui.clone();
    ui.mount(move |cx| {
        let area = cx.scroll_at_end(Sty::column().gap(8.0).grow(1.0), |_| {});
        // A message is its speaker's name and its text; the rest a row each.
        let items = move || {
            conversation.with(|conversation| {
                conversation
                    .rows
                    .iter()
                    .enumerate()
                    .flat_map(|(at, row)| {
                        let named = row.who == Who::You;
                        named.then_some((at, false)).into_iter().chain([(at, true)])
                    })
                    .collect::<Vec<_>>()
            })
        };
        cx.keyed_in(
            area,
            items,
            |item| *item,
            move |cx, &(at, body)| {
                if !body {
                    cx.label("you", Tone::Dim);
                    return;
                }
                let who =
                    conversation.with(|conversation| conversation.rows.get(at).map(|row| row.who));
                // The calls between two texts, folded (KS-042 D1, D3).
                if who == Some(Who::Steps) {
                    steps(cx, conversation, at);
                    return;
                }
                let tone = match who {
                    Some(Who::Note) => Tone::Dim,
                    _ => Tone::Normal,
                };
                let text = prop(move || {
                    conversation.with(|conversation| {
                        conversation
                            .rows
                            .get(at)
                            .map(|row| row.text.clone())
                            .unwrap_or_default()
                    })
                });
                // The agent's text is Markdown (katarsa-shell's KS-040), with a
                // button that copies its source (KS-041 D3).
                if who == Some(Who::Agent) {
                    cx.markdown(Sty::column().gap(6.0), text);
                    let ui = copier.clone();
                    cx.group(Sty::row(), move |cx| {
                        let button = cx.button("copy", "copy", move || {
                            let source = conversation.with(|conversation| {
                                conversation.rows.get(at).map(|row| row.text.clone())
                            });
                            if let Some(source) = source {
                                ui.copy(source);
                            }
                        });
                        // A quiet button, as a fold's header.
                        cx.class(button, "fold");
                    });
                } else {
                    cx.paragraph(text, tone);
                }
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
    ui
}

/// A row of steps as a fold: its header the number of calls and the last
/// one's line, its body each call's line and its output (KS-042).
fn steps(
    cx: &mut katarsa_ui::reactive::Cx,
    conversation: katarsa_reactive::Signal<Conversation>,
    at: usize,
) {
    let header = prop(move || {
        conversation.with(|conversation| {
            conversation
                .rows
                .get(at)
                .map(model::Row::header)
                .unwrap_or_default()
        })
    });
    cx.fold("steps", header, move |cx| {
        let calls = move || {
            conversation.with(|conversation| {
                (0..conversation.rows.get(at).map_or(0, |row| row.steps.len())).collect::<Vec<_>>()
            })
        };
        cx.keyed(
            Sty::column().gap(4.0),
            calls,
            |call| *call,
            move |cx, &call| {
                let step = move |read: fn(&model::Step) -> String| {
                    conversation.with(|conversation| {
                        conversation
                            .rows
                            .get(at)
                            .and_then(|row| row.steps.get(call))
                            .map(read)
                            .unwrap_or_default()
                    })
                };
                let line = prop(move || {
                    step(|step| {
                        let mark = if step.failed { "\u{2717} " } else { "" };
                        format!("{mark}{}", step.line)
                    })
                });
                cx.paragraph(line, Tone::Dim);
                cx.show(
                    Sty::column(),
                    move || !step(|step| step.output.clone().unwrap_or_default()).is_empty(),
                    move |cx| {
                        let node = cx.group(Sty::column(), move |cx| {
                            let output =
                                prop(move || step(|step| step.output.clone().unwrap_or_default()));
                            cx.paragraph(output, Tone::Normal);
                        });
                        cx.class(node, "markdown.code");
                    },
                );
            },
        );
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
