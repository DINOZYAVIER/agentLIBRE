//! Vocabulary shared by the surfaces of a session: the `/` commands and what
//! `/help`, `/status` and `/compact` show (D118, D134, D139, D140), the
//! words of the REPL's rows, a time and a speed (AGL-127, D2).

use std::path::{Path, PathBuf};

use agentlibre_common::i18n::{Lang, m};
use agentlibre_common::reasoning::ReasoningLevel;
use agentlibre_common::tr;
use agentlibre_proto::CutOff;

use crate::commands::SLASH;

/// The line of a cut-off, in the transcript and on stderr (D154;
/// AGL-043 F8).
pub fn cut_off_line(cut: &CutOff, lang: Lang) -> String {
    let what = if cut.call {
        tr!(lang, m::CUT_OFF_CALL)
    } else {
        tr!(lang, m::CUT_OFF_TEXT)
    };
    match cut.thinking {
        None => tr!(lang, m::CUT_OFF_EVENT, tokens = cut.discarded, what = what),
        Some(thinking) => tr!(
            lang,
            m::CUT_OFF_EVENT_THINKING,
            thinking = thinking,
            tokens = cut.discarded,
            what = what
        ),
    }
}

/// The line of a stir, in the transcript and on stderr (AGL-030, D4;
/// AGL-043 F8).
pub fn stir_line(attempt: u32, error: &str, lang: Lang) -> String {
    tr!(lang, m::STIR_EVENT, attempt = attempt, error = error)
}

/// What `/remember` asked for (AGL-006, M34, M37).
#[derive(Debug, PartialEq, Eq)]
pub enum Remember {
    /// Nothing after the command.
    Usage,
    /// `/remember [--workspace] name | text`: the program writes the entry
    /// itself; `workspace` names the workspace's memory in a plane
    /// (AGL-077, D8).
    Entry { name: String, text: String, workspace: bool },
    /// `/remember text`: an ordinary turn with this message; the model
    /// names the entry.
    Turn(String),
}

