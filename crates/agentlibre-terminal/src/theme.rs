//! Terminal colors of the REPL: the styles of all roles and what paints
//! with them (D121, D122, D129). The styles and the roles are `style`.

use std::ffi::OsString;

use crate::style::{Role, Style};

/// The styles of all roles and whether they are used at all (D122).
#[derive(Clone, Debug)]
pub struct Theme {
    enabled: bool,
    styles: Vec<Style>,
}

impl Theme {
    /// The built-in theme with `overrides` applied.
    pub fn new(enabled: bool, overrides: &[(Role, Style)]) -> Theme {
        let mut styles: Vec<Style> = Role::ALL
            .iter()
            .map(|role| {
                Style::parse(role.default_spec(), role.is_background())
                    .expect("built-in styles are valid")
            })
            .collect();
        for (role, style) in overrides {
            styles[*role as usize] = style.clone();
        }
        Theme { enabled, styles }
    }

    /// A theme that paints nothing.
    #[cfg(test)]
    pub fn plain() -> Theme {
        Theme::new(false, &[])
    }

    /// The escape sequence that starts `role`; empty when colors are off.
    pub fn start(&self, role: Role) -> String {
        self.styles[role as usize].start(self.enabled)
    }

    /// The sequence that ends all styles; empty when colors are off.
    pub fn reset(&self) -> &'static str {
        if self.enabled { "\x1b[0m" } else { "" }
    }

    /// `text` in the style of `role`.
    pub fn paint(&self, role: Role, text: &str) -> String {
        let start = self.start(role);
        if start.is_empty() {
            text.into()
        } else {
            format!("{start}{text}\x1b[0m")
        }
    }

    /// The theme with every accent off but the labels' (AGL-105, D1;
    /// AGL-107, D1): each role but a background's and the tool's wears the
    /// style of `thinking_muted` (AGL-121, D16 e). A thinking block paints with it while it has no focus,
    /// so only its head stands out; the rest of the accents come back where
    /// the focus stands on it.
    pub fn gray(&self) -> Theme {
        let muted = self.styles[Role::ThinkingMuted as usize].clone();
        Theme {
            enabled: self.enabled,
            styles: Role::ALL
                .iter()
                .map(|role| {
                    if role.is_background() || *role == Role::Tool {
                        self.styles[*role as usize].clone()
                    } else {
                        muted.clone()
                    }
                })
                .collect(),
        }
    }

    /// `text` in the style of `role` over the base style of `base`
    /// (AGL-072, D2): the base starts, the role follows, so the role's color
    /// wins over the base's and its attributes add while a role without a
    /// color keeps the base's. `None` is the base alone; colors off give no
    /// escape at all.
    pub fn paint_over(&self, base: Role, role: Option<Role>, text: &str) -> String {
        let start = match role {
            Some(role) => format!("{}{}", self.start(base), self.start(role)),
            None => self.start(base),
        };
        if start.is_empty() {
            return text.into();
        }
        format!("{start}{text}{}", self.reset())
    }

    /// The already-styled `row` under the style of `role` (AGL-066, D16;
    /// AGL-093, D2): the style starts the row and is re-asserted after every
    /// reset the row carries, so what the row paints for itself wins and
    /// what it leaves alone keeps `role` — the thinking's own style under
    /// the styles of its Markdown.
    pub fn on_style(&self, role: Role, row: &str) -> String {
        let start = self.start(role);
        if start.is_empty() {
            return row.to_owned();
        }
        let reassert = format!("\x1b[0m{start}");
        format!("{start}{}\x1b[0m", row.replace("\x1b[0m", &reassert))
    }
}

