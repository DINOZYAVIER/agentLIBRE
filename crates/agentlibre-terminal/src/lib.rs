//! agentLIBRE: free and open-source agentic system.

mod ask;
mod cli;
mod error;
mod interrupt;
mod remote;
mod repl;
mod theme;

// The core's modules under the names the terminal's code always used.
pub(crate) use agentlibre_common::{paths, settings, tty};
pub(crate) use agentlibre_core::{
    about,
    action,
    backend,
    engine,
    function,
    i18n,
    plane,
    record,
    records,
    session,
    status,
    stop,
    weights,
    whisper,
    wsl,
};
// What the tests of the surfaces drive: the run's own reports, phases and
// tools (AGL-130, D12).
#[cfg(test)]
pub(crate) use agentlibre_core::{agent, media, phase, tools};
pub(crate) use agentlibre_forge::{forge, memory};

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use cli::args::Command;
use cli::report::StderrReport;
use error::{CoreError, Error};
use function::Function;
use agentlibre_common::i18n::{Lang, m};
use agentlibre_common::tr;
use paths::Paths;
use agentlibre_client::Remote;
use agentlibre_proto as proto;
use proto::{StopReason, TurnEnd};
use session::Session;
use settings::{function_name, ReplSettings, UserFile};
use theme::Theme;

/// The auto-init of the Run path (AGL-026, D2): the workspace the cwd
/// resolved to is materialized when its marker is missing — the same
/// code `agl init` runs — so the store (the engine log, the forge, the
/// memory, the project settings) exists from the first turn, and the
/// first message in an unregistered directory launches the engine
/// instead of dying on the engine log's missing parent directory. The
/// marker's line goes to stderr; the commands keep their own behavior.
fn auto_init(project: &forge::Workspace, lang: Lang) -> Result<(), Error> {
    if project.dir.join(forge::MARKER_FILE).is_file() {
        return Ok(());
    }
    for line in forge::materialize(project, lang)? {
        eprintln!("{line}");
    }
    Ok(())
}

/// How many sessions `agl --resume` lists (AGL-058, D20).
const SESSIONS_LISTED: usize = 20;

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
    pub stores: Arc<remote::Stores>,
    /// agentLIBRE's directories, for the settings of Ctrl-F (AGL-109, D6) and
    /// the daemon's socket.
    pub paths: Paths,
    /// The REPL's voice, by the daemon (AGL-076, D15).
    pub voice: Arc<dyn whisper::Dictation>,
}

pub fn cli() -> ExitCode {
    let env = |name: &str| std::env::var_os(name);
    let mut lang = Lang::from_locale(env);
    match run(env, std::env::args_os().skip(1), &mut lang) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("agl: {}", error.message(lang));
            ExitCode::from(error.exit_code())
        }
    }
}

