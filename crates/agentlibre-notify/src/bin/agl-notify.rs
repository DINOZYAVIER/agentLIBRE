//! `agl-notify` (AGL-121, D23): the notifier on its own; `agl notify`
//! launches this.

use std::process::ExitCode;

use agentlibre_core::error::Error;
use agentlibre_core::i18n::Lang;
use agentlibre_core::paths::Paths;
use agentlibre_core::settings::UserFile;

fn main() -> ExitCode {
    // `agl-notify [--lang en|uk]`; else the user file's language.
    let mut lang = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg != "--lang" {
            eprintln!("agl-notify: unknown option {arg}");
            return ExitCode::FAILURE;
        }
        let Some(code) = args.next() else {
            eprintln!("agl-notify: --lang wants en or uk");
            return ExitCode::FAILURE;
        };
        match Lang::from_code(&code) {
            Some(known) => lang = Some(known),
            None => {
                eprintln!("agl-notify: unknown language {code}");
                return ExitCode::FAILURE;
            }
        }
    }
    let paths = match Paths::from_env(|name| std::env::var_os(name)) {
        Ok(paths) => paths,
        Err(error) => return fail(error),
    };
    let lang = match lang {
        Some(lang) => lang,
        None => match UserFile::load(&paths.settings_file()) {
            Ok(user) => user.language.unwrap_or(Lang::En),
            Err(error) => return fail(error),
        },
    };
    match agentlibre_notify::run(&paths, lang) {
        Ok(code) => code,
        Err(error) => fail(error),
    }
}

fn fail(error: Error) -> ExitCode {
    eprintln!("agl-notify: {}", error.message(Lang::En));
    ExitCode::from(error.exit_code())
}
