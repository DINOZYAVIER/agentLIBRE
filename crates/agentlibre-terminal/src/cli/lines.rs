//! The session without a terminal (D128): every line of stdin is a
//! message; the answers go to stdout and the progress to stderr, as with
//! `-p` (D109).

use std::cell::Cell;
use std::io::{self, BufRead};
use std::process::ExitCode;

use crate::Setup;
use crate::agent::TurnEnd;
use crate::attach::{self, Pending};
use crate::cli::args::{MemoryAction, memory_action};
use crate::cli::commands;
use crate::cli::report::StderrReport;
use crate::error::Error;
use crate::error::CoreError;
use agentlibre_common::i18n::{Lang, m};
use agentlibre_common::reasoning::ReasoningLevel;
use crate::interrupt;
use crate::remote;
use crate::media::Source;
use crate::plane::{Command, Done, Text, Sent, SessionView, Told};
use crate::session::{CommandRun, Event, StopReason};
use crate::status::{
    Facts, Info, Remember, display_path, help_text, parse_remember, status_text,
};
use agentlibre_common::tr;
use crate::outln;

pub fn run(setup: Setup) -> Result<ExitCode, Error> {
    let Setup {
        remote,
        opened,
        lang,
        ..
    } = setup;
    let facts = Facts {
        function: opened.function.clone(),
        workspace: display_path(std::path::Path::new(&opened.workspace)),
        plane: opened.session.plane.clone(),
        server: opened.server_url.clone(),
    };
    let reasoning = opened.reasoning.as_deref().and_then(ReasoningLevel::parse);
    let context = Cell::new(opened.context);
    // What this session holds of its session, after each command.
    let mut session = agentlibre_core::plane::wire::view_of(opened.session.clone());
    interrupt::install();
    remote::forward_stop(&remote);
    eprintln!("{}", tr!(lang, m::SESSION, id = session.id));
    // The plane takes the commands in the daemon (AGL-076, D5).
    let mut ask = |command: Command, session: &mut SessionView| -> Result<Done, Error> {
        let mut report = StderrReport::new(lang);
        let done = remote
            .ask(command, &mut |sent| {
                if let Sent::Context(size) = sent {
                    context.set(Some(size));
                }
                sent.give(&mut report);
            })
            .ok_or(Error::from(CoreError::PlaneGone))?;
        *session = done.session.clone();
        Ok(done)
    };
    let mut state: Option<String> = None;
    // What `/attach` took since the last message (AGL-014, D18).
    let mut pending: Vec<Pending> = Vec::new();
    let mut placeholders: Vec<String> = Vec::new();
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let mut message = line.trim();
        // `/remember text` without a name is an ordinary message: the
        // model names the entry (AGL-006, M37).
        let remembered;
        if message.split_whitespace().next() == Some("/remember")
            && let Remember::Turn(text) = parse_remember(&message["/remember".len()..])
        {
            remembered = text;
            message = &remembered;
        }
        if message.is_empty() {
            continue;
        }
        if message.starts_with('/') {
            let name = message.split_whitespace().next().unwrap_or(message);
            // What the command says goes to the session too (AGL-047, D5).
            let mut said = Record::default();
            // The table says which commands a session without a terminal
            // takes (AGL-121, D9); the rest are unknown here.
            if !commands::takes(name, commands::STDIN) {
                said.fail(tr!(lang, m::UNKNOWN_COMMAND, name = name).to_string());
                said.record(&mut ask, &mut session, message)?;
                continue;
            }
            match name {
                "/quit" | "/exit" => {
                    said.record(&mut ask, &mut session, message)?;
                    break;
                }
                "/help" => {
                    let text = help_text(lang, false);
                    outln!("{text}");
                    said.output.push(text.to_string());
                }
                "/status" => {
                    let info = Info {
                        session: session.id.clone(),
                        state: state.clone(),
                        unfinished: session.unfinished.clone(),
                        context: context.get(),
                    };
                    let text = status_text(&facts, &info, reasoning, lang);
                    outln!("{text}");
                    said.output.push(text.to_string());
                }
                "/remember" => match parse_remember(&message["/remember".len()..]) {
                    Remember::Entry { name, text, workspace } => {
                        let done = ask(Command::Remember { name, text, workspace }, &mut session)?;
                        said.tell(done.said.expect("a command's saying")?, lang)?;
                    }
                    // The turn form was made a message above.
                    Remember::Usage | Remember::Turn(_) => {
                        said.fail(tr!(lang, m::REMEMBER_USAGE).to_string());
                    }
                },
                // With no terminal, a removal needs its `--yes` (AGL-060, D7).
                "/memory" => match memory_action(message.split_whitespace().skip(1).map(str::to_owned)) {
                    Err(_) => said.fail(tr!(lang, m::MEMORY_USAGE).to_string()),
                    Ok(MemoryAction::Remove { name, yes: false }) => said.fail(format!(
                        "agl: {}",
                        crate::memory::Error::RemoveNeedsYes { name }.message(lang)
                    )),
                    Ok(MemoryAction::Remove { name, yes: true }) => {
                        let done = ask(Command::Remove { name }, &mut session)?;
                        said.tell(done.said.expect("a command's saying")?, lang)?;
                    }
                    Ok(action) => {
                        let done = ask(Command::LookAt(action), &mut session)?;
                        said.tell(done.said.expect("a command's saying")?, lang)?;
                    }
                },
                "/attach" => {
                    let path = unquote(message["/attach".len()..].trim());
                    if path.is_empty() {
                        said.fail(tr!(lang, m::ATTACH_USAGE).to_string());
                    } else {
                        match Source::from_file(std::path::Path::new(path)) {
                            Ok(source) => {
                                let name = source.name.clone();
                                let placeholder =
                                    attach::add_counted(&session.media, &mut pending, source);
                                let text =
                                    tr!(lang, m::ATTACHED, placeholder = placeholder, name = name)
                                        .to_string();
                                eprintln!("{text}");
                                said.output.push(text);
                                placeholders.push(placeholder);
                            }
                            Err(error) => said.fail(format!("agl: {}", error.message(lang))),
                        }
                    }
                }
                "/rename" => {
                    let name = message["/rename".len()..].trim();
                    if name.is_empty() {
                        said.fail(tr!(lang, m::RENAME_USAGE).to_string());
                    } else {
                        let done = ask(Command::Rename(name.to_owned()), &mut session)?;
                        said.tell(done.said.expect("a command's saying")?, lang)?;
                    }
                }
                "/new" => {
                    said.record(&mut ask, &mut session, message)?;
                    said = Record::recorded();
                    // A new session records the `run` snapshot of its
                    // Function (AGL-063, D4).
                    let done = ask(Command::New, &mut session)?;
                    said.tell(done.said.expect("a command's saying")?, lang)?;
                    pending.clear();
                    placeholders.clear();
                    state = None;
                }
                "/compact" => {
                    let done = ask(Command::Compact { reasoning }, &mut session)?;
                    let ran = done.said.expect("a command's saying")?;
                    let interrupted = ran.interrupted;
                    said.tell(ran, lang)?;
                    if interrupted {
                        said.record(&mut ask, &mut session, message)?;
                        return Ok(ExitCode::from(interrupt::EXIT_CODE));
                    }
                }
                _ => said.fail(tr!(lang, m::UNKNOWN_COMMAND, name = name).to_string()),
            }
            said.record(&mut ask, &mut session, message)?;
            continue;
        }
        let text = attach::with_placeholders(&placeholders, message);
        placeholders.clear();
        let attached = std::mem::take(&mut pending);
        let done = ask(
            Command::Say {
                message: text,
                attached,
                reasoning,
                observer: None,
            },
            &mut session,
        )?;
        state = None;
        let why = done.why.unwrap_or_default();
        match done.turn.expect("a message's turn") {
            Ok(TurnEnd::Answer(text)) => outln!("{text}"),
            Ok(TurnEnd::Stopped(StopReason::Interrupted)) => {
                eprintln!("{why}");
                return Ok(ExitCode::from(interrupt::EXIT_CODE));
            }
            Ok(TurnEnd::Stopped(_)) => eprintln!("agl: {why}"),
            Err(error) => eprintln!("agl: {}", error.message(lang)),
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `path` without one pair of quotes, as a shell or a file manager gives it.
fn unquote(path: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = path.strip_prefix(quote).and_then(|p| p.strip_suffix(quote)) {
            return inner;
        }
    }
    path
}

/// What a slash command said, for the session (AGL-047, D5).
#[derive(Default)]
struct Record {
    output: Vec<String>,
    failed: bool,
    recorded: bool,
}

impl Record {
    /// For a command already written to the session it leaves, as `/new`.
    fn recorded() -> Record {
        Record {
            recorded: true,
            ..Record::default()
        }
    }

    fn fail(&mut self, text: String) {
        eprintln!("{text}");
        self.output.push(text);
        self.failed = true;
    }

    /// What a command of the run did (AGL-067, F2): its answer to stdout,
    /// a notice to stderr, a refusal plain and an error's text with the
    /// `agl: ` this session puts before it; the answer, a refusal and an
    /// error go into the record, and what the command did when no answer was
    /// printed. A session that would not take the command's line ends the
    /// run (D4).
    fn tell(&mut self, said: Text, lang: Lang) -> Result<(), Error> {
        let Text {
            lines,
            memory,
            content,
            failed: _,
            interrupted: _,
            record_error,
        } = said;
        if let Some(error) = record_error {
            return Err(error.into());
        }
        let mut answer = false;
        for told in lines {
            match told {
                Told::Output(text) => {
                    outln!("{text}");
                    self.output.push(text);
                    answer = true;
                }
                Told::Notice(text) | Told::Pending(text) => eprintln!("{text}"),
                Told::Refusal(text) => self.fail(text),
                Told::Fault(text) => self.fail(format!("agl: {text}")),
            }
        }
        if let Some(write) = memory {
            StderrReport::new(lang).memory(&write.name, write.kind, write.text.as_deref());
        }
        if !self.failed && !answer && let Some(text) = content {
            self.output.push(text);
        }
        Ok(())
    }

    /// A session that cannot be written to says so at the next turn.
    /// The command's line goes to the session's record, by its plane.
    fn record(
        &self,
        ask: &mut impl FnMut(Command, &mut SessionView) -> Result<Done, Error>,
        session: &mut SessionView,
        line: &str,
    ) -> Result<(), Error> {
        if self.recorded {
            return Ok(());
        }
        let event = Event::Command(CommandRun {
            line: line.to_owned(),
            failed: self.failed,
            output: Some(self.output.join("\n")).filter(|output| !output.is_empty()),
        });
        ask(Command::Append(vec![event]), session).map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::unquote;

    #[test]
    fn a_quoted_path_loses_one_pair_of_quotes() {
        assert_eq!(unquote("/a/b.png"), "/a/b.png");
        assert_eq!(unquote("\"/a b/c.png\""), "/a b/c.png");
        assert_eq!(unquote("'/a b/c.png'"), "/a b/c.png");
        assert_eq!(unquote("\"/a b/c.png"), "\"/a b/c.png");
        assert_eq!(unquote(""), "");
    }
}
