use super::*;
use anyhow::ensure;

/// Remove terminal controls from untrusted model and Tool text while retaining
/// ordinary whitespace and line structure.
pub(crate) fn safe_terminal_text(text: &str) -> String {
    text.chars()
        .filter(|ch| *ch == '\n' || *ch == '\r' || *ch == '\t' || !ch.is_control())
        .collect()
}

pub(crate) fn ansi_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM")
            .map(|term| term != "dumb")
            .unwrap_or(true)
}

pub(crate) fn truecolor_enabled() -> bool {
    if !ansi_enabled() || !std::io::stdout().is_terminal() {
        return false;
    }
    std::env::var("COLORTERM")
        .map(|value| matches!(value.to_ascii_lowercase().as_str(), "truecolor" | "24bit"))
        .unwrap_or(false)
}

#[derive(Clone, Debug)]
pub(crate) struct TerminalStyle {
    pub(crate) sgr: Option<String>,
}

impl TerminalStyle {
    #[cfg(test)]
    pub(crate) fn parse(spec: &str) -> Result<Self> {
        Self::parse_mode(spec, false)
    }

    fn parse_mode(spec: &str, background: bool) -> Result<Self> {
        let mut rest = spec.trim();
        if rest == "none" {
            return Ok(Self { sgr: None });
        }
        let mut codes: Vec<String> = Vec::new();
        loop {
            let mut consumed_attribute = false;
            for (name, code) in [
                ("bold", "1"),
                ("dim", "2"),
                ("italic", "3"),
                ("underline", "4"),
            ] {
                if rest == name {
                    codes.push(code.into());
                    rest = "";
                    consumed_attribute = true;
                    break;
                }
                if let Some(next) = rest
                    .strip_prefix(name)
                    .and_then(|next| next.strip_prefix(char::is_whitespace))
                {
                    codes.push(code.into());
                    rest = next.trim_start();
                    consumed_attribute = true;
                    break;
                }
            }
            if !consumed_attribute {
                break;
            }
        }
        if rest.is_empty() {
            ensure!(!codes.is_empty(), "style cannot be empty");
            return Ok(Self {
                sgr: Some(codes.join(";")),
            });
        }
        let (red, green, blue) = parse_terminal_color(rest)?;
        codes.extend([
            if background { "48" } else { "38" }.into(),
            "2".into(),
            red.to_string(),
            green.to_string(),
            blue.to_string(),
        ]);
        Ok(Self {
            sgr: Some(codes.join(";")),
        })
    }

    pub(crate) fn start(&self, enabled: bool) -> String {
        match (enabled, self.sgr.as_deref()) {
            (true, Some(sgr)) => format!("\x1b[{sgr}m"),
            _ => String::new(),
        }
    }