/// Whether the terminal gets colors: truecolor advertised, `TERM` set and
/// not `dumb`, no `NO_COLOR` (D122).
pub fn colors_enabled(env: impl Fn(&str) -> Option<OsString>) -> bool {
    let term_ok = env("TERM").is_some_and(|term| !term.is_empty() && term != "dumb");
    let truecolor = env("COLORTERM").is_some_and(|value| {
        let value = value.to_string_lossy().to_ascii_lowercase();
        value == "truecolor" || value == "24bit"
    });
    env("NO_COLOR").is_none() && term_ok && truecolor
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style::{COLOR_KEYS, StyleError};

    fn style(spec: &str) -> Result<Style, StyleError> {
        Style::parse(spec, false)
    }

    #[test]
    fn the_gray_theme_keeps_the_labels_and_the_backgrounds() {
        // AGL-105, D1; AGL-107, D1; AGL-121, D16 e: every accent of the
        // gray theme is `thinking_muted`'s style, not `muted`'s, but the
        // block's label and the backgrounds — the tool's head still stands
        // out, and the focus's background still stands under the gray rows.
        let red = Style::parse("#FF0000", false).expect("a color");
        let green = Style::parse("#00FF00", false).expect("a color");
        let theme = Theme::new(true, &[(Role::Tool, red), (Role::ThinkingMuted, green)]);
        let gray = theme.gray();
        assert_eq!(gray.start(Role::Tool), theme.start(Role::Tool));
        assert_eq!(gray.start(Role::MarkdownCode), theme.start(Role::ThinkingMuted));
        assert_ne!(gray.start(Role::MarkdownCode), theme.start(Role::Muted));
        assert_eq!(gray.start(Role::FocusBackground), theme.start(Role::FocusBackground));
        assert!(!gray.start(Role::FocusBackground).is_empty());
        assert!(Theme::plain().gray().start(Role::Tool).is_empty());
    }

    #[test]
    fn built_in_theme_covers_every_role() {
        assert_eq!(Role::ALL.len(), COLOR_KEYS.len());
        for role in Role::ALL {
            assert_eq!(Role::from_key(role.key()), Some(*role));
        }
        let theme = Theme::new(true, &[]);
        assert_eq!(
            theme.paint(Role::StatusSuccess, "ok"),
            "\x1b[1;38;2;123;216;143mok\x1b[0m"
        );
        assert!(theme.start(Role::InputBackground).contains("48;2"));
        // AGL-093, D2: the thinking is a register of its own, italic and
        // dim, not the muted color of a notice.
        assert_eq!(theme.start(Role::Thinking), "\x1b[3;2m");
    }

    #[test]
    fn the_thinkings_style_stays_under_its_markdown() {
        // AGL-093, D1, D2: what the thinking paints for itself wins, what
        // it leaves alone keeps the thinking's style.
        let theme = Theme::new(true, &[]);
        let row = format!("{}{}", theme.paint(Role::MarkdownStrong, "x"), " y");
        let under = theme.on_style(Role::Thinking, &row);
        assert!(under.starts_with("\x1b[3;2m"), "{under}");
        // The reset the Markdown row carries puts the thinking's style back.
        assert!(under.contains("\x1b[0m\x1b[3;2m"), "{under}");
        assert_eq!(under.matches("\x1b[0m").count(), 2);
        assert_eq!(Theme::plain().on_style(Role::Thinking, "x"), "x");
    }

    #[test]
    fn a_role_paints_over_its_base() {
        // AGL-072, D2: the base starts, the role follows; the role's color
        // wins over the base's and an attribute-only role keeps it.
        let theme = Theme::new(true, &[]);
        assert_eq!(
            theme.paint_over(Role::InputText, Some(Role::MarkdownStrong), "x"),
            "\x1b[38;2;255;226;226m\x1b[1mx\x1b[0m"
        );
        assert_eq!(
            theme.paint_over(Role::InputText, Some(Role::StatusSuccess), "x"),
            "\x1b[38;2;255;226;226m\x1b[1;38;2;123;216;143mx\x1b[0m"
        );
        // No role is the base alone, and colors off give no escape.
        assert_eq!(
            theme.paint_over(Role::InputText, None, "x"),
            theme.paint(Role::InputText, "x")
        );
        assert_eq!(
            Theme::plain().paint_over(Role::InputText, Some(Role::Tool), "x"),
            "x"
        );
    }

    #[test]
    fn the_background_is_kept_over_the_row_resets() {
        let theme = Theme::new(true, &[]);
        let row = format!(
            "{}{}",
            theme.paint(Role::Tool, "TOOL"),
            theme.paint(Role::StatusSuccess, "ok")
        );
        let on = theme.on_style(Role::FocusBackground, &row);
        // The background starts the row, is re-asserted after each reset,
        // and one final reset ends it.
        assert_eq!(on.matches("\x1b[0m").count(), 3);
        assert!(on.starts_with("\x1b["));
        assert!(on.ends_with("\x1b[0m"));
        assert!(on.contains("48;2"));
        let plain = Theme::plain();
        assert_eq!(plain.on_style(Role::FocusBackground, "x"), "x");
    }

    #[test]
    fn overrides_and_plain_mode() {
        let red = style("#FF0000").unwrap();
        let theme = Theme::new(true, &[(Role::Muted, red)]);
        assert_eq!(theme.paint(Role::Muted, "x"), "\x1b[38;2;255;0;0mx\x1b[0m");
        let plain = Theme::plain();
        assert_eq!(plain.paint(Role::Muted, "x"), "x");
        assert_eq!(plain.start(Role::Muted), "");
        assert_eq!(plain.reset(), "");
    }

    #[test]
    fn color_detection() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        let good = &[("TERM", "xterm-256color"), ("COLORTERM", "truecolor")];
        assert!(colors_enabled(env(good)));
        assert!(colors_enabled(env(&[
            ("TERM", "foot"),
            ("COLORTERM", "24bit")
        ])));
        assert!(!colors_enabled(env(&[("TERM", "xterm")])));
        assert!(!colors_enabled(env(&[
            ("TERM", "dumb"),
            ("COLORTERM", "truecolor")
        ])));
        assert!(!colors_enabled(env(&[("COLORTERM", "truecolor")])));
        assert!(!colors_enabled(env(&[
            ("TERM", "xterm"),
            ("COLORTERM", "truecolor"),
            ("NO_COLOR", "1")
        ])));
    }
}
