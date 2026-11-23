//! `agl server start|stop|status|command`: the commands on the engine that
//! keeps the project's server (AGL-067, F4).

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

use crate::backend::Backend;
use crate::backend::llama::Llama;
use crate::cli::args::ServerAction;
use crate::ask::ask_terminal;
use crate::engine::*;
use crate::error::Error;
use crate::engine::error::Error as EngineError;
use crate::i18n::server as texts;
use agentlibre_common::i18n::{Lang, m};
use agentlibre_common::settings::{ServerFile, ServerKind};
use crate::outln;
use agentlibre_common::tr;

/// Runs a server command. `models` is the models directory.
pub fn run(
    action: ServerAction,
    server: &Server,
    models: &Path,
    path_var: Option<OsString>,
    yes: bool,
    lang: Lang,
) -> Result<ExitCode, Error> {
    let effective = server.effective_settings()?;
    let llama = Llama::with_ca(&server.url, effective.private_ca())?;
    let external = effective.external();
    match action {
        // `lib` prints it before anything is resolved.
        ServerAction::Command => Ok(ExitCode::SUCCESS),
        // A server outside is not started, only looked at (AGL-052, D4).
        ServerAction::Start if external => {
            let what = server.what()?;
            check_outside(&llama, &server.effective_settings()?, &what, &mut |line| outln!("{line}"), lang)?;
            outln!("{}", texts::already_running(&what, &server.url, lang));
            Ok(ExitCode::SUCCESS)
        }
        ServerAction::Start => {
            let others = if yes {
                Others::Yes
            } else if agentlibre_common::tty::is_interactive() {
                Others::Ask(std::sync::Arc::new(ask_terminal))
            } else {
                Others::Refuse
            };
            start(
                server,
                models,
                path_var,
                &llama,
                &others,
                &mut |line| outln!("{line}"),
                lang,
            )
            .map(|()| ExitCode::SUCCESS)
            .map_err(Into::into)
        }
        ServerAction::Stop => {
            // The whisper server of the project goes with it (AGL-014);
            // nothing is said of it when it did not run.
            let whisper = unit_active(&server.whisper_unit)?;
            if whisper {
                systemctl(&["stop", &server.whisper_unit])?;
            }
            if external {
                outln!("{}", tr!(lang, m::SERVER_OUTSIDE_NOT_STOPPED, url = server.url.as_str()));
            } else if unit_active(&server.unit)? {
                // The server by its kind (AGL-038, D1): what the record
                // says it runs, else the server of the project.
                let what = read_record(&server.dir)
                    .and_then(|record| what_of(&record).ok())
                    .unwrap_or_else(|| tr!(lang, m::SERVER_OF_THE_PROJECT).to_owned());
                let pid = unit_main_pid(&server.unit);
                systemctl(&["stop", &server.unit])?;
                remove_record(&server.dir, server.runtime.as_deref(), pid);
                outln!("{}", tr!(lang, m::SERVER_SWITCH_STOPPED, old = what));
            } else if !whisper {
                outln!("{}", texts::not_running(&server.unit, lang));
            }
            if whisper {
                outln!("{}", tr!(lang, m::WHISPER_STOPPED));
            }
            Ok(ExitCode::SUCCESS)
        }
        ServerAction::Status => {
            let active = unit_active(&server.unit)?;
            let health = health(&llama);
            // The actually-running model (AGL-033, D5): both kinds of
            // server expose /v1/models; when the server does not answer
            // there is none.
            let running = (health == Health::Ready)
                .then(|| llama.models().unwrap_or(None))
                .flatten()
                .map(|name| if external { file_name(&name).to_owned() } else { name });
            // The model of the effective settings, as the server names
            // it (AGL-033, D5): the same identity the check of D2
            // compares — the full path of the llama kind, the config's
            // model_name of the strata kind.
            let model = expected_model(&server.effective_settings()?, models)?;
            // A server on Windows names the same file by its Windows path (AGL-056).
            let running = running.map(|name| match &model {
                Some(model) if same_model(&name, model) => model.clone(),
                _ => name,
            });
            outln!(
                "{}",
                texts::status(
                    &server.unit,
                    active,
                    &server.url,
                    model.as_deref(),
                    running.as_deref(),
                    health,
                    lang
                )
            );
            Ok(if health == Health::Ready {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
    }
}

/// `agl server command` (AGL-052, D12): the line that starts the server of
/// the Function `name`, with the files by their names, and the links to them.
/// It reads no server. An address that is not local is reached from another
/// machine, so the server listens on all its addresses.
pub fn print_command(
    name: &str,
    function: &crate::function::Function,
    settings: &ServerFile,
) -> Result<ExitCode, Error> {
    let model = &function.model;
    if model.kind() != ServerKind::Llama {
        return Err(EngineError::ServerCommandNotLlama { function: name.to_owned() }.into());
    }
    outln!("{}", command_line(model, settings));
    for source in &function.sources {
        outln!("{}  {}", source.name, source.url);
    }
    Ok(ExitCode::SUCCESS)
}
