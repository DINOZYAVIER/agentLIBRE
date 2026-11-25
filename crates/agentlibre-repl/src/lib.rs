//! The terminal's sessions (AGL-131, D4): the REPL, the stdin session and
//! `-p`, opened through the client, drawn here, served by the daemon
//! (AGL-076, D5). The command line of `agl` is the terminal crate, not here.

mod error;
mod interrupt;
mod lines;
mod remote;
mod repl;
mod report;
mod theme;

pub use error::Error;

pub(crate) use agentlibre_common::{settings, tty};

// What the tests of the surfaces drive: the run's own reports, phases and
// tools (AGL-130, D12), kept off the crate's link of its own (AGL-131, D4).
#[cfg(test)]
pub(crate) use agentlibre_core::{
    action, agent, backend, engine, function, media, phase, plane, session, tools, whisper,
};
#[cfg(test)]
pub(crate) use agentlibre_forge::memory;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use agentlibre_common::i18n::{Lang, m};
use agentlibre_common::outln;
use agentlibre_common::reasoning::ReasoningLevel;
use agentlibre_common::tr;
use agentlibre_proto::{self as proto, TurnEnd};
use agentlibre_client::Remote;
use agentlibre_common::paths::Paths;
use settings::ReplSettings;

use error::Error as ReplError;
use report::StderrReport;
use theme::Theme;

/// Everything a session needs: its plane in the daemon (AGL-076, D5), and
/// what the terminal draws with.
pub struct Setup {
    pub remote: Arc<Remote>,
    pub opened: proto::Opened,
    pub lang: Lang,
    pub theme: Theme,
    pub repl: ReplSettings,
    pub history_file: PathBuf,
    /// The Functions of the REPL's `/function` (AGL-036, D3) and the planes
    /// of its `/plane` (AGL-077, D12), by the daemon (AGL-126, D2).
    pub stores: Arc<agentlibre_surface::daemon::Stores>,
    /// agentLIBRE's directories, for the settings of Ctrl-F (AGL-109, D6) and
    /// the daemon's socket.
    pub paths: Paths,
    /// The REPL's voice, by the daemon (AGL-076, D15).
    pub voice: Arc<dyn agentlibre_surface::dictation::Dictation>,
    /// What `agl --version` says, for the splash (AGL-131, D4).
    pub version: String,
}

/// What the caller asks of a session (AGL-131, D4): the parsed part of
/// `agl` that chooses the session; reading the disk for the rest is the
/// terminal's.
#[derive(Clone, Default)]
pub struct Args {
    /// `-p <message>`: answer one message and exit; REPL otherwise.
    pub prompt: Option<String>,
    /// `--resume <id>`, or `last`.
    pub resume: Option<String>,
    /// `--function <name>`.
    pub function: Option<String>,
    /// `--plane <name>`: the plane of the run (AGL-077, D6).
    pub plane: Option<String>,
    /// `--think <level>`: the model's reasoning level (AGL-013).
    pub reasoning: Option<ReasoningLevel>,
    /// `--yes`: start the server without asking (AGL-015, D13, D16).
    pub yes: bool,
    /// `--attach <path>`, repeatable (AGL-014, D21).
    pub attach: Vec<String>,
}

/// Everything a run of the sessions needs (AGL-131, D4).
pub struct Options {
    pub paths: Paths,
    /// The user file's `colors`, for the theme.
    pub colors: Vec<(agentlibre_common::style::Role, agentlibre_common::style::Style)>,
    pub repl: ReplSettings,
    pub cwd: PathBuf,
    pub lang: Lang,
    /// What `agl --version` says, for the splash.
    pub version: String,
    pub args: Args,
}

/// The sessions of `agl`: one after another until one ends the process,
/// `/function` and `/plane` beginning the next with their choice kept
/// (AGL-036, D4; AGL-077, D12).
pub fn run(options: Options) -> Result<ExitCode, ReplError> {
    let mut args = options.args.clone();
    loop {
        match agent_session(&options, &args)? {
            repl::End::Exit(code) => return Ok(code),
            // `/function <name>` (AGL-036, D4): a new session with that
            // Function, the reasoning level kept.
            // `/plane <name>` (AGL-077, D12): a new session in that plane,
            // the reasoning level kept.
            repl::End::Plane { name, reasoning } => {
                args = Args { plane: Some(name), reasoning, yes: args.yes, ..Args::default() };
            }
            repl::End::Function { name, reasoning, plane } => {
                args = Args {
                    function: Some(name),
                    plane,
                    reasoning,
                    yes: args.yes,
                    ..Args::default()
                };
            }
        }
    }
}

