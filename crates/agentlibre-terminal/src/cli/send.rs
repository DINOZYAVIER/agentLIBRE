//! `agl send <session> <message>` (AGL-125, D3 to D5): the message goes into
//! the session and this exits; its turn is the daemon's.

use std::path::Path;
use std::process::ExitCode;

use agentlibre_common::i18n::Lang;
use agentlibre_common::paths::Paths;
use crate::remote;
use agentlibre_proto::{Reply, Request};

use crate::error::{CoreError, Error};

pub fn run(session: &str, message: &str, paths: &Paths, cwd: &Path, lang: Lang) -> Result<ExitCode, Error> {
    // No human answers here: a question of the turn waits for whoever opens
    // the session (D7).
    let client = remote::connect(paths, false, lang, true)?;
    let id = id_of(&client, session, cwd)?;
    let request = Request::Say {
        session: id,
        message: message.to_owned(),
        attached: Vec::new(),
        reasoning: None,
    };
    client.ask(&request, &mut |_| {}).map_err(CoreError::from)?;
    Ok(ExitCode::SUCCESS)
}

/// The id of the session `key`: `last` or a name of the project's sessions,
/// and otherwise an id or the start of one, which the daemon resolves.
fn id_of(client: &agentlibre_client::Client, key: &str, cwd: &Path) -> Result<String, Error> {
    let last = key == "last";
    let request = Request::Past {
        dir: Some(cwd.to_string_lossy().into_owned()),
        plane: None,
        limit: if last { 1 } else { usize::MAX },
    };
    let Reply::Past { sessions } = client.ask(&request, &mut |_| {}).map_err(CoreError::from)? else {
        return Ok(key.to_owned());
    };
    Ok(sessions
        .into_iter()
        .find(|session| last || session.name.as_deref() == Some(key))
        .map_or_else(|| key.to_owned(), |session| session.id))
}
