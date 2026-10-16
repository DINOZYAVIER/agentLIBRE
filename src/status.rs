//! Vocabulary shared by the two session surfaces: the `/` commands and
//! what `/help`, `/status` and `/compact` show (D118, D134, D139, D140).

use std::path::{Path, PathBuf};

use crate::agent::Compaction;
use crate::error::Error;
use crate::function::Function;
use crate::i18n::{Lang, repl as texts};
use crate::session::StopReason;
use crate::theme::Role;

/// The `/` commands (D118, D139).
pub const COMMANDS: [&str; 6] = ["/quit", "/exit", "/help", "/status", "/new", "/compact"];

/// What `/status` tells besides the session (D134).
#[derive(Clone)]
pub struct Facts {
    pub function: String,
    pub workspace: String,
    pub server: String,
    pub context: usize,
}

/// What changes during the session.
#[derive(Clone, Default)]
pub struct Info {
    pub session: String,
    /// The state of the last visit.
    pub state: Option<String>,
    /// The state of an interrupted turn.
    pub unfinished: Option<String>,
}

/// `/help` (D134); the keys only make sense in a terminal.
pub fn help_text(lang: Lang, with_keys: bool) -> String {
    let mut lines: Vec<String> = COMMANDS
        .iter()
        .map(|name| format!("{name:<9} {}", texts::command(name, lang)))
        .collect();
    if with_keys {
        lines.push(texts::keys(lang).into());
    }
    lines.join("\n")
}

/// `/status` (D134).
pub fn status_text(facts: &Facts, info: &Info, lang: Lang) -> String {
    let state = match (&info.unfinished, &info.state) {
        (Some(state), _) => texts::unfinished(state, lang),
        (None, Some(state)) => state.clone(),
        (None, None) => texts::idle(lang).to_owned(),
    };
    [
        texts::status_line("session", &info.session, lang),
        texts::status_line("function", &facts.function, lang),
        texts::status_line("workspace", &facts.workspace, lang),
        texts::status_line("server", &facts.server, lang),
        texts::status_line("context", &texts::tokens(facts.context, lang), lang),
        texts::status_line("state", &state, lang),
    ]
    .join("\n")
}

pub fn display_path(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
    {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// What to tell after `/compact`; a summary shows itself (D140).
pub fn compact_notice(
    result: Result<Compaction, Error>,
    function: &Function,
    lang: Lang,
) -> Option<(String, Role)> {
    match result {
        Ok(Compaction::Done) => None,
        Ok(Compaction::Nothing) => Some((texts::nothing_to_compact(lang).into(), Role::Muted)),
        Ok(Compaction::Impossible) => Some((
            texts::compaction_failed(function, lang),
            Role::StatusFailure,
        )),
        Err(Error::Interrupted) => Some((
            texts::stopped(StopReason::Interrupted, function, lang),
            Role::StatusPending,
        )),
        Err(error) => Some((error.message(lang), Role::StatusFailure)),
    }
}

