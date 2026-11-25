//! How the session's blocks are drawn: the primitives — a label's row,
//! the body under the elbow, the fold — and the cards of a step, drawn
//! once here and asked by the transcript with its detail levels (AGL-092,
//! AGL-095) and by the inspect mode with its focus (AGL-066, D4;
//! AGL-121, D14). On the focus a block prints whole, in the colors of a
//! message's text (AGL-085, D3; AGL-098, D2); off it the block is gray
//! and its accents come only back on the focus (AGL-105, D1).

use super::markdown;
use super::text::{clean, hard_wrap, width, wrap_line};
use agentlibre_proto::ToolCall;
use agentlibre_common::i18n::{Lang, m};
use crate::records;
use agentlibre_common::settings::Detail;
use agentlibre_common::style::Role;
use crate::theme::Theme;
use crate::records::ToolResult;
use agentlibre_common::tr;

/// An event's notice as a block (AGL-099, D1): the label in the tool's
/// color, the rest under the elbow — the look of the THINKING and RESULT
/// blocks; a label with no body prints alone, with no placeholder. What
/// carries no label of its own — the server's line, a command's answer,
/// an error — hangs under the elbow too (AGL-106, D2), in its own role.
pub fn notice_block_lines(
    theme: &Theme,
    lang: Lang,
    columns: usize,
    text: &str,
    role: Role,
) -> Vec<String> {
    let Some((label, body)) = crate::status::notice_parts(text, lang) else {
        let inner = columns.saturating_sub(BODY_PREFIX).max(1);
        let rows = text
            .lines()
            .flat_map(|line| hard_wrap(&clean(line), inner))
            .map(|row| theme.paint(role, &row))
            .collect();
        return elbow_rows(theme, rows);
    };
    let mut lines = vec![theme.paint(Role::Tool, &label)];
    if body.trim().is_empty() {
        return lines;
    }
    let inner = columns.saturating_sub(BODY_PREFIX).max(1);
    let mut rows = Vec::new();
    for line in clean(&body).lines() {
        for row in hard_wrap(line, inner) {
            rows.push(theme.paint(role, &row));
        }
    }
    lines.extend(elbow_rows(theme, rows));
    lines
}

pub fn notice_lines(theme: &Theme, text: &str, columns: usize, role: Role) -> Vec<String> {
    text.lines()
        .flat_map(|line| hard_wrap(&clean(line), columns))
        .map(|line| theme.paint(role, &line))
        .collect()
}

/// A step's row: its status and its items over ` · `, with no rule around
/// them (AGL-066, D26; AGL-080, D1), the items in the summary's role
/// (AGL-121, D16 d). A row longer than the width wraps as the terminal
/// lets it (AGL-045).
pub fn summary(theme: &Theme, status: Option<(&str, Role)>, items: &[String]) -> String {
    let mut parts: Vec<(String, Role)> = Vec::new();
    if let Some((text, status_role)) = status {
        parts.push((text.into(), status_role));
        if !items.is_empty() {
            parts.push((" · ".into(), Role::Summary));
        }
    }
    if !items.is_empty() {
        parts.push((items.join(" · "), Role::Summary));
    }
    parts
        .iter()
        .map(|(text, role)| theme.paint(*role, text))
        .collect()
}

/// The user's message as rows on the input background (D138). Its marker is
/// the input's own, two columns wide (AGL-094, D1), so a message of the
/// human and what he types begin at the same column. The text takes `role`:
/// a message the color of the input, a command the tool's (AGL-101).
pub fn user_lines(theme: &Theme, message: &str, columns: usize, role: Role) -> Vec<String> {
    let columns = columns.max(4);
    let background = theme.start(Role::InputBackground);
    let reset = theme.reset();
    let mut rows = Vec::new();
    for line in clean(message).trim_end().split('\n') {
        rows.extend(wrap_line(line, columns - 2));
    }
    rows.iter()
        .enumerate()
        .map(|(index, row)| {
            let marker = if index == 0 { "› " } else { "  " };
            let pad = " ".repeat(columns - 2 - width(row));
            format!(
                "{background}{}{background}{}{background}{pad}{reset}",
                theme.paint(Role::InputPrompt, marker),
                theme.paint(role, row)
            )
        })
        .collect()
}

