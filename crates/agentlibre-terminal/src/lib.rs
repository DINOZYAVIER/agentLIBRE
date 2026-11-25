//! agentLIBRE: free and open-source agentic system.

mod cli;
mod error;

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
    session,
    weights,
    wsl,
};
// The media a listed session names (AGL-014), in the session listing's tests.
#[cfg(test)]
pub(crate) use agentlibre_core::media;
pub(crate) use agentlibre_forge::{forge, memory};

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

use cli::args::Command;
use error::{CoreError, Error};
use function::Function;
use agentlibre_common::i18n::{Lang, m};
use agentlibre_common::tr;
use paths::Paths;
use session::Session;
use settings::{function_name, ReplSettings, UserFile};

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
                    weights::Missing::Ask(&agentlibre_common::tty::ask_line, &|line| eprintln!("{line}"))
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
            if let Err(error) = agentlibre_daemon::serve(paths) {
                if let agentlibre_daemon::Error::Running { path } = error {
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
    // uses it (AGL-076, D16); the sessions are the repl crate's (AGL-131, D4).
    let rev = env(about::REV_VAR).and_then(|rev| rev.into_string().ok());
    agentlibre_repl::run(agentlibre_repl::Options {
        paths,
        repl: ReplSettings::of(&user),
        colors: user.colors,
        cwd,
        lang: *lang,
        version: about::version(rev.as_deref()),
        args: agentlibre_repl::Args {
            prompt: args.prompt.clone(),
            resume: args.resume.clone(),
            function: args.function.clone(),
            plane: args.plane.clone(),
            reasoning: args.reasoning,
            yes: args.yes,
            attach: args.attach.clone(),
        },
    })
    .map_err(Error::from)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

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
