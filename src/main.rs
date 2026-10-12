//! agl-lite: a coding agent for local models whose state machine drives
//! the grammar. See `SPEC.md`.

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the agent step uses actions from task-08")
)]
mod action;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the agent step uses the backend from task-08")
)]
mod backend;
mod cli;
mod error;
mod format;
mod function;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the agent step uses it from task-08")
)]
mod grammar;
mod i18n;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the agent loop runs it from task-08")
)]
mod machine;
mod paths;
mod settings;
#[cfg(test)]
mod test_server;
mod tomlfile;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the agent step runs tools from task-08")
)]
mod tools;

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

use cli::Command;
use error::Error;
use function::Function;
use i18n::Lang;
use paths::Paths;
use settings::{ProjectFile, Settings, UserFile};

fn main() -> ExitCode {
    let env = |name: &str| std::env::var_os(name);
    let mut lang = Lang::from_locale(env);
    match run(env, std::env::args_os().skip(1), &mut lang) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("agl-lite: {}", error.message(lang));
            ExitCode::from(error.exit_code())
        }
    }
}

/// Runs `agl-lite`. Switches `lang` to the user file's `language` as soon
/// as that file loads, so later errors and `--help` use it (D66).
fn run(
    env: impl Fn(&str) -> Option<OsString>,
    args: impl IntoIterator<Item = OsString>,
    lang: &mut Lang,
) -> Result<(), Error> {
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
            // A closed pipe, as in `agl-lite --help | head -1`, is not an error.
            let _ = writeln!(io::stdout(), "{}", i18n::help(*lang));
            return Ok(());
        }
        Command::Run(args) => args,
    };
    let (paths, user) = loaded?;
    let user_path = paths.settings_file();
    let cwd = std::env::current_dir().map_err(Error::CurrentDir)?;
    let project = settings::find_project(&cwd, &user_path)
        .map(|path| ProjectFile::load(&path, &user_path))
        .transpose()?;
    let settings = Settings::resolve(
        &user,
        &user_path,
        project.as_ref(),
        args.function.as_deref(),
    )?;
    let _function = Function::load(&paths.functions(), &paths.formats(), &settings.function)?;
    Err(Error::NotImplemented)
}
