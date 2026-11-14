//! The splash of the REPL's start (AGL-065): the art of the root file
//! `splash`, the whole when the terminal is wide enough, the short
//! variant otherwise, and under it, muted: the version, the tagline and
//! the repository's address.

use unicode_width::UnicodeWidthChar;

use super::text::width;
use crate::i18n::{Lang, m};
use crate::style::Role;
use crate::theme::Theme;
use crate::tr;

/// The art, from the root file `splash` (D1).
const ART: &str = include_str!("../../../../splash");

/// The columns of the short variant, as the old CLI had them (D4): the
/// characters `[start, end]` of each line, joined by two spaces.
const SHORT_RANGES: [(usize, usize); 3] = [(1, 8), (11, 18), (51, 58)];

/// The width of the whole art, in columns.
pub fn full_width() -> usize {
    ART.lines().map(width).max().unwrap_or(0)
}

/// The short variant: the `SHORT_RANGES` of each line of the art, joined
/// by two spaces (D4).
fn short() -> String {
    ART.lines()
        .map(|line| {
            SHORT_RANGES
                .iter()
                .map(|&(start, end)| {
                    line.chars()
                        .skip(start)
                        .take(end - start + 1)
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("  ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Cuts `line` to `columns` by width, without an ellipsis.
fn cut(line: &str, columns: usize) -> &str {
    if width(line) <= columns {
        return line;
    }
    let mut end = 0;
    let mut used = 0;
    for (index, ch) in line.char_indices() {
        let w = ch.width().unwrap_or(0);
        if used + w > columns {
            break;
        }
        used += w;
        end = index + ch.len_utf8();
    }
    &line[..end]
}

/// The splash as the lines of the transcript: the art, cut to the
/// terminal's width and painted with the old color (D3), and under it,
/// muted: the welcome phrase (D7, D8), the version (D6) and the
/// repository's address (D7).
pub fn lines(theme: &Theme, columns: usize, lang: Lang, version: &str) -> Vec<String> {
    let short_art = short();
    let art = if columns >= full_width() { ART } else { short_art.as_str() };
    let mut out: Vec<String> = art
        .lines()
        .map(|line| theme.paint(Role::InputPrompt, cut(line, columns)))
        .collect();
    for line in [
        tr!(lang, m::SPLASH_TAGLINE),
        version,
        tr!(lang, m::SPLASH_URL),
    ] {
        out.push(theme.paint(Role::Muted, cut(line, columns)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme(enabled: bool) -> Theme {
        Theme::new(enabled, &[])
    }

    #[test]
    fn the_whole_art_when_the_terminal_reaches_its_width() {
        let lines = lines(&theme(false), full_width(), Lang::En, "agl 0.0.2");
        assert_eq!(lines.len(), ART.lines().count() + 3);
        assert_eq!(lines[0], ART.lines().next().unwrap());
        assert_eq!(
            lines[ART.lines().count()],
            "agentLIBRE is a local-first agentic system."
        );
        assert_eq!(lines[ART.lines().count() + 1], "agl 0.0.2");
        assert_eq!(lines.last().unwrap(), "github.com/dinozyavier/agentLIBRE");
    }

    #[test]
    fn the_short_variant_below_the_width_of_the_art() {
        let short = short();
        let lines = lines(&theme(false), full_width() - 1, Lang::En, "agl 0.0.2");
        assert_eq!(
            lines.iter().take(short.lines().count()).map(String::as_str).collect::<Vec<_>>(),
            short.lines().collect::<Vec<_>>()
        );
    }

    #[test]
    fn no_line_is_wider_than_the_terminal_and_none_ends_with_the_ellipsis() {
        for columns in [full_width(), full_width() - 1, 60, 28, 10] {
            let lines = lines(&theme(false), columns, Lang::En, "agl 0.0.2");
            for line in lines {
                assert!(width(&line) <= columns, "{columns}");
                assert!(!line.ends_with('…'), "{columns}");
            }
        }
    }

    #[test]
    fn the_tagline_version_and_url_follow_the_art_and_are_muted() {
        let lines = lines(&theme(true), full_width(), Lang::En, "agl 0.0.2 (abc1234)");
        let tagline = &lines[ART.lines().count()];
        let version = &lines[ART.lines().count() + 1];
        let url = lines.last().unwrap();
        assert!(tagline.contains("agentLIBRE is a local-first agentic system"));
        assert!(version.contains("agl 0.0.2 (abc1234)"));
        assert_eq!(width(version), "agl 0.0.2 (abc1234)".len());
        assert!(url.contains("github.com/dinozyavier/agentLIBRE"));
    }

    #[test]
    fn the_tagline_and_url_take_the_language() {
        let lines = lines(&theme(false), full_width(), Lang::Uk, "agl 0.0.2");
        assert_eq!(
            lines[ART.lines().count()],
            "agentLIBRE — агентна система, що живе на вашому комп'ютері."
        );
        assert_eq!(lines.last().unwrap(), "github.com/dinozyavier/agentLIBRE");
    }

    #[test]
    fn the_bare_lines_of_a_disabled_theme() {
        let bare = lines(&theme(false), full_width(), Lang::En, "agl 0.0.2");
        assert!(!bare.iter().any(|line| line.contains('\x1b')));
        let painted = lines(&theme(true), full_width(), Lang::En, "agl 0.0.2");
        assert!(painted.iter().all(|line| line.contains('\x1b')));
    }
}
