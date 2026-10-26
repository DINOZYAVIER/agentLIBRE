//! agentLIBRE: free and open-source agentic system.

mod action;
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
mod paths;
mod record;
mod repl;
mod search;
mod session;
mod settings;
mod status;
#[cfg(test)]
mod test_server;
mod theme;
mod tomlfile;
mod tools;

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
}

impl EngineGuard {
    fn new(autostart: &cli::serve::Autostart<'_>) -> Self {
        EngineGuard {
            engine: Arc::clone(&autostart.engine),
        }
    }
}

impl Drop for EngineGuard {
    fn drop(&mut self) {
        if let Some(engine) = self.engine.lock().unwrap().take() {
            engine.shutdown();
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
    pub forge: tools::ForgeEnv,
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
        Command::Help => {
            // A closed pipe, as in `agl --help | head -1`, is not an error.
            let _ = writeln!(io::stdout(), "{}", tr!(*lang, m::HELP));
            return Ok(ExitCode::SUCCESS);
        }
        Command::Run(args) => args,
        Command::Server(action, yes) => {
            let (paths, user) = loaded?;
            let cwd = std::env::current_dir().map_err(Error::CurrentDir)?;
            let project = cli::forge::project_of(
                &paths,
                &cwd,
                user.forge.auto_rehome.unwrap_or(true),
                *lang,
            )?;
            let settings = project.server.over(&user.server);
            // The Function's `[server]` table, when the file declares it,
            // is the model side of the settings (AGL-033, D1, D3): the
            // command works on the project's function (it has no
            // `--function` of its own).
            let name = function_name(
                None,
                project.function.as_deref(),
                user.function.as_deref(),
            );
            let function = Function::load(
                &paths.functions_dir(&project.key),
                &paths.functions(),
                &paths.formats(),
                &name,
            )?;
            let function_server = function
                .server
                .as_ref()
                .map(|table| (table.clone(), function.path.clone()));
            let server = cli::serve::Server::of(
                &project,
                &settings,
                &paths.settings_file(),
                function_server,
            )?;
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
    };
    let (paths, user) = loaded?;
    let user_path = paths.settings_file();
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
    let mut function = Function::load(
        &paths.functions_dir(&project.key),
        &paths.functions(),
        &paths.formats(),
        &name,
    )?;
    let settings = Settings::resolve(
        &user,
        &user_path,
        (
            &project.dir.join(forge::MARKER_FILE),
            project.function.as_deref(),
            &project.server,
            &project.whisper,
            &project.search,
        ),
        resumed.or(args.function.as_deref()),
        function.server.as_ref().map(|table| (table, function.path.as_path())),
    )?;
    // The tool `search` is there only where `[search]` is complete: no tool,
    // no grammar for it and no word of it in the prompt otherwise
    // (AGL-019, D5).
    let curl;
    let search: &dyn search::Searcher = match &settings.search {
        Some(config) => {
            curl = search::Curl::new(config.clone());
            &curl
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

    let workspace = session.as_ref().map_or(cwd, |s| s.workspace.clone());
    let forge = tools::ForgeEnv {
        lang: *lang,
        auto_rehome: user.forge.auto_rehome.unwrap_or(true),
    };
    let image_tokens = settings
        .server
        .image_max_tokens
        .unwrap_or(settings::DEFAULT_IMAGE_TOKENS) as usize;
    let memory = memory::StoreHome {
        workspace: workspace.clone(),
        auto_rehome: user.forge.auto_rehome.unwrap_or(true),
        lang: *lang,
    };
    // The resolved `[server]` (the project's over the user's): the known
    // state of a running server (AGL-033, D2). The settings' `server` is
    // the effective one (the function's `[server]` table when the file
    // declares it, D1, D3), and the backend and the url run on it.
    let fallback = project.server.over(&user.server);
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
        project: &project,
        settings: &fallback,
        effective: &settings.server,
        whisper: &settings.whisper,
        user_path: user_path.clone(),
        models: paths.models(),
        path_var: env("PATH"),
        lang: *lang,
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
        engine: Arc::new(Mutex::new(None)),
    };
    // The engine agl launched (when it did) is shut down when this guard
    // drops, at the end of the command: normal exit, an error, an
    // interrupted turn (AGL-024, D3). The hard `_exit` of a second Ctrl-C
    // runs no Rust code and leaves the engine running, the accepted
    // consequence recorded in the spec.
    let _engine_guard = EngineGuard::new(&autostart);
    // The files of `--attach` are read before anything is started.
    let sources = args
        .attach
        .iter()
        .map(|path| media::Source::from_file(Path::new(path)))
        .collect::<Result<Vec<_>, _>>()?;
    // `-p` has one request, so the server is brought up now; the
    // interactive and the stdin session do it at their first request
    // (AGL-015, D10) and take the context size if the server answers.
    let context = if args.prompt.is_some() {
        autostart.ensure(&mut |line| eprintln!("{line}"))?;
        Some(backend.context_size()?)
    } else {
        backend.context_size().ok()
    };
    let resumed = session.is_some();
    let mut session = match session {
        Some(session) => session,
        None => Session::create(&paths.sessions(), &settings.function, &workspace)?,
    };
    let Some(message) = args.prompt else {
        // No `-p`: the interactive session (D136).
        let interactive = repl::term::is_interactive();
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
            lang: *lang,
            forge,
            history_file: paths.history_file(),
            reasoning: args.reasoning,
            image_tokens,
            search,
            memory: &memory,
        };
        return if interactive {
            repl::run(setup)
        } else {
            cli::lines::run(setup)
        };
    };
    eprintln!("{}", tr!(*lang, m::SESSION, id = session.id));
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
    attach::store(
        &mut session,
        pending,
        &message,
        backend.as_ref(),
        &autostart,
        &mut |line| eprintln!("{line}"),
    )?;
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
    };
    match agent.turn(&message, &mut StderrReport::new(*lang))? {
        TurnEnd::Answer(text) => {
            println!("{text}");
            Ok(ExitCode::SUCCESS)
        }
        TurnEnd::Stopped(reason) => {
            eprintln!("agl: {}", i18n::stopped(reason, &function, &id, *lang));
            Ok(ExitCode::from(match reason {
                StopReason::Interrupted => interrupt::EXIT_CODE,
                _ => 1,
            }))
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
