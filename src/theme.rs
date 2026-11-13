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

    /// The already-painted `row` on the background of `role` (AGL-066,
    /// D16): the background starts the row and is re-asserted after every
    /// reset the row carries, so the row's own styles stay on it.
    pub fn on_background(&self, role: Role, row: &str) -> String {
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
    }

    #[test]
    fn the_background_is_kept_over_the_row_resets() {
        let theme = Theme::new(true, &[]);
        let row = format!(
            "{}{}",
            theme.paint(Role::Tool, "TOOL"),
            theme.paint(Role::StatusSuccess, "ok")
        );
        let on = theme.on_background(Role::FocusBackground, &row);
        // The background starts the row, is re-asserted after each reset,
        // and one final reset ends it.
        assert_eq!(on.matches("\x1b[0m").count(), 3);
        assert!(on.starts_with("\x1b["));
        assert!(on.ends_with("\x1b[0m"));
        assert!(on.contains("48;2"));
        let plain = Theme::plain();
        assert_eq!(plain.on_background(Role::FocusBackground, "x"), "x");
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
