//! Styles and the roles they are for: a style value such as `bold #7BD88F`,
//! the role a piece of the screen is painted for and its default (D121, D122,
//! D129). What paints them is the terminal's `theme` (AGL-067).

use crate::i18n::{Lang, Msg};
use crate::tr;

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

impl StyleError {
    /// The message of `self` in `lang` (AGL-121, D66).
    pub fn message(&self, lang: Lang) -> String {
        match self {
            Self::Empty => tr!(lang, STYLE_EMPTY).to_string(),
            Self::Color(color) => tr!(lang, STYLE_COLOR, color = color),
            Self::Oklch => tr!(lang, STYLE_OKLCH).to_string(),
        }
    }
}

/// `StyleError`'s problems, for `settings::error::Error::InvalidStyle`.
const STYLE_EMPTY: Msg = Msg {
    en: "a style cannot be empty",
    uk: "стиль не може бути порожнім",
};
const STYLE_COLOR: Msg = Msg {
    en: "`{color}` is not #RRGGBB or oklch(L C H)",
    uk: "`{color}` не є #RRGGBB або oklch(L C H)",
};
const STYLE_OKLCH: Msg = Msg {
    en: "oklch needs three numbers, lightness 0..1 and chroma >= 0",
    uk: "oklch потребує трьох чисел, яскравість 0..1, насиченість >= 0",
};

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

    pub fn start(&self, enabled: bool) -> String {
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

            pub fn default_spec(self) -> &'static str {
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
    InputSelected "input_selected" "bold oklch(0.518 0.253 323.949)",
    FocusBackground "focus_background" "oklch(0.33 0 0)",
    Muted "muted" "dim",
    Thinking "thinking" "italic dim",
    ThinkingMuted "thinking_muted" "dim",
    Tool "tool" "bold #EC4899",
    Activity "activity" "bold #EC4899",
    Summary "summary" "bold #EC4899",
    StatusSuccess "status_success" "bold #7BD88F",
    StatusFailure "status_failure" "bold #FF0000",
    StatusWarn "status_warn" "bold #FFA500",
    StatusPending "status_pending" "bold #FFFF00",
    MarkdownHeading "markdown_heading" "bold #EC4899",
    MarkdownCode "markdown_code" "dim",
    MarkdownInlineCode "markdown_inline_code" "#FFFF00",
    MarkdownStrong "markdown_strong" "bold",
    MarkdownEmphasis "markdown_emphasis" "italic",
    MarkdownLink "markdown_link" "#8AA2D8",
    MarkdownQuote "markdown_quote" "dim",
    MarkdownRule "markdown_rule" "dim",
}

impl Role {
    /// Whether the style sets a background color.
    pub fn is_background(self) -> bool {
        matches!(self, Role::InputBackground | Role::FocusBackground)
    }
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
}
