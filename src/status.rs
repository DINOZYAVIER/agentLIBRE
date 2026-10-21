//! Vocabulary shared by the two session surfaces: the `/` commands and
//! what `/help`, `/status` and `/compact` show (D118, D134, D139, D140).

use std::path::{Path, PathBuf};

use crate::agent::Compaction;
use crate::backend::ReasoningLevel;
use crate::error::Error;
use crate::function::Function;
use crate::i18n::{Lang, m, repl as texts};
use crate::session::StopReason;
use crate::theme::Role;
use crate::tr;

/// The `/` commands (D118, D139, AGL-013).
pub const COMMANDS: [&str; 7] = [
    "/quit", "/exit", "/help", "/status", "/think", "/new", "/compact",
];

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
        lines.push(tr!(lang, m::KEYS).to_string());
    }
    lines.join("\n")
}

/// `/status` (D134, AGL-013).
pub fn status_text(
    facts: &Facts,
    info: &Info,
    reasoning: Option<ReasoningLevel>,
    lang: Lang,
) -> String {
    let state = match (&info.unfinished, &info.state) {
        (Some(state), _) => tr!(lang, m::UNFINISHED, state = state),
        (None, Some(state)) => state.clone(),
        (None, None) => tr!(lang, m::IDLE).to_string(),
    };
    [
        texts::status_line("session", &info.session, lang),
        texts::status_line("function", &facts.function, lang),
        texts::status_line("workspace", &facts.workspace, lang),
        texts::status_line("server", &facts.server, lang),
        texts::status_line("context", &tr!(lang, m::TOKENS, n = facts.context), lang),
        texts::status_line("reasoning", &texts::reasoning_level(reasoning, lang), lang),
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
        Ok(Compaction::Nothing) => {
            Some((tr!(lang, m::NOTHING_TO_COMPACT).to_string(), Role::Muted))
        }
        Ok(Compaction::Impossible) => Some((
            tr!(
                lang,
                m::COMPACTION_FAILED,
                limit = function.max_compact_tokens
            ),
            Role::StatusFailure,
        )),
        Err(Error::Interrupted) => Some((
            texts::stopped(StopReason::Interrupted, function, lang),
            Role::StatusPending,
        )),
        Err(error) => Some((error.message(lang), Role::StatusFailure)),
    }
}