/// The lines of the thinking the transcript shows by default (AGL-068);
/// the full text is in the inspect mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fold {
    /// Every row of it.
    Whole,
    /// Its head and its tail, with `⋯` between them (AGL-092, D4).
    Folded,
}

/// The rows of the thinking's head: as many of its first paragraph as these
/// allow (AGL-092, D4).
const THINKING_HEAD: usize = 3;
/// The rows of the thinking's tail: as many of its last rows as these
/// allow, two more when that is where a paragraph begins.
const THINKING_TAIL: usize = 5;
/// A fold that would hide fewer rows than these is not worth a row of `⋯`.
const THINKING_FOLD_MIN: usize = 3;

/// A row of a folded block: the thinking's own, or the count of the rows
/// that are hidden.
enum Shown {
    Row(String),
    Hidden(usize),
}

/// Whether a block renders the thinking's Markdown (AGL-093, D1): the
/// printed card does, the live block does not — that is redrawn with every
/// piece, and the whole of the text with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Render {
    Plain,
    Markdown,
}

/// The rows of a block, each wrapped to `inner` and painted for `text_role`,
/// with the row where every paragraph of it begins: a run of its non-empty
/// lines, as the model or the tool wrote them (AGL-092, D4; AGL-093, D1;
/// AGL-095, D2).
fn block_rows(
    text: &str,
    inner: usize,
    theme: &Theme,
    text_role: Role,
    render: Render,
) -> (Vec<String>, Vec<usize>) {
    let mut rows: Vec<String> = Vec::new();
    let mut starts: Vec<usize> = Vec::new();
    let mut paragraph: Vec<String> = Vec::new();
    let mut flush = |paragraph: &mut Vec<String>, rows: &mut Vec<String>| {
        if paragraph.is_empty() {
            return;
        }
        starts.push(rows.len());
        match render {
            Render::Plain => rows.extend(paragraph.iter().flat_map(|line| {
                hard_wrap(line, inner).into_iter().map(|row| theme.paint(text_role, &row))
            })),
            Render::Markdown => rows.extend(
                markdown::render(&paragraph.join("\n"), inner, theme)
                    .into_iter()
                    .map(|row| theme.on_style(text_role, &row)),
            ),
        }
        paragraph.clear();
    };
    for line in text.lines() {
        if line.trim().is_empty() {
            flush(&mut paragraph, &mut rows);
            continue;
        }
        paragraph.push(clean(line));
    }
    flush(&mut paragraph, &mut rows);
    (rows, starts)
}

/// The rows a folded thinking keeps (AGL-092, D4): the first paragraph's
/// first rows, `⋯ n` for what lies between, and its last rows — the tail
/// reaching back to the start of its paragraph when two rows buy that, so
/// it never opens in the middle of a paragraph and always shows how the
/// thinking ended. A thinking with no room for `⋯` shows all of it.
fn fold_rows(rows: &[String], starts: &[usize]) -> Vec<Shown> {
    let total = rows.len();
    let all = || rows.iter().map(|row| Shown::Row(row.clone())).collect();
    // The rows of the first paragraph: what ends it is where the second
    // one begins.
    let paragraph = *starts.get(1).unwrap_or(&total);
    let head = paragraph.min(THINKING_HEAD);
    if total < head + THINKING_TAIL + THINKING_FOLD_MIN {
        return all();
    }
    let mut tail = total - THINKING_TAIL;
    if let Some(start) = starts.iter().rev().find(|start| **start < tail)
        && tail - start <= 2
    {
        tail = *start;
    }
    let Some(hidden) = tail.checked_sub(head) else {
        return all();
    };
    let mut kept: Vec<Shown> = rows[..head]
        .iter()
        .map(|row| Shown::Row(row.clone()))
        .collect();
    kept.push(Shown::Hidden(hidden));
    kept.extend(rows[tail..].iter().map(|row| Shown::Row(row.clone())));
    kept
}

