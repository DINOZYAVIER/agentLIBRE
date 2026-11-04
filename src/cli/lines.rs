//! The session without a terminal (D128): every line of stdin is a
//! message; the answers go to stdout and the progress to stderr, as with
//! `-p` (D109).

use std::io::{self, BufRead};
use std::process::ExitCode;

use crate::Setup;
use crate::agent::{Agent, Report, TurnEnd};
use crate::attach::{self, Pending};
use crate::cli::report::StderrReport;
use crate::cli::serve::ServerProgress;
use crate::error::Error;
use crate::i18n::{m, repl as texts};
use crate::interrupt;
use crate::media::Source;
use crate::phase::{Observer, PhaseEvent};
use crate::session::{CommandRun, Event, Session, StopReason};
use crate::status::{
    Facts, Info, Remember, compact_notice, display_path, help_text, parse_remember, status_text,
};
use crate::tr;
use crate::outln;

pub fn run(setup: Setup) -> Result<ExitCode, Error> {
    let Setup {
        function,
        function_name,
        backend,
        stirred,
        mut session,
        sessions_dir,
        mut context,
        server_url,
        lang,
        forge,
        reasoning,
        autostart,
        search,
        image_tokens,
        memory,
        ..
    } = setup;
    let workspace = session.workspace.clone();
    let facts = Facts {
        function: function_name.into(),
        workspace: display_path(&workspace),
        server: server_url.into(),
    };
    interrupt::install();
    eprintln!("{}", tr!(lang, m::SESSION, id = session.id));
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
            let mut said = Said::default();
            match name {
                "/quit" | "/exit" => {
                    said.record(&mut session, message);
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
                        unfinished: session.unfinished().and_then(|turn| turn.state),
                        context,
                    };
                    let text = status_text(&facts, &info, reasoning, lang);
                    outln!("{text}");
                    said.output.push(text.to_string());
                }
                "/remember" => match parse_remember(&message["/remember".len()..]) {
                    Remember::Entry { name, text } => match memory.write() {
                        Ok((dir, lines)) => {
                            for line in lines {
                                eprintln!("{line}");
                            }
                            match crate::memory::remember(&dir, &name, &text) {
                                (_, Some(write)) => {
                                    session.append(Event::Memory {
                                        name: write.name.clone(),
                                        kind: write.kind,
                                        text: write.text.clone(),
                                    })?;
                                    StderrReport::new(lang).memory(
                                        &write.name,
                                        write.kind,
                                        write.text.as_deref(),
                                    );
                                    said.output.push(format!("Remembered `{}`.", write.name));
                                }
                                (result, None) => said.fail(
                                    tr!(lang, m::REMEMBER_FAILED, reason = result.content)
                                        .to_string(),
                                ),
                            }
                        }
                        Err(error) => said.fail(
                            tr!(lang, m::REMEMBER_FAILED, reason = error.message(lang))
                                .to_string(),
                        ),
                    },
                    // The turn form was made a message above.
                    Remember::Usage | Remember::Turn(_) => {
                        said.fail(tr!(lang, m::REMEMBER_USAGE).to_string());
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
                                let placeholder = attach::add(&session, &mut pending, source);
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
                "/new" => {
                    said.record(&mut session, message);
                    said = Said::recorded();
                    session = Session::create(sessions_dir, function_name, &workspace)?;
                    pending.clear();
                    placeholders.clear();
                    state = None;
                    eprintln!("{}", tr!(lang, m::NEW_SESSION, id = session.id));
                }
                "/compact" => {
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
                        .compact(&mut StderrReport::new(lang))
                    });
                    let interrupted = matches!(result, Err(Error::Interrupted));
                    if let Some((text, role)) = compact_notice(result, function, lang) {
                        eprintln!("{text}");
                        said.failed = role != crate::theme::Role::Muted;
                        said.output.push(text);
                    }
                    if interrupted {
                        said.record(&mut session, message);
                        return Ok(ExitCode::from(interrupt::EXIT_CODE));
                    }
                }
                _ => said.fail(tr!(lang, m::UNKNOWN_COMMAND, name = name).to_string()),
            }
            said.record(&mut session, message);
            continue;
        }
        let mut report = StderrReport::new(lang);
        let text = attach::with_placeholders(&placeholders, message);
        placeholders.clear();
        let attached = std::mem::take(&mut pending);
        // The FSM of the turn sees the start too (AGL-046, D10).
        let mut observer = Observer::new();
        observer.attach(&session);
        // The server is started here when it does not answer, at the
        // first request (AGL-015, D10, AGL-024).
        let ready = autostart
            .ensure(&mut |progress| match progress {
                ServerProgress::Starting => {
                    observer.enter(PhaseEvent::ServerStart, &mut report);
                }
                ServerProgress::Line(line) => eprintln!("{line}"),
            })
            .and_then(|()| backend.context_size());
        let end = ready.and_then(|size| {
            context = Some(size);
            // The media the message names are stored; an audio is
            // transcribed here (AGL-014).
            attach::store(
                &mut session,
                attached,
                &text,
                backend,
                autostart,
                &mut |line| {
                    observer.enter(PhaseEvent::Transcribe, &mut report);
                    eprintln!("{line}");
                },
            )?;
            Agent {
                function,
                function_name,
                backend,
                stirred,
                session: &mut session,
                workspace: &workspace,
                context: size,
                forge,
                reasoning,
                image_tokens,
                whisper: autostart,
                search,
                memory,
            }
            .turn_observed(&text, &mut report, &mut observer)
        });
        // A start that failed never reached the agent.
        observer.end(PhaseEvent::Failed, &mut report);
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
struct Said {
    output: Vec<String>,
    failed: bool,
    recorded: bool,
}

impl Said {
    /// For a command already written to the session it leaves, as `/new`.
    fn recorded() -> Said {
        Said {
            recorded: true,
            ..Said::default()
        }
    }

    fn fail(&mut self, text: String) {
        eprintln!("{text}");
        self.output.push(text);
        self.failed = true;
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