/// Runs `agl`. Switches `lang` to the user file's `language` as soon
/// as that file loads, so later errors and `--help` use it (D66).
fn run(
    env: impl Fn(&str) -> Option<OsString>,
    args: impl IntoIterator<Item = OsString>,
    lang: &mut Lang,
) -> Result<ExitCode, Error> {
    let loaded = Paths::from_env(&env).map_err(Error::from).and_then(|paths| {
        let user = UserFile::load(&paths.settings_file())?;
        Ok((paths, user))
    });
    if let Ok((_, user)) = &loaded
        && let Some(language) = user.language
    {
        *lang = language;
    }
    // A command on a plane's store (AGL-077, D11).
    let (command, plane) = match cli::args::parse(args)? {
        Command::InPlane { plane, command } => (*command, Some(plane)),
        command => (command, None),
    };
    let plane = plane.as_deref();
    let args = match command {
        Command::Version => {
            let rev = env(about::REV_VAR).and_then(|rev| rev.into_string().ok());
            let _ = writeln!(io::stdout(), "{}", about::version(rev.as_deref()));
            return Ok(ExitCode::SUCCESS);
        }
        Command::Help(topic) => {
            // A closed pipe, as in `agl --help | head -1`, is not an error.
            let _ = writeln!(io::stdout(), "{}", cli::help::text(*lang, topic));
            return Ok(ExitCode::SUCCESS);
        }
        Command::Run(args) => args,
        Command::Server(action, yes, function_option) => {
            let (paths, user) = loaded?;
            let cwd = std::env::current_dir().map_err(Error::CurrentDir)?;
            let project = cli::forge::project_of(
                &paths,
                &cwd,
                user.forge.auto_rehome.unwrap_or(true),
                *lang,
            )?;
            let settings = project.server.over(&user.server);
            // The model of the Function is the model of the server
            // (AGL-039, D21): the command works on `--function`, else the
            // project's function (AGL-035, D3).
            let name = function_name(
                function_option.as_deref(),
                project.function.as_deref(),
                user.function.as_deref(),
            );
            let function = Function::load(&[&paths.functions_dir(&project.key), &paths.functions()], &name)?;
            if action == cli::args::ServerAction::Command {
                return cli::serve::print_command(&name, &function, &settings);
            }
            // The weights: `start` asks for a file it does not find, as a
            // run does; `status` and `stop` keep the name (AGL-039, D24).
            let mut model = function.model.clone();
            model.llama_external = settings.llama_external;
            model.llama_private_ca = settings.llama_private_ca.clone();
            model.strata_private_ca = settings.strata_private_ca.clone();
            let missing = match action {
                cli::args::ServerAction::Start if tty::is_interactive() => {
                    weights::Missing::Ask(&ask::ask_line, &|line| eprintln!("{line}"))
                }
                cli::args::ServerAction::Start => weights::Missing::Fail,
                _ => weights::Missing::Keep,
            };
            let models = wsl::own_models(paths.models())?;
            weights::resolve(
                &mut model,
                &mut settings::WhisperFile::default(),
                &weights::Folders {
                    own: models.clone(),
                    listed: &user.models,
                    user_path: &paths.settings_file(),
                },
                &function.sources,
                &missing,
                *lang,
            )?;
            let server = engine::Server::of(&project, paths.runtime.clone(), &settings, model)?;
            return cli::serve::run(action, &server, &models, env("PATH"), yes, *lang);
        }
        Command::Init => {
            let (paths, user) = loaded?;
            let cwd = std::env::current_dir().map_err(Error::CurrentDir)?;
            return cli::init::run(&user, &paths, &cwd, *lang);
        }
        // Parsed before the settings load (AGL-061, D1), it runs after
        // them, so `[forge]` and `language` apply.
        Command::Forge(action) => {
            let (paths, user) = loaded?;
            let cwd = std::env::current_dir().map_err(Error::CurrentDir)?;
            return cli::forge::run(action, plane, &user, &paths, &cwd, *lang);
        }
        Command::Function(action) => {
            let (paths, user) = loaded?;
            let cwd = std::env::current_dir().map_err(Error::CurrentDir)?;
            return cli::function::run(action, plane, &user, &paths, &cwd, *lang);
        }
        Command::Search(action) => {
            let (paths, user) = loaded?;
            return cli::search::run(action, &user, &paths, *lang);
        }
        Command::Memory(action) => {
            let (paths, user) = loaded?;
            let cwd = std::env::current_dir().map_err(Error::CurrentDir)?;
            return cli::memory::run(action, plane, &user, &paths, &cwd, *lang);
        }
        Command::Plane(action) => {
            let (paths, _) = loaded?;
            let cwd = std::env::current_dir().map_err(Error::CurrentDir)?;
            return cli::plane::run(action, &paths, &cwd, *lang);
        }
        Command::Session(show) => {
            let (paths, user) = loaded?;
            let cwd = std::env::current_dir().map_err(Error::CurrentDir)?;
            return cli::session::run(show, plane, &user, &paths, &cwd, *lang);
        }
        Command::Send { session, message } => {
            let (paths, _) = loaded?;
            let cwd = std::env::current_dir().map_err(Error::CurrentDir)?;
            return cli::send::run(&session, &message, &paths, &cwd, *lang);
        }
        // The launcher starts `agl-notify` beside itself; it needs no paths (D23).
        Command::Notify => return cli::notify::run(*lang),
        Command::Bridge(action) => {
            let (paths, user) = loaded?;
            return cli::bridge::run(action, &user, &paths, *lang);
        }
        Command::Daemon => {
            let (paths, _) = loaded?;
            // A daemon already answers: agl is served, not broken — it says
            // so and exits 0 (AGL-120, D4), so a unit and a forked process
            // that lose the race for the socket leave no failed unit behind.
            if let Err(error) = agentlibre_core::daemon::serve(paths) {
                if let CoreError::DaemonServer(agentlibre_core::daemon::error::Error::Running { path }) = error {
                    println!("{}", tr!(*lang, m::DAEMON_SERVED, path = path.display()));
                    return Ok(ExitCode::SUCCESS);
                } else {
                    return Err(error.into());
                }
            }
            return Ok(ExitCode::SUCCESS);
        }
        Command::InPlane { .. } => unreachable!("taken apart above"),
    };
    let (paths, user) = loaded?;
    let cwd = std::env::current_dir().map_err(Error::CurrentDir)?;
    // The project settings come from the store of the project (AGL-010).
    let project = cli::forge::project_of(
        &paths,
        &cwd,
        user.forge.auto_rehome.unwrap_or(true),
        *lang,
    )?;
    // `agl --resume` with no id lists the sessions (AGL-058, D20).
    if args.list_sessions {
        let found = Session::latest(&paths.sessions(), &project.project, SESSIONS_LISTED);
        if found.is_empty() {
            let _ = writeln!(io::stdout(), "{}", tr!(*lang, m::NO_SESSIONS));
        }
        for session in found {
            let _ = writeln!(io::stdout(), "{}  {}  {}", session.id, session.time, session.label());
        }
        return Ok(ExitCode::SUCCESS);
    }
    auto_init(&project, *lang)?;
    if args.resume.is_some() && args.function.is_some() {
        return Err(CoreError::FunctionWithResume.into());
    }
    // The engine agl launched is the daemon's now, stopped when no plane
    // uses it (AGL-076, D16).
    let mut args = args;
    loop {
        match agent_session(&env, &paths, &user, &cwd, &args, *lang)? {
            repl::End::Exit(code) => return Ok(code),
            // `/function <name>` (AGL-036, D4): a new session with that
            // Function, the reasoning level kept.
            // `/plane <name>` (AGL-077, D12): a new session in that plane,
            // the reasoning level kept.
            repl::End::Plane { name, reasoning } => {
                args = cli::args::Args {
                    plane: Some(name),
                    reasoning,
                    yes: args.yes,
                    ..cli::args::Args::default()
                };
            }
            repl::End::Function {
                name,
                reasoning,
                plane,
            } => {
                args = cli::args::Args {
                    function: Some(name),
                    plane,
                    reasoning,
                    yes: args.yes,
                    ..cli::args::Args::default()
                };
            }
        }
    }
}

