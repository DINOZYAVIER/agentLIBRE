//! agentLIBRE: free and open-source agentic system.

mod cli;
mod interrupt;
mod repl;
mod theme;

// The core's modules under the names the terminal's code always used.
pub(crate) use agentlibre_core::{
    about,
    action,
    agent,
    attach,
    backend,
    control,
    engine,
    error,
    forge,
    function,
    i18n,
    media,
    memory,
    nix,
    paths,
    phase,
    plane,
    programs,
    record,
    records,
    session,
    settings,
    status,
    stop,
    style,
    tools,
    tty,
    weights,
    whisper,
    wsl,
};
pub(crate) use agentlibre_core::tr;

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use agent::TurnEnd;
use cli::args::Command;
use cli::report::StderrReport;
use error::Error;
use function::Function;
use i18n::{Lang, m};
use paths::Paths;
use plane::Plane;
use session::{Session, StopReason};
use settings::{function_name, ReplSettings, UserFile};
use theme::Theme;

/// Closes the engine agl launched itself (AGL-024, D3): when the guard
/// drops, the child in the field is shut down. It lives to the end of the
/// agent command, so every return — normal exit, an error, an interrupted
/// turn — closes the engine; a pre-existing server is not in the field and
/// keeps running.
struct EngineGuard {
    engine: Arc<Mutex<Option<engine::Engine>>>,
    /// The project's store directory, where the record of the launched
    /// server is (AGL-035, D1).
    dir: PathBuf,
    /// The runtime directory, where the file of the server is
    /// (AGL-064, D1, D3).
    runtime: Option<PathBuf>,
}

impl Drop for EngineGuard {
    fn drop(&mut self) {
        if let Some(engine) = self.engine.lock().unwrap().take() {
            let pid = engine.pid();
            engine.shutdown();
            engine::remove_record(&self.dir, self.runtime.as_deref(), Some(pid));
        }
    }
}

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

