//! agentLIBRE: free and open-source agentic system.

mod action;
mod about;
mod agent;
mod attach;
mod backend;
mod cli;
mod error;
mod forge;
mod format;
mod function;
mod grammar;
mod i18n;
mod interrupt;
mod machine;
mod media;
mod memory;
mod nix;
mod phase;
mod paths;
mod programs;
mod record;
mod repl;
mod search;
mod session;
mod settings;
mod status;
#[cfg(test)]
mod test_server;
mod theme;
mod tools;
mod weights;

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use agent::{Agent, TurnEnd};
use backend::Backend;
use backend::ReasoningLevel;
use backend::llama::Llama;
use backend::strata::Strata;
use cli::args::Command;
use cli::report::StderrReport;
use error::Error;
use function::Function;
use i18n::{Lang, m};
use machine::Machine;
use paths::Paths;
use session::{Session, StopReason};
use settings::{function_name, ReplSettings, ServerKind, Settings, UserFile};
use theme::Theme;

/// Closes the engine agl launched itself (AGL-024, D3): when the guard
/// drops, the child in the field is shut down. It lives to the end of the
/// agent command, so every return — normal exit, an error, an interrupted
/// turn — closes the engine; a pre-existing server is not in the field and
/// keeps running.
struct EngineGuard {
    engine: Arc<Mutex<Option<cli::serve::Engine>>>,
    /// The project's store directory, where the record of the launched
    /// server is (AGL-035, D1).
    dir: PathBuf,
}

