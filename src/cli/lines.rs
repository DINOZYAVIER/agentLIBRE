//! The session without a terminal (D128): every line of stdin is a
//! message; the answers go to stdout and the progress to stderr, as with
//! `-p` (D109).

use std::io::{self, BufRead};
use std::process::ExitCode;

use crate::Setup;
use crate::agent::{Agent, TurnEnd};
use crate::cli::report::StderrReport;
use crate::error::Error;
use crate::i18n::{m, repl as texts};
use crate::interrupt;
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
        context,
        server_url,
        lang,
        forge,
        ..
    } = setup;
    let workspace = session.workspace.clone();
    let facts = Facts {
        function: function_name.into(),
        workspace: display_path(&workspace),
        server: server_url.into(),
        context,
    };
    interrupt::install();
    eprintln!("{}", tr!(lang, m::SESSION, id = session.id));
    let mut state: Option<String> = None;
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
                    };
                    println!("{}", status_text(&facts, &info, lang));
                }
                "/new" => {
                    session = Session::create(sessions_dir, function_name, &workspace)?;
                    state = None;
                    eprintln!("{}", tr!(lang, m::NEW_SESSION, id = session.id));
                }
                "/compact" => {
                    let result = Agent {
                        function,
                        function_name,
                        backend,
                        session: &mut session,
                        workspace: &workspace,
                        context,
                        forge,
                        reasoning: None,
                    }
                    .compact(&mut StderrReport::new(lang));
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
        let end = Agent {
            function,
            function_name,
            backend,
            session: &mut session,
            workspace: &workspace,
            context,
            forge,
            reasoning: None,
        }
        .turn(message, &mut report);
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
                eprintln!("agentLIBRE: {}", texts::stopped(reason, function, lang));
            }
            Err(error) => eprintln!("agentLIBRE: {}", error.message(lang)),
        }
    }
    Ok(ExitCode::SUCCESS)
}
