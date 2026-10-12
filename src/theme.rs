//! Terminal colors of the REPL: styles, roles and their defaults (D121,
//! D122, D129).

use std::ffi::OsString;

/// A style value such as `bold #7BD88F`, `dim` or `oklch(0.7 0.2 350)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Style {
    /// The SGR parameters, `None` for `none`.
    sgr: Option<String>,
}

/// Why a style value is not valid.
#[derive(Debug, PartialEq, Eq)]
pub enum StyleError {
    Empty,
    /// The color is neither `#RRGGBB` nor `oklch(L C H)`.
    Color(String),
    /// `oklch(...)` needs three numbers, lightness in 0..1, chroma >= 0.
    Oklch,
}

impl Style {
    /// Parses a foreground style, or a background one when `background`.
    pub fn parse(spec: &str, background: bool) -> Result<Style, StyleError> {
        let mut rest = spec.trim();
        if rest == "none" {
            return Ok(Style { sgr: None });
        }
        let mut codes: Vec<String> = Vec::new();
        'attributes: loop {
            for (name, code) in [
                ("bold", "1"),
                ("dim", "2"),
                ("italic", "3"),
                ("underline", "4"),
            ] {
                if rest == name {
                    codes.push(code.into());
                    rest = "";
                    break 'attributes;
                }
                if let Some(next) = rest
                    .strip_prefix(name)
                    .and_then(|next| next.strip_prefix(char::is_whitespace))
                {
                    codes.push(code.into());
                    rest = next.trim_start();
                    continue 'attributes;
                }
            }
            break;
        }
        if !rest.is_empty() {
            let (red, green, blue) = parse_color(rest)?;
            codes.extend([
                if background { "48" } else { "38" }.into(),
                "2".into(),
                red.to_string(),
                green.to_string(),
                blue.to_string(),
            ]);
        }
        if codes.is_empty() {
            return Err(StyleError::Empty);
        }
        Ok(Style {
            sgr: Some(codes.join(";")),
        })
    }

    fn start(&self, enabled: bool) -> String {
        match (enabled, &self.sgr) {
            (true, Some(sgr)) => format!("\x1b[{sgr}m"),
            _ => String::new(),
        }
    }
}

fn parse_color(value: &str) -> Result<(u8, u8, u8), StyleError> {
    let bad = || StyleError::Color(value.into());
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() != 6 || !hex.is_ascii() {
            return Err(bad());
        }
        let channel =
            |range: std::ops::Range<usize>| u8::from_str_radix(&hex[range], 16).map_err(|_| bad());
        return Ok((channel(0..2)?, channel(2..4)?, channel(4..6)?));
    }
    let payload = value
        .strip_prefix("oklch(")
        .and_then(|value| value.strip_suffix(')'))
        .ok_or_else(bad)?;
    let numbers = payload
        .split_whitespace()
        .map(|number| number.parse::<f32>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| StyleError::Oklch)?;
    let [lightness, chroma, hue] = numbers[..] else {
        return Err(StyleError::Oklch);
    };
    let finite = numbers.iter().all(|number| number.is_finite());
    if !finite || !(0.0..=1.0).contains(&lightness) || chroma < 0.0 {
        return Err(StyleError::Oklch);
    }
    Ok(oklch_to_srgb(lightness, chroma, hue))
}

fn oklch_to_srgb(lightness: f32, chroma: f32, hue: f32) -> (u8, u8, u8) {
    let hue = hue.to_radians();
    let a = chroma * hue.cos();
    let b = chroma * hue.sin();
    let l_ = lightness + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = lightness - 0.105_561_35 * a - 0.063_854_17 * b;
    let s_ = lightness - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l, m, s) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);
    let red = 4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s;
    let green = -1.268_438 * l + 2.609_757_4 * m - 0.341_319_4 * s;
    let blue = -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s;
    fn encode(value: f32) -> u8 {
        let value = if value <= 0.003_130_8 {
            12.92 * value
        } else {
            1.055 * value.clamp(0.0, 1.0).powf(1.0 / 2.4) - 0.055
        };
        (value.clamp(0.0, 1.0) * 255.0).round() as u8
    }
    (encode(red), encode(green), encode(blue))
}