/// Everything a session needs: the plane it runs on, and what the terminal
/// draws with.
pub struct Setup<'a> {
    pub plane: Plane,
    /// Whether the session existed before this run (D126).
    pub resumed: bool,
    pub server_url: &'a str,
    pub theme: Theme,
    pub repl: ReplSettings,
    pub history_file: PathBuf,
    /// The Functions of the REPL's `/function` (AGL-036, D3).
    pub catalog: &'a function::catalog::Catalog,
    /// agentLIBRE's runtime directory, where the REPL's control socket is
    /// made (AGL-074, D3).
    pub runtime: Option<PathBuf>,
    /// agentLIBRE's directories, for the planes of `/plane` (AGL-077, D12).
    pub paths: Paths,
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
    let loaded = Paths::from_env(&env).and_then(|paths| {
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
            let missing = match action {
                cli::args::ServerAction::Start if tty::is_interactive() => {
                    weights::Missing::Ask(&engine::ask_line, &|line| eprintln!("{line}"))
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
        Command::Notify => {
            let (paths, _) = loaded?;
            return cli::notify::run(&paths, *lang);
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
        return Err(Error::FunctionWithResume);
    }
    // The engine agl launched (when it did) is shut down when this guard
    // drops, at the end of the command: normal exit, an error, an
    // interrupted turn (AGL-024, D3). The hard `_exit` of a second Ctrl-C
    // runs no Rust code and leaves the engine running, the accepted
    // consequence recorded in the spec. It outlives the sessions of a
    // `/function` switch, so the switch of D10 (AGL-036) can stop it.
    let engine = Arc::new(Mutex::new(None));
    let _engine_guard = EngineGuard {
        engine: Arc::clone(&engine),
        dir: project.dir.clone(),
        runtime: paths.runtime.clone(),
    };
    let mut args = args;
    loop {
        match agent_session(&env, &paths, &user, &cwd, &project, &args, &engine, *lang)? {
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
/// interactive session may end in a switch to another Function).
#[allow(clippy::too_many_arguments)]
fn agent_session(
    env: &impl Fn(&str) -> Option<OsString>,
    paths: &Paths,
    user: &UserFile,
    cwd: &Path,
    project: &forge::Workspace,
    args: &cli::args::Args,
    engine: &Arc<Mutex<Option<engine::Engine>>>,
    lang: Lang,
) -> Result<repl::End, Error> {
    // The files of `--attach` are read before anything is started.
    let sources = args
        .attach
        .iter()
        .map(|path| media::Source::from_file(Path::new(path)))
        .collect::<Result<Vec<_>, _>>()?;
    let interactive = tty::is_interactive();
    let opened = Plane::open(plane::Opening {
        env,
        paths,
        user,
        cwd,
        project,
        resume: args.resume.as_deref(),
        function: args.function.as_deref(),
        plane: args.plane.as_deref(),
        reasoning: args.reasoning,
        one_request: args.prompt.is_some(),
        engine: Arc::clone(engine),
        lang,
        stop: interrupt::stop().clone(),
        // The terminal asks the human (AGL-075, D8): a missing weights file
        // in a terminal (AGL-039, D22-D24), and the restart's consent as
        // the server commands do (AGL-033, D2) — `--yes` skips, the
        // terminal asks, a non-interactive terminal refuses.
        asker: plane::Asker {
            say: Arc::new(|line| eprintln!("{line}")),
            line: interactive.then(|| Arc::new(engine::ask_line) as plane::AskLine),
            others: if args.yes {
                engine::Others::Yes
            } else if interactive {
                engine::Others::Ask(Arc::new(engine::ask_terminal))
            } else {
                engine::Others::Refuse
            },
        },
    })?;
    let plane::Opened {
        plane,
        resumed,
        settings,
        mut observer,
    } = opened;
    let Some(message) = args.prompt.clone() else {
        // No `-p`: the interactive session (D136).
        // The Functions of `/function` (AGL-036, D3, D9), the run's one
        // marked.
        let catalog = function::catalog::Catalog {
            current: settings.function.clone(),
            ..function::catalog::Catalog::of(paths, project, user)
        };
        // The switch of D10 (AGL-036) before the terminal is taken: its
        // question needs the plain terminal.
        if interactive {
            plane.autostart.switch(&mut |line| eprintln!("{line}"))?;
        }
        let setup = Setup {
            plane,
            resumed,
            server_url: &settings.server_url,
            theme: Theme::new(interactive && theme::colors_enabled(env), &settings.colors),
            repl: settings.repl,
            history_file: paths.history_file(),
            catalog: &catalog,
            runtime: paths.runtime.clone(),
            paths: paths.clone(),
        };
        return if interactive {
            repl::run(setup)
        } else {
            cli::lines::run(setup).map(repl::End::Exit)
        };
    };
    eprintln!("{}", tr!(lang, m::SESSION, id = plane.session.id));
    interrupt::install();
    let id = plane.session.id.clone();
    // The attached files get their placeholders before the message and are
    // stored with the session; an audio is transcribed here (AGL-014).
    let mut pending = Vec::new();
    let placeholders: Vec<String> = sources
        .into_iter()
        .map(|source| attach::add(&plane.session, &mut pending, source))
        .collect();
    let message = attach::with_placeholders(&placeholders, &message);
    observer.attach(&plane.session);
    let reasoning = plane.reasoning;
    // The turn runs on the plane's thread (AGL-075, D7); what it tells is
    // said here.
    let handle = plane.start();
    let mut report = StderrReport::new(lang);
    let done = handle
        .ask(
            plane::Command::Say {
                message,
                attached: pending,
                reasoning,
                observer: Some(observer),
            },
            &mut |sent| sent.give(&mut report),
        )
        .ok_or(Error::PlaneGone)?;
    match done.turn.expect("a message's turn")? {
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
    use forge::testutil::TempDir;

    /// The workspace of a fresh, unregistered `cwd` under a fresh `data`
    /// (nothing stored there, so no choice is asked).
    fn fresh(data: &Path, cwd: &Path) -> forge::Workspace {
        let (ws, lines) = forge::resolve(
            data,
            cwd,
            Lang::En,
            true,
            &|options| Err(Error::RehomeNoStore {
                stores: options.to_vec(),
            }),
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
