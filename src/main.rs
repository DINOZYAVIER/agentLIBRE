//! agentLIBRE: free and open-source agentic system.

mod action;
mod agent;
mod backend;
mod cli;
mod error;
mod format;
mod function;
mod grammar;
mod i18n;
mod interrupt;
mod machine;
mod paths;
mod repl;
mod report;
mod server;
mod session;
mod settings;
mod term;
#[cfg(test)]
mod test_server;
mod theme;
mod tomlfile;
mod tools;

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

use agent::{Agent, TurnEnd};
use backend::Backend;
use backend::llama::Llama;
use cli::Command;
use error::Error;
use function::Function;
use i18n::Lang;
use machine::Machine;
use paths::Paths;
use report::StderrReport;
use session::{Session, StopReason};
use settings::{ProjectFile, Settings, UserFile};
use theme::Theme;

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
    let args = match cli::parse(args)? {
        Command::Help => {
            // A closed pipe, as in `agentLIBRE --help | head -1`, is not an error.
            let _ = writeln!(io::stdout(), "{}", i18n::help(*lang));
            return Ok(ExitCode::SUCCESS);
        }
        Command::Run(args) => args,
        Command::Server(action) => {
            let (paths, user) = loaded?;
            return server::run(
                action,
                &user,
                &paths.settings_file(),
                &paths.models(),
                env("PATH"),
                *lang,
            );
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
        let interactive = term::is_interactive();
        let setup = repl::Setup {
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
            repl::run_lines(setup)
        };
    };
    eprintln!("{}", i18n::session(&session.id, *lang));
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
