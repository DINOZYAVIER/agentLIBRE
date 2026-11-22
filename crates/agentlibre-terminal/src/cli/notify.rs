//! `agl notify`: three lines of a launcher (AGL-074, D2; AGL-121, D23).
//! The notifier lives in the crate `agentlibre-notify`; this starts the
//! `agl-notify` beside this executable and carries its exit code.

use std::io;
use std::process::{Command, ExitCode};

use crate::error::Error;
use crate::i18n::Lang;

pub fn run(lang: Lang) -> Result<ExitCode, Error> {
    let exe = std::env::current_exe().map_err(|source| Error::NotifySpawn { source })?;
    let Some(dir) = exe.parent() else {
        return Err(Error::NotifySpawn {
            source: io::Error::other("no directory beside this executable"),
        });
    };
    let code = match lang {
        Lang::En => "en",
        Lang::Uk => "uk",
    };
    let status = Command::new(dir.join("agl-notify"))
        .arg("--lang")
        .arg(code)
        .status()
        .map_err(|source| Error::NotifySpawn { source })?;
    Ok(status
        .code()
        .map(|code| ExitCode::from(u8::try_from(code).unwrap_or(1)))
        .unwrap_or(ExitCode::FAILURE))
}