/// The lines of one block (AGL-104, D1): the label in the tool's color,
/// and its rows below, painted for `text_role` (D23, D24; AGL-095, D2) —
/// all of them, or the head and the tail of them, or, when the block has no
/// text of its own, the placeholder that keeps its place (AGL-095, D4).
/// The body hangs under `⎿ `, the rows that follow stand under it.
#[allow(clippy::too_many_arguments)]
pub fn block_lines(
    theme: &Theme,
    lang: Lang,
    columns: usize,
    label: &str,
    text: &str,
    text_role: Role,
    fold: Fold,
    render: Render,
) -> Vec<String> {
    let inner = columns.saturating_sub(BODY_PREFIX).max(1);
    let (rows, starts) = block_rows(text, inner, theme, text_role, render);
    let kept = match fold {
        Fold::Whole => rows.iter().map(|row| Shown::Row(row.clone())).collect(),
        Fold::Folded => fold_rows(&rows, &starts),
    };
    let mut lines = vec![theme.paint(Role::Tool, label)];
    if rows.is_empty() {
        let placeholder = theme.paint(Role::Muted, tr!(lang, m::EMPTY));
        lines.extend(elbow_rows(theme, vec![placeholder]));
        return lines;
    }
    let kept = kept
        .into_iter()
        .map(|row| match row {
            Shown::Row(row) => row,
            Shown::Hidden(n) => theme.paint(Role::Muted, &tr!(lang, m::THINKING_FOLDED, n = n)),
        })
        .collect();
    lines.extend(elbow_rows(theme, kept));
    lines
}

/// The width of the body's prefix (AGL-104, D1): `⎿ ` before a block's
/// first row, two spaces before the rows that follow.
pub const BODY_PREFIX: usize = 2;

/// The body's rows with their prefix (AGL-104, D1; AGL-106, D1): the elbow
/// in the muted color — dark, whatever block it begins — before the first
/// row, and two spaces before the rest, so they all stand under it.
pub fn elbow_rows(theme: &Theme, rows: Vec<String>) -> Vec<String> {
    let elbow = theme.paint(Role::Muted, "⎿ ");
    rows.into_iter()
        .enumerate()
        .map(|(n, row)| {
            format!(
                "{}{row}",
                if n == 0 {
                    elbow.as_str()
                } else {
                    "  "
                }
            )
        })
        .collect()
}

/// The thinking's block (D3, D19, D24; AGL-092, D2): the label in the
/// tool's color, the rows under the gutter — all of them, or the head
/// and the tail, or nothing at all when `detail` is `off` and the block
/// has no focus (AGL-099, D2). Gray off the focus, its own and the
/// Markdown's accents alike; on the focus whole, in the colors of a
/// message's text (AGL-085, D3; AGL-105, D1).
pub fn thinking(
    theme: &Theme,
    lang: Lang,
    columns: usize,
    text: &str,
    detail: Detail,
    focused: bool,
) -> Vec<String> {
    if detail == Detail::Off && !focused {
        return Vec::new();
    }
    let gray = theme.gray();
    block_lines(
        if focused { theme } else { &gray },
        lang,
        columns,
        tr!(lang, m::THINKING),
        text,
        if focused { Role::InputText } else { Role::Thinking },
        if focused || detail == Detail::Full {
            Fold::Whole
        } else {
            Fold::Folded
        },
        Render::Markdown,
    )
}

/// The text the model wrote before a call (D9): its first three lines as
/// Markdown, with `…` when more; whole on the inspect focus (AGL-098,
/// D4). It stands inside its block, above the card, so it ends with no
/// blank row (AGL-085, D1).
pub fn text(theme: &Theme, columns: usize, text: &str, focused: bool) -> Vec<String> {
    let mut lines = markdown::render(text.trim(), columns, theme);
    if !focused && lines.len() > 3 {
        lines.truncate(3);
        lines
            .last_mut()
            .expect("three lines")
            .push_str(&theme.paint(Role::Muted, "…"));
    }
    lines
}

