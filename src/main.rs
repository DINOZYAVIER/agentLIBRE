//! agentLIBRE: free and open-source agentic system.

mod action;
mod agent;
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
mod paths;
mod repl;
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

use agent::{Agent, TurnEnd};
use backend::Backend;
use backend::llama::Llama;
use cli::args::Command;
use cli::report::StderrReport;
use error::Error;
use function::Function;
use i18n::{Lang, m};
use machine::Machine;
use paths::Paths;
use session::{Session, StopReason};
use settings::{ProjectFile, ReplSettings, Settings, UserFile};
use theme::Theme;

/// Everything a session needs.
pub struct Setup<'a> {
    pub function: &'a Function,
    pub function_name: &'a str,
    pub backend: &'a dyn Backend,
    pub session: Session,
    /// Whether the session existed before this run (D126).
    pub resumed: bool,
    pub sessions_dir: &'a Path,
    pub context: usize,
    pub server_url: &'a str,
    pub theme: Theme,
    pub repl: ReplSettings,
    pub lang: Lang,
    pub history_file: PathBuf,
}

fn main() -> ExitCode {
    let env = |name: &str| std::env::var_os(name);
    let mut lang = Lang::from_locale(env);
    match run(env, std::env::args_os().skip(1), &mut lang) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("agentLIBRE: {}", error.message(lang));
            ExitCode::from(error.exit_code())
        }
    }
}

/// Runs `agentLIBRE`. Switches `lang` to the user file's `language` as soon
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
            // A closed pipe, as in `agentLIBRE --help | head -1`, is not an error.
            let _ = writeln!(io::stdout(), "{}", tr!(*lang, m::HELP));
            return Ok(ExitCode::SUCCESS);
        }
        Command::Run(args) => args,
        Command::Server(action) => {
            let (paths, user) = loaded?;
            return cli::serve::run(
                action,
                &user,
                &paths.settings_file(),
                &paths.models(),
                env("PATH"),
                *lang,
            );
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
    let project = settings::find_project(&cwd, &user_path)
        .map(|path| ProjectFile::load(&path, &user_path))
        .transpose()?;
    if args.resume.is_some() && args.function.is_some() {
        return Err(Error::FunctionWithResume);
    }
    let session = args
        .resume
        .as_deref()
        .map(|id| Session::open(&paths.sessions(), id))
        .transpose()?;
    // A resumed session keeps its Function (D106).
    let function = session.as_ref().map(|s| s.function.as_str());
    let settings = Settings::resolve(
        &user,
        &user_path,
        project.as_ref(),
        function.or(args.function.as_deref()),
    )?;
    let function = Function::load(&paths.functions(), &paths.formats(), &settings.function)?;
    // A turn to continue needs its state in the Function (D105).
    if let Some(turn) = session.as_ref().and_then(Session::unfinished) {
        Machine::resume(&function, &settings.function, &turn)?;
    }

    let workspace = session.as_ref().map_or(cwd, |s| s.workspace.clone());
    let backend = Llama::new(&settings.server_url);
    let context = backend.context_size()?;
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
            backend: &backend,
            session,
            resumed,
            sessions_dir: &paths.sessions(),
            context,
            server_url: &settings.server_url,
            theme: Theme::new(interactive && theme::colors_enabled(env), &settings.colors),
            repl: settings.repl,
            lang: *lang,
            history_file: paths.history_file(),
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
    let mut agent = Agent {
        function: &function,
        function_name: &settings.function,
        backend: &backend,
        session: &mut session,
        workspace: &workspace,
        context,
    };
    match agent.turn(&message, &mut StderrReport::new(*lang))? {
        TurnEnd::Answer(text) => {
            println!("{text}");
            Ok(ExitCode::SUCCESS)
        }
        TurnEnd::Stopped(reason) => {
            eprintln!("agentLIBRE: {}", i18n::stopped(reason, &function, &id, *lang));
            Ok(ExitCode::from(match reason {
                StopReason::Interrupted => interrupt::EXIT_CODE,
                _ => 1,
            }))
        }
    }
}
