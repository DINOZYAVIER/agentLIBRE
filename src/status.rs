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
pub const COMMANDS: [&str; 10] = [
    "/quit", "/exit", "/help", "/status", "/think", "/attach", "/remember", "/new", "/compact",
    "/function",
];

/// What `/remember` asked for (AGL-006, M34, M37).
#[derive(Debug, PartialEq, Eq)]
pub enum Remember {
    /// Nothing after the command.
    Usage,
    /// `/remember name | text`: the program writes the entry itself.
    Entry { name: String, text: String },
    /// `/remember text`: an ordinary turn with this message; the model
    /// names the entry.
    Turn(String),
}

/// Reads what follows `/remember`: with a `|` the name is before it and
/// the text after (M37); an empty name or text is the usage.
pub fn parse_remember(rest: &str) -> Remember {
    let rest = rest.trim();
    if rest.is_empty() {
        return Remember::Usage;
    }
    match rest.split_once('|') {
        None => Remember::Turn(format!("remember: {rest}")),
        Some((name, text)) => {
            let (name, text) = (name.trim(), text.trim());
            if name.is_empty() || text.is_empty() {
                Remember::Usage
            } else {
                Remember::Entry {
                    name: name.to_owned(),
                    text: text.to_owned(),
                }
            }
        }
    }
}

/// What `/status` tells besides the session (D134).
#[derive(Clone)]
pub struct Facts {
    pub function: String,
    pub workspace: String,
    pub server: String,
}

/// What changes during the session.
#[derive(Clone, Default)]
pub struct Info {
    pub session: String,
    /// The state of the last visit.
    pub state: Option<String>,
    /// The state of an interrupted turn.
    pub unfinished: Option<String>,
    /// The context size of the server, known once it answers (AGL-015).
    pub context: Option<usize>,
}

/// `/help` (D134); the keys only make sense in a terminal, and
/// `/function` is the terminal session's (AGL-036, D3).
pub fn help_text(lang: Lang, with_keys: bool) -> String {
    let mut lines: Vec<String> = COMMANDS
        .iter()
        .filter(|name| with_keys || **name != "/function")
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
        texts::status_line(
            "context",
            &info
                .context
                .map_or_else(|| "—".to_owned(), |n| tr!(lang, m::TOKENS, n = n)),
            lang,
        ),
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


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_reads_a_name_before_the_bar_and_a_text_after_it() {
        assert_eq!(parse_remember(""), Remember::Usage);
        assert_eq!(parse_remember("  "), Remember::Usage);
        assert_eq!(
            parse_remember(" Build and test | cargo build / cargo test "),
            Remember::Entry {
                name: "Build and test".into(),
                text: "cargo build / cargo test".into(),
            }
        );
        // Only the first bar divides; the text may hold more.
        assert_eq!(
            parse_remember("Pipes | a | b"),
            Remember::Entry {
                name: "Pipes".into(),
                text: "a | b".into(),
            }
        );
        // No name or no text: the usage.
        assert_eq!(parse_remember("| text"), Remember::Usage);
        assert_eq!(parse_remember("name |"), Remember::Usage);
        // No bar: an ordinary turn, the model names the entry (M37).
        assert_eq!(
            parse_remember("cargo build / cargo test"),
            Remember::Turn("remember: cargo build / cargo test".into())
        );
    }

    fn facts() -> Facts {
        Facts {
            function: "minimal".into(),
            workspace: "~/project".into(),
            server: "http://127.0.0.1:18080".into(),
        }
    }

    #[test]
    fn the_context_line_is_a_dash_until_the_server_has_answered() {
        let mut info = Info::default();
        let text = status_text(&facts(), &info, None, Lang::En);
        assert!(text.contains("context: —"), "{text}");
        info.context = Some(65536);
        let text = status_text(&facts(), &info, None, Lang::En);
        assert!(text.contains("context: 65536 tokens"), "{text}");
    }
}