    pub(crate) fn paint(&self, enabled: bool, text: &str) -> String {
        let start = self.start(enabled);
        if start.is_empty() {
            text.to_owned()
        } else {
            format!("{start}{text}\x1b[0m")
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TerminalTheme {
    pub(crate) enabled: bool,
    pub(crate) styles: BTreeMap<&'static str, TerminalStyle>,
    input_text_spec: String,
    input_background_spec: String,
    input_selected_spec: String,
}

impl TerminalTheme {
    pub(crate) fn from_colors(colors: &agl_core::agent::PresentationColors) -> Result<Self> {
        let specs = [
            ("rule", colors.rule.as_str()),
            ("run", colors.run.as_str()),
            ("run_id", colors.run_id.as_str()),
            ("status_success", colors.status_success.as_str()),
            ("status_failure", colors.status_failure.as_str()),
            ("status_pending", colors.status_pending.as_str()),
            ("operation", colors.operation.as_str()),
            ("tool", colors.tool.as_str()),
            ("ordinal", colors.ordinal.as_str()),
            ("field", colors.field.as_str()),
            ("muted", colors.muted.as_str()),
            ("json_key", colors.json_key.as_str()),
            ("json_string", colors.json_string.as_str()),
            ("json_number", colors.json_number.as_str()),
            ("json_boolean", colors.json_boolean.as_str()),
            ("json_null", colors.json_null.as_str()),
            ("markdown_heading", colors.markdown_heading.as_str()),
            ("markdown_code", colors.markdown_code.as_str()),
            ("markdown_inline_code", colors.markdown_inline_code.as_str()),
            ("markdown_strong", colors.markdown_strong.as_str()),
            ("markdown_emphasis", colors.markdown_emphasis.as_str()),
            ("markdown_link", colors.markdown_link.as_str()),
            ("markdown_quote", colors.markdown_quote.as_str()),
            ("markdown_bullet", colors.markdown_bullet.as_str()),
            ("markdown_rule", colors.markdown_rule.as_str()),
            ("input_rule", colors.input_rule.as_str()),
            ("input_background", colors.input_background.as_str()),
            ("input_prompt", colors.input_prompt.as_str()),
            ("input_hint", colors.input_hint.as_str()),
            ("input_text", colors.input_text.as_str()),
            ("input_activity", colors.input_activity.as_str()),
            ("input_selected", colors.input_selected.as_str()),
        ];
        let styles = specs
            .into_iter()
            .map(|(role, spec)| {
                TerminalStyle::parse_mode(spec, role == "input_background")
                    .map(|style| (role, style))
                    .with_context(|| format!("invalid presentation.colors.{role}"))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        Ok(Self {
            enabled: truecolor_enabled(),
            styles,
            input_text_spec: colors.input_text.clone(),
            input_background_spec: colors.input_background.clone(),
            input_selected_spec: colors.input_selected.clone(),
        })
    }

    pub(crate) fn start(&self, role: &'static str) -> String {
        self.styles
            .get(role)
            .expect("known presentation color role")
            .start(self.enabled)
    }

    pub(crate) fn paint(&self, role: &'static str, text: &str) -> String {
        self.styles
            .get(role)
            .expect("known presentation color role")
            .paint(self.enabled, text)
    }

    pub(crate) fn reset(&self) -> &'static str {
        if self.enabled { "\x1b[0m" } else { "" }
    }
}

pub(crate) fn input_highlighter(theme: &TerminalTheme) -> Result<Box<dyn Highlighter>> {
    let style = parse_ansi_style(&theme.input_text_spec, Some(&theme.input_background_spec))?;
    Ok(Box::new(InputHighlighter { style }))
}

pub(crate) fn input_selection_style(theme: &TerminalTheme) -> Result<AnsiStyle> {
    parse_ansi_style(
        &theme.input_selected_spec,
        Some(&theme.input_background_spec),
    )
}

pub(crate) fn parse_ansi_style(foreground: &str, background: Option<&str>) -> Result<AnsiStyle> {
    type ParsedAnsiStyle = (AnsiStyle, Option<(u8, u8, u8)>);

    fn parse_component(spec: &str) -> Result<ParsedAnsiStyle> {
        let mut rest = spec.trim();
        let mut style = AnsiStyle::new();
        if rest == "none" {
            return Ok((style, None));
        }
        loop {
            let mut consumed = false;
            for name in ["bold", "dim", "italic", "underline"] {
                if rest == name {
                    style = match name {
                        "bold" => style.bold(),
                        "dim" => style.dimmed(),
                        "italic" => style.italic(),
                        _ => style.underline(),
                    };
                    rest = "";
                    consumed = true;
                    break;
                }
                if let Some(next) = rest
                    .strip_prefix(name)
                    .and_then(|next| next.strip_prefix(char::is_whitespace))
                {
                    style = match name {
                        "bold" => style.bold(),
                        "dim" => style.dimmed(),
                        "italic" => style.italic(),
                        _ => style.underline(),
                    };
                    rest = next.trim_start();
                    consumed = true;
                    break;
                }
            }
            if !consumed {
                break;
            }
        }
        if rest.is_empty() {
            ensure!(spec.trim() != "", "style cannot be empty");
            return Ok((style, None));
        }
        Ok((style, Some(parse_terminal_color(rest)?)))
    }

    let (mut style, foreground) = parse_component(foreground)?;
    if let Some(color) = foreground {
        style = style.fg(AnsiColor::Rgb(color.0, color.1, color.2));
    }
    if let Some(background) = background {
        let (_, color) = parse_component(background)?;
        if let Some(color) = color {
            style = style.on(AnsiColor::Rgb(color.0, color.1, color.2));
        }
    }
    Ok(style)
}

pub(crate) fn parse_terminal_color(value: &str) -> Result<(u8, u8, u8)> {
    if let Some(hex) = value.strip_prefix('#') {
        ensure!(hex.len() == 6, "hex color must use #RRGGBB");
        return Ok((
            u8::from_str_radix(&hex[0..2], 16)?,
            u8::from_str_radix(&hex[2..4], 16)?,
            u8::from_str_radix(&hex[4..6], 16)?,
        ));
    }
    let payload = value
        .strip_prefix("oklch(")
        .and_then(|value| value.strip_suffix(')'))
        .context("color must use #RRGGBB or oklch(L C H)")?;
    let values = payload
        .split_whitespace()
        .map(|value| value.parse::<f32>())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(values.len() == 3, "OKLCH requires three values");
    ensure!(
        values.iter().all(|value| value.is_finite())
            && (0.0..=1.0).contains(&values[0])
            && values[1] >= 0.0,
        "OKLCH lightness must be 0..1 and chroma non-negative"
    );
    Ok(oklch_to_srgb(values[0], values[1], values[2]))
}

pub(crate) fn oklch_to_srgb(lightness: f32, chroma: f32, hue: f32) -> (u8, u8, u8) {
    let hue = hue.to_radians();
    let a = chroma * hue.cos();
    let b = chroma * hue.sin();
    let l_ = lightness + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = lightness - 0.105_561_35 * a - 0.063_854_17 * b;
    let s_ = lightness - 0.089_484_18 * a - 1.291_485_5 * b;
    let l = l_ * l_ * l_;
    let m = m_ * m_ * m_;
    let s = s_ * s_ * s_;
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

pub(crate) fn answer_rule(theme: &TerminalTheme) -> String {
    answer_rule_for_width(theme, answer_width())
}

pub(crate) fn answer_footer(theme: &TerminalTheme, elapsed: Duration) -> String {
    answer_footer_for_width(theme, elapsed, answer_width())
}

pub(crate) fn answer_rule_for_width(theme: &TerminalTheme, width: usize) -> String {
    theme.paint("input_rule", &"─".repeat(width.max(1)))
}

pub(crate) fn answer_footer_for_width(
    theme: &TerminalTheme,
    elapsed: Duration,
    width: usize,
) -> String {
    let prefix = format!("─ Worked for {} ", format_worked_for(elapsed));
    let fill = width.saturating_sub(prefix.chars().count());
    theme.paint("input_rule", &format!("{prefix}{}", "─".repeat(fill)))
}

pub(crate) fn answer_width() -> usize {
    terminal::size()
        .map(|(columns, _)| usize::from(columns))
        .ok()
        .filter(|width| *width > 0)
        .unwrap_or(FALLBACK_ANSWER_WIDTH)
}

pub(crate) fn render_answer_block(text: &str, elapsed: Duration, theme: &TerminalTheme) {
    if send_tty(TtyMessage::AnswerFinal {
        text: text.to_owned(),
        elapsed,
    }) {
        return;
    }
    begin_external_block();
    let rendered = markdown_to_terminal(text, theme);
    // Transcript cards share the terminal width. The previous content-sized
    // rule made a later repaint wrap the already styled body and produced an
    // extra trailing `──` line.
    let width = answer_width();
    println!();
    println!("{}", answer_rule_for_width(theme, width));
    println!();
    println!("{}", rendered);
    println!("{}", answer_footer_for_width(theme, elapsed, width));
    println!();
}

pub(crate) fn terminal_visible_width(text: &str) -> usize {
    text.split('\n')
        .map(|line| {
            let mut width = 0;
            let mut cursor = 0;
            while cursor < line.len() {
                if line.as_bytes()[cursor] == 0x1b {
                    cursor = ansi_escape_end(line, cursor);
                    continue;
                }
                let ch = line[cursor..]
                    .chars()
                    .next()
                    .expect("cursor always points at a character boundary");
                width += UnicodeWidthChar::width(ch).unwrap_or(0);
                cursor += ch.len_utf8();
            }
            width
        })
        .max()
        .unwrap_or(0)
}

/// Return the end of one ANSI/ECMA-48 control sequence. In particular, the
/// `[` byte introduces a CSI sequence and is not its final byte; treating it
/// as the terminator leaves truecolor parameters counted as visible text.
pub(crate) fn ansi_escape_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut cursor = (start + 1).min(bytes.len());
    if cursor >= bytes.len() {
        return cursor;
    }
    match bytes[cursor] {
        b'[' => {
            cursor += 1;
            while cursor < bytes.len() {
                let byte = bytes[cursor];
                cursor += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
        }
        b']' => {
            cursor += 1;
            while cursor < bytes.len() {
                if bytes[cursor] == 0x07 {
                    cursor += 1;
                    break;
                }
                if bytes[cursor] == 0x1b && bytes.get(cursor + 1) == Some(&b'\\') {
                    cursor += 2;
                    break;
                }
                cursor += 1;
            }
        }
        _ => cursor += 1,
    }
    cursor
}