macro_rules! roles {
    ($($variant:ident $key:literal $default:literal,)*) => {
        /// What a piece of the screen is painted for (D129).
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Role {
            $($variant,)*
        }

        impl Role {
            pub const ALL: &'static [Role] = &[$(Role::$variant,)*];

            /// The key in `[colors]`.
            #[cfg(test)]
            pub fn key(self) -> &'static str {
                match self {
                    $(Role::$variant => $key,)*
                }
            }

            /// The role with the `[colors]` key `key`.
            pub fn from_key(key: &str) -> Option<Role> {
                match key {
                    $($key => Some(Role::$variant),)*
                    _ => None,
                }
            }

            fn default_spec(self) -> &'static str {
                match self {
                    $(Role::$variant => $default,)*
                }
            }
        }

        /// The keys of `[colors]`.
        pub const COLOR_KEYS: &[&str] = &[$($key,)*];
    };
}

roles! {
    InputRule "input_rule" "oklch(0.439 0 0)",
    InputBackground "input_background" "oklch(0.269 0 0)",
    InputPrompt "input_prompt" "bold oklch(0.718 0.202 349.761)",
    InputHint "input_hint" "dim oklch(0.823 0.12 346.018)",
    InputText "input_text" "oklch(0.936 0.032 17.717)",
    InputActivity "input_activity" "oklch(0.823 0.12 346.018)",
    InputSelected "input_selected" "bold oklch(0.518 0.253 323.949)",
    Muted "muted" "dim",
    Rule "rule" "dim",
    Tool "tool" "bold #00FFFF",
    StatusSuccess "status_success" "bold #7BD88F",
    StatusFailure "status_failure" "bold #FF0000",
    StatusPending "status_pending" "bold #FFFF00",
    MarkdownHeading "markdown_heading" "bold #00FFFF",
    MarkdownCode "markdown_code" "dim",
    MarkdownInlineCode "markdown_inline_code" "#FFFF00",
    MarkdownStrong "markdown_strong" "bold",
    MarkdownEmphasis "markdown_emphasis" "italic",
    MarkdownLink "markdown_link" "#8AA2D8",
    MarkdownQuote "markdown_quote" "dim",
    MarkdownBullet "markdown_bullet" "#00FFFF",
    MarkdownRule "markdown_rule" "dim",
}

impl Role {
    /// Whether the style sets a background color.
    pub fn is_background(self) -> bool {
        self == Role::InputBackground
    }
}

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

    fn style(spec: &str) -> Result<Style, StyleError> {
        Style::parse(spec, false)
    }

    #[test]
    fn attributes_and_colors() {
        assert_eq!(style("bold").unwrap().sgr.as_deref(), Some("1"));
        assert_eq!(
            style("bold #7BD88F").unwrap().sgr.as_deref(),
            Some("1;38;2;123;216;143")
        );
        assert_eq!(
            style("dim italic #000001").unwrap().sgr.as_deref(),
            Some("2;3;38;2;0;0;1")
        );
        assert_eq!(style("none").unwrap().sgr, None);
        assert_eq!(
            Style::parse("#102030", true).unwrap().sgr.as_deref(),
            Some("48;2;16;32;48")
        );
    }

    #[test]
    fn oklch_colors() {
        // Black, white and a mid gray survive the conversion.
        assert_eq!(
            style("oklch(0 0 0)").unwrap().sgr.as_deref(),
            Some("38;2;0;0;0")
        );
        assert_eq!(
            style("oklch(1 0 0)").unwrap().sgr.as_deref(),
            Some("38;2;255;255;255")
        );
        let gray = style("oklch(0.5 0 0)").unwrap().sgr.unwrap();
        assert!(gray.starts_with("38;2;99;99;99"), "{gray}");
    }

    #[test]
    fn invalid_styles() {
        assert_eq!(style(""), Err(StyleError::Empty));
        assert!(matches!(style("red"), Err(StyleError::Color(_))));
        assert!(matches!(style("bold #12345"), Err(StyleError::Color(_))));
        assert!(matches!(style("#12345g"), Err(StyleError::Color(_))));
        assert_eq!(style("oklch(2 0 0)"), Err(StyleError::Oklch));
        assert_eq!(style("oklch(0.5 0)"), Err(StyleError::Oklch));
        assert_eq!(style("oklch(0.5 -1 0)"), Err(StyleError::Oklch));
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