/// Reads what follows `/remember`: with a `|` the name is before it and
/// the text after (M37); an empty name or text is the usage.
pub fn parse_remember(rest: &str) -> Remember {
    let rest = rest.trim();
    // `--workspace` first: the workspace's memory in a plane (AGL-077, D8).
    let (workspace, rest) = match rest.strip_prefix("--workspace") {
        Some(after) if after.is_empty() || after.starts_with(char::is_whitespace) => (true, after.trim()),
        _ => (false, rest),
    };
    if rest.is_empty() {
        return Remember::Usage;
    }
    match rest.split_once('|') {
        None if workspace => Remember::Turn(format!("remember, in the workspace's memory: {rest}")),
        None => Remember::Turn(format!("remember: {rest}")),
        Some((name, text)) => {
            let (name, text) = (name.trim(), text.trim());
            if name.is_empty() || text.is_empty() {
                Remember::Usage
            } else {
                Remember::Entry {
                    name: name.to_owned(),
                    text: text.to_owned(),
                    workspace,
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
    /// The plane of the run, when it is in one (AGL-077, D6).
    pub plane: Option<String>,
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
    let mut lines: Vec<String> = SLASH
        .iter()
        .map(|cmd| cmd.name)
        // The stdin session has no `/function` and no `/plane`.
        .filter(|name| with_keys || !matches!(*name, "/function" | "/plane"))
        .map(|name| format!("{name:<9} {}", command(name, lang)))
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
        status_line("session", &info.session, lang),
        status_line("function", &facts.function, lang),
        status_line("workspace", &facts.workspace, lang),
        status_line("plane", facts.plane.as_deref().unwrap_or("—"), lang),
        status_line("server", &facts.server, lang),
        status_line(
            "context",
            &info
                .context
                .map_or_else(|| "—".to_owned(), |n| tr!(lang, m::TOKENS, n = n)),
            lang,
        ),
        status_line("reasoning", &reasoning_level(reasoning, lang), lang),
        status_line("state", &state, lang),
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

/// The block parts of an event's line (AGL-099, D1): the bracketed lines
/// the session writes itself split at the first `: ` into a label and a
/// body — `stir 2: parse error` into `stir 2` and `parse error` — and the
/// compaction line is a label with no body. Any other line says nothing;
/// it prints plain, as a command's answer does.
pub fn notice_parts(line: &str, lang: Lang) -> Option<(String, String)> {
    if line == tr!(lang, m::COMPACTING) {
        return Some((line.to_owned(), String::new()));
    }
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    let (label, body) = inner.split_once(": ")?;
    Some((label.to_owned(), body.to_owned()))
}

/// The description of a `/` command in the menu and in `/help`.
pub fn command(name: &str, lang: Lang) -> &'static str {
    match name {
        "/quit" => tr!(lang, m::CMD_QUIT),
        "/exit" => tr!(lang, m::CMD_EXIT),
        "/help" => tr!(lang, m::CMD_HELP),
        "/status" => tr!(lang, m::CMD_STATUS),
        "/think" => tr!(lang, m::CMD_THINK),
        "/attach" => tr!(lang, m::CMD_ATTACH),
        "/remember" => tr!(lang, m::CMD_REMEMBER),
        "/memory" => tr!(lang, m::CMD_MEMORY),
        "/rename" => tr!(lang, m::CMD_RENAME),
        "/new" => tr!(lang, m::CMD_NEW),
        "/compact" => tr!(lang, m::CMD_COMPACT),
        "/function" => tr!(lang, m::CMD_FUNCTION),
        "/plane" => tr!(lang, m::CMD_PLANE),
        _ => "",
    }
}

/// One line of `/status`.
pub fn status_line(label: &str, value: &str, lang: Lang) -> String {
    let label = match label {
        "session" => tr!(lang, m::LABEL_SESSION),
        "function" => tr!(lang, m::LABEL_FUNCTION),
        "workspace" => tr!(lang, m::LABEL_WORKSPACE),
        "plane" => tr!(lang, m::LABEL_PLANE),
        "server" => tr!(lang, m::LABEL_SERVER),
        "context" => tr!(lang, m::LABEL_CONTEXT),
        "reasoning" => tr!(lang, m::LABEL_REASONING),
        "state" => tr!(lang, m::LABEL_STATE),
        other => other,
    };
    tr!(lang, m::STATUS_LINE, label = label, value = value)
}

/// The reasoning level to show (AGL-013): the level's name, or the
/// template default when unset.
pub fn reasoning_level(level: Option<ReasoningLevel>, lang: Lang) -> String {
    match level {
        None => tr!(lang, m::REASONING_DEFAULT).to_string(),
        Some(level) => level.name().to_string(),
    }
}

/// `n` with a no-break space between groups of three digits, as the dock
/// groups them.
pub fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push('\u{a0}');
        }
        out.push(digit);
    }
    out
}

/// The context usage of the dock's row (D152; AGL-139, D1): `41 213 /
/// 131 072 (32%)`, `— / size` before the first prompt, `—` while the
/// server has not answered (a `size` of 0, AGL-015).
pub fn context_usage(used: Option<usize>, size: usize) -> String {
    if size == 0 {
        return "\u{2014}".into();
    }
    let Some(used) = used else {
        return format!("\u{2014} / {}", grouped(size));
    };
    let percent = used.saturating_mul(100) / size.max(1);
    format!("{} / {} ({percent}%)", grouped(used), grouped(size))
}

/// Time as `5s`, `2m 3s` or `1h 4m`.
pub fn duration(seconds: u64) -> String {
    match seconds {
        0 => "<1s".into(),
        1..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// The decoding speed (AGL-012): `46.3 tok/s` in English and
/// `46,3 ток/с` in Ukrainian.
pub fn speed(tokens_per_second: f64, lang: Lang) -> String {
    let number = format!("{tokens_per_second:.1}");
    let number = if lang == Lang::Uk {
        number.replace('.', ",")
    } else {
        number
    };
    tr!(lang, m::SPEED, speed = number)
}

#[cfg(test)]
mod notice_tests {
    use super::*;

    #[test]
    fn an_events_line_splits_to_label_and_body() {
        assert_eq!(
            notice_parts("[stir 2: parse error]", Lang::En),
            Some(("stir 2".into(), "parse error".into()))
        );
        assert_eq!(
            notice_parts("[memory: +name]", Lang::En),
            Some(("memory".into(), "+name".into()))
        );
        // The compaction line is a label with no body.
        assert_eq!(
            notice_parts("Compacting", Lang::En),
            Some(("Compacting".into(), String::new()))
        );
        // A plain line, and an unsplit bracketed one, say nothing.
        assert_eq!(notice_parts("/help says things", Lang::En), None);
        assert_eq!(notice_parts("[idle]", Lang::En), None);
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
                workspace: false,
            }
        );
        // Only the first bar divides; the text may hold more.
        assert_eq!(
            parse_remember("Pipes | a | b"),
            Remember::Entry {
                name: "Pipes".into(),
                text: "a | b".into(),
                workspace: false,
            }
        );
        assert_eq!(
            parse_remember("--workspace Build | cargo build"),
            Remember::Entry {
                name: "Build".into(),
                text: "cargo build".into(),
                workspace: true,
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
            plane: None,
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

    /// `/help` lists the names of the table, in its order, and the stdin
    /// session's lacks `/function` and `/plane` (AGL-121, D9; AGL-127, D2).
    #[test]
    fn help_lists_the_names_of_the_table_in_its_order() {
        let names = |text: &str| -> Vec<String> {
            text.lines()
                .filter(|line| line.starts_with('/'))
                .map(|line| line.split_whitespace().next().unwrap().to_owned())
                .collect()
        };
        let table: Vec<String> = SLASH.iter().map(|cmd| cmd.name.to_owned()).collect();
        for lang in [Lang::En, Lang::Uk] {
            assert_eq!(names(&help_text(lang, true)), table);
            let without: Vec<String> = table
                .iter()
                .filter(|name| !matches!(name.as_str(), "/function" | "/plane"))
                .cloned()
                .collect();
            assert_eq!(names(&help_text(lang, false)), without);
        }
    }

    #[test]
    fn the_decoding_speed_is_formatted_per_language() {
        assert_eq!(speed(46.3, Lang::En), "46.3 tok/s");
        assert_eq!(speed(46.26, Lang::En), "46.3 tok/s");
        assert_eq!(speed(46.3, Lang::Uk), "46,3 ток/с");
        assert_eq!(speed(120.0, Lang::Uk), "120,0 ток/с");
    }
}
