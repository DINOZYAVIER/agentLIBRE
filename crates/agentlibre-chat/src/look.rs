//! The chat's colors are the REPL's (AGL-128, D8): each role of `colors` in
//! `settings.nix`, the user's or the default, gives its color to the part of
//! the window that plays it.

use agentlibre_common::style::{Role, Style as Spec};
use katarsa_ui::style::{Keys, Sheet, Style, StyleBuilder, Variants};

/// A role's colors, as its style sets them.
#[derive(Debug, Default, PartialEq)]
pub struct Colors {
    /// The text's color, `#RRGGBB`.
    pub text: Option<String>,
    /// The background's color, `#RRGGBB`.
    pub fill: Option<String>,
}

/// The colors of a terminal style's SGR: `38;2;R;G;B` the text, `48;2;R;G;B`
/// the background.
pub fn colors(spec: &Spec) -> Colors {
    let start = spec.start(true);
    let sgr = start.trim_start_matches("\u{1b}[").trim_end_matches('m');
    let parts: Vec<&str> = sgr.split(';').collect();
    let mut found = Colors::default();
    let mut at = 0;
    while at < parts.len() {
        match (parts[at], parts.get(at + 1)) {
            (kind @ ("38" | "48"), Some(&"2")) if at + 4 < parts.len() => {
                let rgb: Vec<u8> = parts[at + 2..at + 5].iter().filter_map(|n| n.parse().ok()).collect();
                if let [r, g, b] = rgb[..] {
                    let hex = format!("#{r:02X}{g:02X}{b:02X}");
                    if kind == "38" {
                        found.text = Some(hex);
                    } else {
                        found.fill = Some(hex);
                    }
                }
                at += 5;
            }
            _ => at += 1,
        }
    }
    found
}

/// The style of each role: the user's, else the default.
fn role_colors(user: &[(Role, Spec)], role: Role) -> Colors {
    match user.iter().find(|(set, _)| *set == role) {
        Some((_, spec)) => colors(spec),
        None => Spec::parse(role.default_spec(), role.is_background())
            .map(|spec| colors(&spec))
            .unwrap_or_default(),
    }
}

/// The window's styles (KS-032), with the colors of the roles (D8).
pub fn styles(user: &[(Role, Spec)]) -> Sheet {
    let text = |role| role_colors(user, role).text;
    let fill = |role| role_colors(user, role).fill;
    let with_text = |style: StyleBuilder, role| match text(role) {
        Some(color) => style.text_color(&color),
        None => style,
    };
    let control = |style: StyleBuilder| style.pad_xy(10.0, 6.0);
    let mut field = control(Style::builder());
    if let Some(color) = text(Role::InputText) {
        field = field.text_color(&color);
    }
    if let Some(color) = fill(Role::InputBackground) {
        field = field.fill(&color);
    }
    if let Some(color) = text(Role::InputRule) {
        field = field.border(1.0, &color);
    }
    let mut sheet = Sheet::new()
        .define("chat", Style::builder().fill("oklch(0.19 0.01 260)").text_size(15.0))
        .define("chat.field", field)
        .define("chat.button", control(Style::builder()))
        // A line that failed, as the REPL's `status_failure`.
        .define("chat.failure", with_text(Style::builder(), Role::StatusFailure))
        // The row under the field, in the roles of the REPL's dock (D1):
        // the Function as a tool, the usage as the text, the phase as the
        // activity.
        .define("chat.dock", with_text(Style::builder(), Role::Tool))
        .define("chat.usage", with_text(Style::builder(), Role::InputText))
        .define("chat.activity", with_text(Style::builder(), Role::Activity));
    // The headings of Markdown and its code block.
    if let Some(color) = text(Role::MarkdownHeading) {
        for name in ["markdown.h1", "markdown.h2", "markdown.h3"] {
            let size = match name {
                "markdown.h1" => 24.0,
                "markdown.h2" => 20.0,
                _ => 17.0,
            };
            sheet = sheet.define(
                name,
                Style::builder()
                    .text_size(size)
                    .text_weight(katarsa_ui::retained::Weight::SemiBold)
                    .text_color(&color),
            );
        }
    }
    if let Some(color) = text(Role::MarkdownCode) {
        sheet = sheet.define(
            "markdown.code",
            Style::builder()
                .text_font(katarsa_ui::retained::Font::Mono)
                .fill("muted")
                .radius("md")
                .pad_xy(10.0, 8.0)
                .text_color(&color),
        );
    }
    // A fold's header: the calls', as the REPL's `tool`.
    if let Some(color) = text(Role::Tool) {
        sheet = sheet.define(
            "fold",
            Style::builder()
                .fill("transparent")
                .pad_xy(6.0, 2.0)
                .text_color(&color)
                .hover(|v| v.fill("muted"))
                .focus_visible(|v| v.fill("muted"))
                .pressed(|v| v.fill("muted")),
        );
    }
    // A paragraph's spans: the inline code, the link, the strong and the
    // emphasised words, each the engine's look but for its color.
    let inline = text(Role::MarkdownInlineCode).unwrap_or_else(|| "oklch(0.85 0.07 200)".into());
    let link = text(Role::MarkdownLink).unwrap_or_else(|| "primary".into());
    let strong = text(Role::MarkdownStrong).unwrap_or_else(|| "oklch(0.87 0.11 85)".into());
    let emphasis = text(Role::MarkdownEmphasis).unwrap_or_else(|| "oklch(0.80 0.08 300)".into());
    sheet.define(
        "paragraph",
        Style::builder()
            .part("strong", |p| p.text_color(&strong).text_weight(katarsa_ui::retained::Weight::SemiBold))
            .part("emphasis", |p| p.text_color(&emphasis))
            .part("code", |p| {
                p.text_color(&inline).text_font(katarsa_ui::retained::Font::Mono).fill("muted").radius(3.0)
            })
            .part("link", |p| p.text_color(&link)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_style_s_colors_are_read_off_its_sgr() {
        let spec = Spec::parse("bold #EC4899", false).unwrap();
        assert_eq!(colors(&spec), Colors { text: Some("#EC4899".into()), fill: None });
        let spec = Spec::parse("#262626", true).unwrap();
        assert_eq!(colors(&spec), Colors { text: None, fill: Some("#262626".into()) });
        assert_eq!(colors(&Spec::parse("dim", false).unwrap()), Colors::default());
    }

    #[test]
    fn the_user_s_role_wins_over_the_default() {
        let user = vec![(Role::Tool, Spec::parse("#123456", false).unwrap())];
        assert_eq!(role_colors(&user, Role::Tool).text.as_deref(), Some("#123456"));
        assert_eq!(role_colors(&[], Role::Tool).text.as_deref(), Some("#EC4899"));
    }
}