/// One session of the agent command: `-p`, the interactive session or the
/// stdin session, with the Function of `args` (AGL-036, D4: the
/// interactive session may end in a switch to another Function), its
/// plane in the daemon (AGL-076, D5).
fn agent_session(options: &Options, args: &Args) -> Result<repl::End, ReplError> {
    // The files of `--attach` are read before anything is started.
    let sources = args
        .attach
        .iter()
        .map(|path| agentlibre_common::media::Source::from_file(Path::new(path)))
        .collect::<Result<Vec<_>, _>>()?;
    let interactive = tty::is_interactive();
    let (remote, opened) = remote::open(
        &options.paths,
        remote::Opening {
            cwd: &options.cwd,
            plane: args.plane.as_deref(),
            resume: args.resume.as_deref(),
            function: args.function.as_deref(),
            reasoning: args.reasoning,
            yes: args.yes,
            one_request: args.prompt.is_some(),
            // The server comes up as the REPL starts (AGL-046).
            start_server: args.prompt.is_none() && interactive && options.repl.start_server,
        },
        options.lang,
    )?;
    let Some(message) = args.prompt.clone() else {
        // No `-p`: the interactive session (D136).
        let plane = opened.session.plane.clone();
        let stores = Arc::new(agentlibre_surface::daemon::Stores {
            paths: options.paths.clone(),
            lang: options.lang,
            dir: opened.workspace.clone(),
            plane: plane.clone(),
            function: opened.function.clone(),
        });
        let voice = Arc::new(agentlibre_surface::dictation::RemoteVoice {
            paths: options.paths.clone(),
            dir: opened.workspace.clone(),
            plane,
            lang: options.lang,
        });
        let setup = Setup {
            remote: Arc::clone(&remote),
            opened,
            lang: options.lang,
            theme: Theme::new(
                interactive && theme::colors_enabled(|name| std::env::var_os(name)),
                &options.colors,
            ),
            repl: options.repl,
            history_file: options.paths.history_file(),
            stores,
            paths: options.paths.clone(),
            voice,
            version: options.version.clone(),
        };
        let end = if interactive {
            repl::run(setup)
        } else {
            lines::run(setup).map(repl::End::Exit)
        };
        remote.close();
        return end;
    };
    let id = opened.session.id.clone();
    eprintln!("{}", tr!(options.lang, m::SESSION, id = id));
    interrupt::install();
    remote::forward_stop(&remote);
    // The attached files get their placeholders before the message and are
    // stored with the session; an audio is transcribed there (AGL-014).
    let mut pending = Vec::new();
    let placeholders: Vec<String> = sources
        .into_iter()
        .map(|source| agentlibre_surface::attach::add_counted(&opened.session.media, &mut pending, source))
        .collect();
    let message = agentlibre_surface::attach::with_placeholders(&placeholders, &message);
    // The turn runs in the daemon (AGL-076, D5); what it tells is said here.
    let mut report = StderrReport::new(options.lang);
    let done = remote
        .ask(
            agentlibre_client::Command::Say {
                message,
                attached: pending.iter().map(agentlibre_surface::attach::attached).collect(),
                reasoning: args.reasoning.map(|level| level.name().to_owned()),
            },
            &mut |sent| agentlibre_client::give(sent, &mut report),
        )?;
    remote.close();
    let turn = done
        .turn
        .expect("a message's turn")
        .map_err(|text| agentlibre_client::Error::Said { text })?;
    match turn {
        TurnEnd::Answer(text) => {
            outln!("{text}");
            Ok(repl::End::Exit(ExitCode::SUCCESS))
        }
        TurnEnd::Stopped(reason) => {
            let why = done.why.unwrap_or_default();
            // Why a turn stopped, and how to go on (D113).
            eprintln!(
                "agl: {}\n{}",
                why,
                tr!(options.lang, m::STOPPED_RESUME, id = id)
            );
            Ok(repl::End::Exit(ExitCode::from(match reason {
                proto::StopReason::Interrupted => interrupt::EXIT_CODE,
                _ => 1,
            })))
        }
    }
}
