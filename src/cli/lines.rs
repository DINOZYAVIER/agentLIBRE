//! The session without a terminal (D128): every line of stdin is a
//! message; the answers go to stdout and the progress to stderr, as with
//! `-p` (D109).

use std::io::{self, BufRead};
use std::process::ExitCode;

use crate::Setup;
use crate::agent::{Agent, TurnEnd};
use crate::attach::{self, Pending};
use crate::cli::report::StderrReport;
use crate::cli::serve::Others;
use crate::error::Error;
use crate::i18n::{m, repl as texts};
use crate::interrupt;
use crate::media::Source;
use crate::session::{Session, StopReason};
use crate::status::{Facts, Info, compact_notice, display_path, help_text, status_text};
use crate::tr;

pub fn run(setup: Setup) -> Result<ExitCode, Error> {
    let Setup {
        function,
        function_name,
        backend,
        mut session,
        sessions_dir,
        mut context,
        server_url,
        lang,
        forge,
        reasoning,
        autostart,
        yes,
        image_tokens,
        ..
    } = setup;
    let workspace = session.workspace.clone();
    let facts = Facts {
        function: function_name.into(),
        workspace: display_path(&workspace),
        server: server_url.into(),
    };
    // Nobody can be asked on stdin: another project's server running
    // is a refusal unless `--yes` (AGL-015, D13, D16).
    let others = if yes { Others::Yes } else { Others::Refuse };
    interrupt::install();
    eprintln!("{}", tr!(lang, m::SESSION, id = session.id));
    let mut state: Option<String> = None;
    // What `/attach` took since the last message (AGL-014, D18).
    let mut pending: Vec<Pending> = Vec::new();
    let mut placeholders: Vec<String> = Vec::new();
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
                "/help" => println!("{}", help_text(lang, false)),
                "/status" => {
                    let info = Info {
                        session: session.id.clone(),
                        state: state.clone(),
                        unfinished: session.unfinished().and_then(|turn| turn.state),
                        context,
                    };
                    println!("{}", status_text(&facts, &info, reasoning, lang));
                }
                "/attach" => {
                    let path = unquote(message["/attach".len()..].trim());
                    if path.is_empty() {
                        eprintln!("{}", tr!(lang, m::ATTACH_USAGE));
                    } else {
                        match Source::from_file(std::path::Path::new(path)) {
                            Ok(source) => {
                                let name = source.name.clone();
                                let placeholder = attach::add(&session, &mut pending, source);
                                eprintln!(
                                    "{}",
                                    tr!(lang, m::ATTACHED, placeholder = placeholder, name = name)
                                );
                                placeholders.push(placeholder);
                            }
                            Err(error) => eprintln!("agl: {}", error.message(lang)),
                        }
                    }
                }
                "/new" => {
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
                            session: &mut session,
                            workspace: &workspace,
                            context,
                            forge,
                            reasoning,
                            image_tokens,
                            whisper: autostart,
                        }
                        .compact(&mut StderrReport::new(lang))
                    });
                    let interrupted = matches!(result, Err(Error::Interrupted));
                    if let Some((text, _)) = compact_notice(result, function, lang) {
                        eprintln!("{text}");
                    }
                    if interrupted {
                        return Ok(ExitCode::from(interrupt::EXIT_CODE));
                    }
                }
                _ => eprintln!("{}", tr!(lang, m::UNKNOWN_COMMAND, name = name)),
            }
            continue;
        }
        let mut report = StderrReport::new(lang);
        let text = attach::with_placeholders(&placeholders, message);
        placeholders.clear();
        let attached = std::mem::take(&mut pending);
        // The server is started here when it does not answer, at the
        // first request (AGL-015, D10).
        let ready = autostart
            .ensure(&others, &mut |line| eprintln!("{line}"))
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
                &mut |line| eprintln!("{line}"),
            )?;
            Agent {
                function,
                function_name,
                backend,
                session: &mut session,
                workspace: &workspace,
                context: size,
                forge,
                reasoning,
                image_tokens,
                whisper: autostart,
            }
            .turn(&text, &mut report)
        });
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
