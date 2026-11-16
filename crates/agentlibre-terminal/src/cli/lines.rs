//! The session without a terminal (D128): every line of stdin is a
//! message; the answers go to stdout and the progress to stderr, as with
//! `-p` (D109).

use std::io::{self, BufRead};
use std::process::ExitCode;

use crate::Setup;
use crate::agent::TurnEnd;
use crate::attach::{self, Pending};
use crate::cli::args::{MemoryAction, memory_action};
use crate::cli::report::StderrReport;
use crate::error::Error;
use crate::i18n::{Lang, m, repl as texts};
use crate::interrupt;
use crate::media::Source;
use crate::run::{Said, Told};
use crate::session::{CommandRun, Event, Session, StopReason};
use crate::status::{
    Facts, Info, Remember, display_path, help_text, parse_remember, status_text,
};
use crate::tr;
use crate::outln;

pub fn run(setup: Setup) -> Result<ExitCode, Error> {
    let Setup {
        mut run,
        server_url,
        ..
    } = setup;
    let function = run.function;
    let function_name = run.function_name;
    let lang = run.lang;
    let workspace = run.workspace.clone();
    let facts = Facts {
        function: function_name.into(),
        workspace: display_path(&workspace),
        server: server_url.into(),
    };
    interrupt::install();
    eprintln!("{}", tr!(lang, m::SESSION, id = run.session.id));
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
            match name {
                "/quit" | "/exit" => {
                    said.record(&mut run.session, message);
                    break;
                }
                "/help" => {
                    let text = help_text(lang, false);
                    outln!("{text}");
                    said.output.push(text.to_string());
                }
                "/status" => {
                    let info = Info {
                        session: run.session.id.clone(),
                        state: state.clone(),
                        unfinished: run.session.unfinished().and_then(|turn| turn.state),
                        context: run.context,
                    };
                    let text = status_text(&facts, &info, run.reasoning, lang);
                    outln!("{text}");
                    said.output.push(text.to_string());
                }
                "/remember" => match parse_remember(&message["/remember".len()..]) {
                    Remember::Entry { name, text } => {
                        said.tell(run.remember(&name, &text), lang)?;
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
                        Error::MemoryRemoveNeedsYes { name }.message(lang)
                    )),
                    Ok(MemoryAction::Remove { name, yes: true }) => {
                        said.tell(run.remove(&name), lang)?;
                    }
                    Ok(action) => said.tell(run.looked_at(&action), lang)?,
                },
                "/attach" => {
                    let path = unquote(message["/attach".len()..].trim());
                    if path.is_empty() {
                        said.fail(tr!(lang, m::ATTACH_USAGE).to_string());
                    } else {
                        match Source::from_file(std::path::Path::new(path)) {
                            Ok(source) => {
                                let name = source.name.clone();
                                let placeholder = attach::add(&run.session, &mut pending, source);
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
                        said.tell(run.rename(name), lang)?;
                    }
                }
                "/new" => {
                    said.record(&mut run.session, message);
                    said = Record::recorded();
                    // A new session records the `run` snapshot of its
                    // Function (AGL-063, D4).
                    said.tell(run.new_session()?, lang)?;
                    pending.clear();
                    placeholders.clear();
                    state = None;
                }
                "/compact" => {
                    let ran = run.compact(&mut StderrReport::new(lang));
                    let interrupted = ran.interrupted;
                    said.tell(ran, lang)?;
                    if interrupted {
                        said.record(&mut run.session, message);
                        return Ok(ExitCode::from(interrupt::EXIT_CODE));
                    }
                }
                _ => said.fail(tr!(lang, m::UNKNOWN_COMMAND, name = name).to_string()),
            }
            said.record(&mut run.session, message);
            continue;
        }
        let mut report = StderrReport::new(lang);
        let text = attach::with_placeholders(&placeholders, message);
        placeholders.clear();
        let attached = std::mem::take(&mut pending);
        let end = run.turn(&text, attached, &mut report);
        state = None;
        match end {
            Ok(TurnEnd::Answer(text)) => outln!("{text}"),
            Ok(TurnEnd::Stopped(StopReason::Interrupted)) => {
                eprintln!(
                    "{}",
                    texts::stopped(StopReason::Interrupted, function, lang)
                );
                return Ok(ExitCode::from(interrupt::EXIT_CODE));
            }
            Ok(TurnEnd::Stopped(reason)) => {
                eprintln!("agl: {}", texts::stopped(reason, function, lang));
            }
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
    fn tell(&mut self, said: Said, lang: Lang) -> Result<(), Error> {
        let Said {
            lines,
            memory,
            content,
            failed: _,
            interrupted: _,
            record_error,
        } = said;
        if let Some(error) = record_error {
            return Err(error);
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
    fn record(&self, session: &mut Session, line: &str) {
        if self.recorded {
            return;
        }
        let _ = session.append(Event::Command(CommandRun {
            line: line.to_owned(),
            failed: self.failed,
            output: Some(self.output.join("\n")).filter(|output| !output.is_empty()),
        }));
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