impl Drop for EngineGuard {
    fn drop(&mut self) {
        if let Some(engine) = self.engine.lock().unwrap().take() {
            engine.shutdown();
            cli::serve::remove_record(&self.dir);
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

/// Everything a session needs.
pub struct Setup<'a> {
    pub function: &'a Function,
    pub function_name: &'a str,
    pub backend: &'a dyn Backend,
    /// The stirred mode (AGL-030, D1): the action phase generates
    /// unconstrained and a parse failure is a bounded corrective retry.
    pub stirred: bool,
    pub session: Session,
    /// Whether the session existed before this run (D126).
    pub resumed: bool,
    pub sessions_dir: &'a Path,
    /// The context size of the server when it already answered at the
    /// start; read at the first request otherwise (AGL-015, D10).
    pub context: Option<usize>,
    pub server_url: &'a str,
    /// Brings the server up when a request finds none (AGL-015).
    pub autostart: &'a cli::serve::Autostart<'a>,
    pub theme: Theme,
    pub repl: ReplSettings,
    pub lang: Lang,
    /// What the agent's `forge` tool takes from the session (AGL-008).
    pub forge: tools::ForgeEnv<'a>,
    pub history_file: PathBuf,
    /// The model's reasoning level (AGL-013); the REPL's `/think`
    /// changes it for the session.
    pub reasoning: Option<ReasoningLevel>,
    /// The most tokens one picture takes (AGL-014, D10).
    pub image_tokens: usize,
    /// Runs the `search` tool (AGL-019).
    pub search: &'a dyn search::Searcher,
    /// Where the memory of the workspace is (AGL-006).
    pub memory: &'a dyn memory::Home,
    /// What the model is told of its environment and of this agl (AGL-048).
    pub about: Option<&'a str>,
    /// The Functions of the REPL's `/function` (AGL-036, D3).
    pub catalog: &'a cli::function::Catalog,
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
    let args = match cli::args::parse(args)? {
        Command::Version => {
            let rev = env(about::REV_VAR).and_then(|rev| rev.into_string().ok());
            let _ = writeln!(io::stdout(), "{}", about::version(rev.as_deref()));
            return Ok(ExitCode::SUCCESS);
        }
        Command::Help => {
            // A closed pipe, as in `agl --help | head -1`, is not an error.
            let _ = writeln!(io::stdout(), "{}", tr!(*lang, m::HELP));
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
            let function = Function::load(&paths.functions_dir(&project.key), &paths.functions(), &name)?;
            // The weights: `start` asks for a file it does not find, as a
            // run does; `status` and `stop` keep the name (AGL-039, D24).
            let mut model = function.model.clone();
            let missing = match action {
                cli::args::ServerAction::Start if repl::term::is_interactive() => {
                    weights::Missing::Ask(&cli::serve::ask_line)
                }
                cli::args::ServerAction::Start => weights::Missing::Fail,
                _ => weights::Missing::Keep,
            };
            weights::resolve(
                &mut model,
                &mut settings::WhisperFile::default(),
                &weights::Folders {
                    own: paths.models(),
                    listed: &user.models,
                    user_path: &paths.settings_file(),
                },
                &missing,
                *lang,
            )?;
            let server = cli::serve::Server::of(&project, &settings, &paths.settings_file(), model)?;
            return cli::serve::run(action, &server, &paths.models(), env("PATH"), yes, *lang);
        }
        Command::Init => {
            let (paths, user) = loaded?;
            let cwd = std::env::current_dir().map_err(Error::CurrentDir)?;
            return cli::init::run(&user, &paths, &cwd, *lang);
        }
        // The forge surface parses its own arguments (AGL-007); it runs
        // after the settings load, so `[forge]` and `language` apply.
        Command::Forge(args) => {
            let (paths, user) = loaded?;
            let cwd = std::env::current_dir().map_err(Error::CurrentDir)?;
            return cli::forge::run(&args, &user, &paths, &cwd, *lang);
        }
        Command::Function(action) => {
            let (paths, user) = loaded?;
            let cwd = std::env::current_dir().map_err(Error::CurrentDir)?;
            return cli::function::run(action, &user, &paths, &cwd, *lang);
        }
        Command::Search(action) => {
            let (paths, user) = loaded?;
            return cli::search::run(action, &user, &paths, *lang);
        }
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
    };
    let mut args = args;
    loop {
        match agent_session(&env, &paths, &user, &cwd, &project, &args, &engine, *lang)? {
            repl::End::Exit(code) => return Ok(code),
            // `/function <name>` (AGL-036, D4): a new session with that
            // Function, the reasoning level kept.
            repl::End::Function { name, reasoning } => {
                args = cli::args::Args {
                    function: Some(name),
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
    engine: &Arc<Mutex<Option<cli::serve::Engine>>>,
    lang: Lang,
) -> Result<repl::End, Error> {
    let user_path = paths.settings_file();
    let session = args
        .resume
        .as_deref()
        .map(|id| Session::open(&paths.sessions(), id))
        .transpose()?;
    // A resumed session keeps its Function (D106).
    let resumed = session.as_ref().map(|s| s.function.as_str());
    // The Function's name (D58, D59, AGL-015); the Function loads before
    // the settings, because its `[server]` table is the model side of the
    // settings when the file declares it (AGL-033, D1, D3).
    let name = function_name(
        resumed.or(args.function.as_deref()),
        project.function.as_deref(),
        user.function.as_deref(),
    );
    // The Function's tiers (AGL-028, D3): the workspace store's
    // `functions/` directory, then the user's, then the built-ins.
    let mut function = Function::load(&paths.functions_dir(&project.key), &paths.functions(), &name)?;
    let project_settings = settings::ProjectFile {
        function: project.function.clone(),
        server: project.server.clone(),
        whisper: project.whisper.clone(),
        search: project.search.clone(),
    };
    let mut settings = Settings::resolve(
        user,
        &user_path,
        (&project.dir.join(settings::SETTINGS_FILE), &project_settings),
        resumed.or(args.function.as_deref()),
        &function.model,
    )?;
    // The weights files become absolute paths before anything starts; a
    // file found nowhere is asked for in a terminal (AGL-039, D22-D24).
    let missing = if repl::term::is_interactive() {
        weights::Missing::Ask(&cli::serve::ask_line)
    } else {
        weights::Missing::Fail
    };
    weights::resolve(
        &mut settings.server,
        &mut settings.whisper,
        &weights::Folders {
            own: paths.models(),
            listed: &user.models,
            user_path: &user_path,
        },
        &missing,
        lang,
    )?;
    // The tool `search` is there only where `[search]` is complete: no tool,
    // no grammar for it and no word of it in the prompt otherwise
    // (AGL-019, D5).
    // An expired client certificate leaves the tool out too, with a
    // notice (AGL-043 F20).
    let expired = settings.search.as_ref().is_some_and(|config| {
        programs::program("step").is_ok_and(|step| {
            cli::search::certificate_expired(&step, &config.client_certificate)
        })
    });
    let curl;
    let search: &dyn search::Searcher = match &settings.search {
        Some(config) if !expired => {
            curl = search::Curl::new(config.clone())?;
            &curl
        }
        Some(config) => {
            eprintln!(
                "{}",
                tr!(
                    lang,
                    m::SEARCH_CERT_EXPIRED,
                    cert = config.client_certificate.display()
                )
            );
            function.without_tool("search");
            &search::NoSearch
        }
        None => {
            function.without_tool("search");
            &search::NoSearch
        }
    };
    // A turn to continue needs its state in the Function (D105).
    if let Some(turn) = session.as_ref().and_then(Session::unfinished) {
        Machine::resume(&function, &settings.function, &turn)?;
    }

    let workspace = session
        .as_ref()
        .map_or_else(|| cwd.to_path_buf(), |s| s.workspace.clone());
    let forge = tools::ForgeEnv {
        lang,
        auto_rehome: user.forge.auto_rehome.unwrap_or(true),
        data: &paths.data,
    };
    let image_tokens = settings
        .server
        .image_max_tokens
        .unwrap_or(settings::DEFAULT_IMAGE_TOKENS) as usize;
    let memory = memory::StoreHome {
        workspace: workspace.clone(),
        auto_rehome: user.forge.auto_rehome.unwrap_or(true),
        lang,
    };
    // The settings' `server` is the effective one (the function's
    // `[server]` table when the file declares it, AGL-033, D1, D3), and
    // the backend and the url run on it.
    // The backend is the Llama or the Strata backend, by the effective
    // settings' `[server] kind` (AGL-030, D1); the stirred mode runs on
    // Strata.
    let stirred = settings.server.kind() == ServerKind::Strata;
    let backend: Box<dyn Backend> = if stirred {
        Box::new(Strata::new(&settings.server_url))
    } else {
        Box::new(Llama::new(&settings.server_url))
    };
    let autostart = cli::serve::Autostart {
        llama: backend.as_ref(),
        project,
        effective: &settings.server,
        whisper: &settings.whisper,
        user_path: user_path.clone(),
        models: paths.models(),
        path_var: env("PATH"),
        lang,
        // The consent of D2 (AGL-033): the restart asks, as the server
        // commands do — `--yes` skips, the terminal asks, a
        // non-interactive terminal refuses.
        others: if args.yes {
            cli::serve::Others::Yes
        } else if repl::term::is_interactive() {
            cli::serve::Others::Ask(&cli::serve::ask_terminal)
        } else {
            cli::serve::Others::Refuse
        },
        engine: Arc::clone(engine),
    };
    // The files of `--attach` are read before anything is started.
    let sources = args
        .attach
        .iter()
        .map(|path| media::Source::from_file(Path::new(path)))
        .collect::<Result<Vec<_>, _>>()?;
    // `-p` has one request, so the server is brought up now; the stdin
    // session does it at its first request (AGL-015, D10), and the
    // interactive one as it starts unless `repl.start_server` is off
    // (AGL-046). Those two take the context size if the server answers.
    // The FSM of the turn sees the start too; its lines wait for the session
    // (AGL-046, D10).
    let mut observer = phase::Observer::new();
    let context = if args.prompt.is_some() {
        autostart.ensure(&mut |progress| match progress {
            cli::serve::ServerProgress::Starting => {
                observer.on(phase::PhaseEvent::ServerStart);
            }
            cli::serve::ServerProgress::Line(line) => eprintln!("{line}"),
        })?;
        Some(backend.context_size()?)
    } else {
        backend.context_size().ok()
    };
    let resumed = session.is_some();
    let mut session = match session {
        Some(session) => session,
        None => Session::create(&paths.sessions(), &settings.function, &workspace)?,
    };
    // What the model is told of its environment and of this agl (AGL-048).
    let about = settings.about.enabled.unwrap_or(true).then(|| {
        let nix_dir = env(nix::NIX_VAR).map(PathBuf::from);
        let name = format!("{}.nix", settings.function);
        let function_file = [
            paths.functions_dir(&project.key).join(&name),
            paths.functions().join(&name),
        ]
        .into_iter()
        .find(|file| file.is_file())
        .or_else(|| {
            nix_dir
                .as_ref()
                .map(|dir| dir.join("functions").join(&name))
                .filter(|file| file.is_file())
        });
        let session_format = nix_dir
            .as_ref()
            .map(|dir| dir.join("docs/session-file.md"))
            .filter(|file| file.is_file());
        let git_root = about::git_root(&workspace);
        let now = session::utc_now();
        let version = about::version(env(about::REV_VAR).and_then(|rev| rev.into_string().ok()).as_deref());
        let (server_kind, model) = match settings.server.kind() {
            ServerKind::Strata => ("strata", settings.server.strata_config.as_deref()),
            ServerKind::Llama => ("llama", settings.server.model.as_deref()),
        };
        about::About {
            workspace: &workspace,
            git_root: git_root.as_deref(),
            system: std::env::consts::OS,
            date: &now[..10.min(now.len())],
            version: &version,
            function: &settings.function,
            function_file: function_file.as_deref(),
            server_kind,
            server_url: &settings.server_url,
            model,
            session_format: session_format.as_deref(),
            user_settings: user_path.as_path(),
            project_store: &project.dir,
            source: settings.about.source.as_deref(),
        }
        .render()
    });
    let Some(message) = args.prompt.clone() else {
        // No `-p`: the interactive session (D136).
        let interactive = repl::term::is_interactive();
        // The Functions of `/function` (AGL-036, D3, D9), the run's one
        // marked.
        let catalog = cli::function::Catalog {
            current: settings.function.clone(),
            ..cli::function::Catalog::of(paths, project, user)
        };
        // The switch of D10 (AGL-036) before the terminal is taken: its
        // question needs the plain terminal.
        if interactive {
            autostart.switch(&mut |line| eprintln!("{line}"))?;
        }
        let setup = Setup {
            function: &function,
            function_name: &settings.function,
            backend: backend.as_ref(),
            stirred,
            session,
            resumed,
            sessions_dir: &paths.sessions(),
            context,
            server_url: &settings.server_url,
            autostart: &autostart,
            theme: Theme::new(interactive && theme::colors_enabled(env), &settings.colors),
            repl: settings.repl,
            lang,
            forge,
            history_file: paths.history_file(),
            reasoning: args.reasoning,
            image_tokens,
            search,
            memory: &memory,
            about: about.as_deref(),
            catalog: &catalog,
        };
        return if interactive {
            repl::run(setup)
        } else {
            cli::lines::run(setup).map(repl::End::Exit)
        };
    };
    eprintln!("{}", tr!(lang, m::SESSION, id = session.id));
    interrupt::install();
    let id = session.id.clone();
    // The attached files get their placeholders before the message and are
    // stored with the session; an audio is transcribed here (AGL-014).
    let mut pending = Vec::new();
    let placeholders: Vec<String> = sources
        .into_iter()
        .map(|source| attach::add(&session, &mut pending, source))
        .collect();
    let message = attach::with_placeholders(&placeholders, &message);
    observer.attach(&session);
    let stored = attach::store(
        &mut session,
        pending,
        &message,
        backend.as_ref(),
        &autostart,
        &mut |line| {
            observer.on(phase::PhaseEvent::Transcribe);
            eprintln!("{line}");
        },
    );
    if stored.is_err() {
        observer.on(phase::PhaseEvent::Failed);
    }
    stored?;
    let mut agent = Agent {
        function: &function,
        function_name: &settings.function,
        backend: backend.as_ref(),
        stirred,
        session: &mut session,
        workspace: &workspace,
        context: context.expect("`-p` read the context size"),
        forge,
        reasoning: args.reasoning,
        image_tokens,
        whisper: &autostart,
        search,
        memory: &memory,
        about: about.as_deref(),
    };
    match agent.turn_observed(&message, &mut StderrReport::new(lang), &mut observer)? {
        TurnEnd::Answer(text) => {
            outln!("{text}");
            Ok(repl::End::Exit(ExitCode::SUCCESS))
        }
        TurnEnd::Stopped(reason) => {
            eprintln!("agl: {}", i18n::stopped(reason, &function, &id, lang));
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