/// One session of the agent command: `-p`, the interactive session or the
/// stdin session, with the Function of `args` (AGL-036, D4: the
/// interactive session may end in a switch to another Function), its
/// plane in the daemon (AGL-076, D5).
fn agent_session(
    env: &impl Fn(&str) -> Option<OsString>,
    paths: &Paths,
    user: &UserFile,
    cwd: &Path,
    args: &cli::args::Args,
    lang: Lang,
) -> Result<repl::End, Error> {
    // The files of `--attach` are read before anything is started.
    let sources = args
        .attach
        .iter()
        .map(|path| agentlibre_common::media::Source::from_file(Path::new(path)))
        .collect::<Result<Vec<_>, _>>()?;
    let interactive = tty::is_interactive();
    let repl_settings = ReplSettings::of(user);
    let (remote, opened) = remote::open(
        paths,
        remote::Opening {
            cwd,
            plane: args.plane.as_deref(),
            resume: args.resume.as_deref(),
            function: args.function.as_deref(),
            reasoning: args.reasoning,
            yes: args.yes,
            one_request: args.prompt.is_some(),
            // The server comes up as the REPL starts (AGL-046).
            start_server: args.prompt.is_none() && interactive && repl_settings.start_server,
        },
        lang,
    )?;
    let Some(message) = args.prompt.clone() else {
        // No `-p`: the interactive session (D136).
        let plane = opened.session.plane.clone();
        let stores = Arc::new(remote::Stores {
            paths: paths.clone(),
            lang,
            dir: opened.workspace.clone(),
            plane: plane.clone(),
            function: opened.function.clone(),
        });
        let voice = Arc::new(remote::RemoteVoice {
            paths: paths.clone(),
            dir: opened.workspace.clone(),
            plane,
            lang,
        });
        let setup = Setup {
            remote: Arc::clone(&remote),
            opened,
            lang,
            theme: Theme::new(interactive && theme::colors_enabled(env), &user.colors),
            repl: repl_settings,
            history_file: paths.history_file(),
            stores,
            paths: paths.clone(),
            voice,
        };
        let end = if interactive {
            repl::run(setup)
        } else {
            cli::lines::run(setup).map(repl::End::Exit)
        };
        remote.close();
        return end;
    };
    let id = opened.session.id.clone();
    eprintln!("{}", tr!(lang, m::SESSION, id = id));
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
    let mut report = StderrReport::new(lang);
    let done = remote
        .ask(
            agentlibre_client::Command::Say {
                message,
                attached: pending.iter().map(agentlibre_surface::attach::attached).collect(),
                reasoning: args.reasoning.map(|level| level.name().to_owned()),
            },
            &mut |sent| agentlibre_client::give(sent, &mut report),
        )
        .map_err(|error| Error::from(CoreError::from(error)))?;
    remote.close();
    let turn = done
        .turn
        .expect("a message's turn")
        .map_err(|text| Error::from(CoreError::Said { text }))?;
    match turn {
        TurnEnd::Answer(text) => {
            outln!("{text}");
            Ok(repl::End::Exit(ExitCode::SUCCESS))
        }
        TurnEnd::Stopped(reason) => {
            let why = done.why.unwrap_or_default();
            eprintln!("agl: {}", i18n::stopped_why(&why, &id, lang));
            Ok(repl::End::Exit(ExitCode::from(match reason {
                StopReason::Interrupted => interrupt::EXIT_CODE,
                _ => 1,
            })))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentlibre_common::testutil::TempDir;

    /// The workspace of a fresh, unregistered `cwd` under a fresh `data`
    /// (nothing stored there, so no choice is asked).
    fn fresh(data: &Path, cwd: &Path) -> forge::Workspace {
        let (ws, lines) = forge::resolve(
            data,
            cwd,
            Lang::En,
            true,
            &|options| Err(crate::forge::error::Error::RehomeNoStore {
                stores: options.to_vec(),
            }
            .into()),
        )
        .unwrap();
        assert!(lines.is_empty());
        ws
    }

    #[test]
    fn the_auto_init_materializes_a_fresh_workspace() {
        let dir = TempDir::new();
        let data = dir.0.join("data");
        let cwd = dir.0.join("project");
        std::fs::create_dir(&cwd).unwrap();
        let ws = fresh(&data, &cwd);
        let marker = ws.dir.join(forge::MARKER_FILE);
        assert!(!marker.is_file());
        auto_init(&ws, Lang::En).unwrap();
        assert!(marker.is_file());
        let text = std::fs::read_to_string(&marker).unwrap();
        assert!(text.contains(&ws.identity));
    }

    #[test]
    fn the_auto_init_is_a_no_op_on_a_registered_workspace() {
        let dir = TempDir::new();
        let data = dir.0.join("data");
        let cwd = dir.0.join("project");
        std::fs::create_dir(&cwd).unwrap();
        let ws = fresh(&data, &cwd);
        auto_init(&ws, Lang::En).unwrap();
        let marker = ws.dir.join(forge::MARKER_FILE);
        let before = std::fs::read(&marker).unwrap();
        // A second run changes nothing.
        auto_init(&ws, Lang::En).unwrap();
        assert_eq!(std::fs::read(&marker).unwrap(), before);
        // The store holds only the marker.
        let entries: Vec<String> = std::fs::read_dir(&ws.dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec![forge::MARKER_FILE.to_string()]);
    }
}