/// The card of a call (AGL-066, D1, D6; AGL-080, D1; AGL-095, D2, D5;
/// AGL-121, D14): the tool's name and status, its input's rows under the
/// elbow, and the result's block when `result_detail` asks for one. Off
/// the focus `detail` says how much of the input to show — nothing, the
/// hint, or the whole of what was sent (AGL-095, D5); on it the whole
/// input stands in the colors of a message's text (AGL-098, D4). What
/// the surfaces add themselves: the row of the phases' times, and the
/// blank row with which the transcript ends its block (AGL-085, D1).
#[allow(clippy::too_many_arguments)]
pub fn tool(
    theme: &Theme,
    lang: Lang,
    columns: usize,
    call: &ToolCall,
    result: Option<&ToolResult>,
    hint: &str,
    detail: Detail,
    result_detail: Detail,
    focused: bool,
) -> Vec<String> {
    let (status_text, status_role) = records::status(result.map(|result| result.outcome));
    let mut lines = vec![format!(
        "{}  {}",
        theme.paint(
            Role::Tool,
            &format!("{} {}", tr!(lang, m::TOOL_LABEL), call.name)
        ),
        theme.paint(status_role, &status_text)
    )];
    // AGL-068: the row under the gutter says what the call did — the
    // command, the file, the query, or the action of a forge or a
    // memory call — in the muted color, as the thinking's (D26); on the
    // focus, as the thinking's there, in the color of a message's text.
    // AGL-095, D5: `off` says nothing of it, `flexible` its one line,
    // `full` — as the focus — the whole of what was sent.
    let input = if focused || detail == Detail::Full {
        records::call_input(call)
    } else if detail == Detail::Off {
        String::new()
    } else {
        hint.to_owned()
    };
    let inner = columns.saturating_sub(BODY_PREFIX).max(1);
    let mut rows = Vec::new();
    for line in clean(&input).lines() {
        for row in hard_wrap(line, inner) {
            rows.push(theme.paint(
                if focused {
                    Role::InputText
                } else {
                    Role::Muted
                },
                &row,
            ));
        }
    }
    lines.extend(elbow_rows(theme, rows));
    // AGL-095, D2: the tool's answer, at the tool's own level, folded by
    // the thinking's rule; `off` keeps it out of the card.
    if result_detail != Detail::Off {
        let content = result
            .map(|result| result.content.as_str())
            .unwrap_or_default();
        lines.extend(self::result(
            theme,
            lang,
            columns,
            &clean(content),
            result_detail,
            false,
        ));
    }
    lines
}

/// The result's block (AGL-095, D2; AGL-098, D2): folded by `detail`,
/// whole on the focus, where it stands in the colors of a message's
/// text. `content` is the result's text; taking the whole of it from
/// the output's file when the call's output was cut (AGL-049) is the
/// inspect mode's choice, not the block's.
pub fn result(
    theme: &Theme,
    lang: Lang,
    columns: usize,
    content: &str,
    detail: Detail,
    focused: bool,
) -> Vec<String> {
    block_lines(
        theme,
        lang,
        columns,
        tr!(lang, m::RESULT_LABEL),
        content,
        if focused {
            Role::InputText
        } else {
            Role::Muted
        },
        if focused || detail == Detail::Full {
            Fold::Whole
        } else {
            Fold::Folded
        },
        Render::Plain,
    )
}

/// The answer's block (AGL-080, D2; AGL-097; AGL-100; AGL-104, D1): its
/// text as Markdown, set off by no rows of its own; `head` gives it the
/// `ANSWER` label with the body under the elbow, as the other blocks
/// have — the inspect mode needs a head for the focus to stand on.
/// `time` is the row of the turn's times below the text, in the tool's
/// color; no time, no row.
pub fn answer(
    theme: &Theme,
    lang: Lang,
    columns: usize,
    text: &str,
    time: Option<String>,
    head: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    if head {
        lines.push(theme.paint(Role::Tool, tr!(lang, m::ANSWER_LABEL)));
        let body = markdown::render(
            text.trim(),
            columns.saturating_sub(BODY_PREFIX).max(1),
            theme,
        );
        lines.extend(elbow_rows(theme, body));
    } else {
        lines.extend(markdown::render(text.trim(), columns, theme));
    }
    lines.extend(time);
    lines
}
