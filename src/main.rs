//! agl-lite: a coding agent for local models whose state machine drives
//! the grammar. See `SPEC.md`.

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the agent step uses actions from task-08")
)]
mod action;
mod cli;
mod error;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "Function loading uses formats from task-04")
)]
mod format;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the agent step uses it from task-08")
)]
mod grammar;
mod i18n;
mod paths;
mod settings;
#[cfg(test)]
mod test_server;
mod tomlfile;

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

use cli::Command;
use error::Error;
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
    let _settings = Settings::resolve(
        &user,
        &user_path,
        project.as_ref(),
        args.function.as_deref(),
    )?;
    Err(Error::NotImplemented)
}
