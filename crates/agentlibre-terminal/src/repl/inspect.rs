//! The inspect mode of the session (AGL-066, D4, D5, D8): the alternate
//! screen on which the records of the session are drawn in full — the
//! outputs of the tools, the thinking, the text before a call — with the
//! navigation between the records and the messages of the human. Leaving
//! it puts the transcript back as it was.

use std::collections::BTreeSet;
use std::path::Path;

use super::markdown;
use crate::records::{self, Record};
use super::term::{self, Key};
use super::text::{clean, hard_wrap, truncate};
use super::transcript::{Fold, notice_lines, summary, thinking_lines, user_lines};
use crate::action::Value;
use crate::backend::ToolCall;
use crate::i18n::{Lang, m, repl as texts};
use crate::session::{Event, TransitionLine};
use crate::style::Role;
use crate::theme::Theme;
use crate::tools::ToolResult;
use crate::tr;

/// The rows the screen keeps for the blank one and the hint (D8).
const RESERVE: usize = 2;

/// The records of a session and the navigation over them (D4, D5, D8).
pub struct Inspect {
    records: Vec<Record>,
    /// The record the navigation points at.
    selection: usize,
    /// The records shown in full (D4).
    expanded: BTreeSet<usize>,
    /// The tail of the session is kept on screen (D8: `G`).
    follow: bool,
    /// The first row of the window, to keep it steady between redraws.
    top: Option<usize>,
}

impl Inspect {
    pub fn new(events: &[Event], transitions: &[TransitionLine], lang: Lang) -> Inspect {
        let records = records::group(events, transitions, lang);
        let selection = records.len().saturating_sub(1);
        Inspect {
            records,
            selection,
            expanded: BTreeSet::new(),
            follow: true,
            top: None,
        }
    }

    /// Takes a key (D8); false when the mode is over.
    pub fn key(&mut self, key: Key) -> bool {
        match key {
            Key::Escape => false,
            Key::Up => {
                self.follow = false;
                if self.selection > 0 {
                    self.selection -= 1;
                }
                true
            }
            Key::Down => {
                self.follow = false;
                if self.selection + 1 < self.records.len() {
                    self.selection += 1;
                }
                true
            }
            Key::Enter | Key::Right => {
                self.expanded.insert(self.selection);
                true
            }
            Key::Left => {
                self.expanded.remove(&self.selection);
                true
            }
            Key::Char('u') => {
                self.jump_to_user(false);
                true
            }
            Key::Char('U') => {
                self.jump_to_user(true);
                true
            }
            Key::Char('g') => {
                self.selection = 0;
                self.follow = false;
                true
            }
            Key::Char('G') => {
                self.selection = self.records.len().saturating_sub(1);
                self.follow = true;
                true
            }
            _ => true,
        }
    }

    /// `u` and `U`: the previous and the next message of the human (D5);
    /// the navigation stays when there is none.
    fn jump_to_user(&mut self, next: bool) {
        let found = if next {
            (self.selection + 1..self.records.len())
                .find(|&index| is_user(&self.records[index]))
        } else {
            (0..self.selection)
                .rev()
                .find(|&index| is_user(&self.records[index]))
        };
        if let Some(index) = found {
            self.selection = index;
            self.follow = false;
        }
    }

    /// The whole screen: the window of the records, the blank row and the
    /// hint (D4, D8), the dock's rows at the bottom (D14), exactly
    /// `rows` rows.
    pub fn render(
        &self,
        theme: &Theme,
        lang: Lang,
        columns: usize,
        rows: usize,
        outputs: &Path,
        dock: &[String],
    ) -> Vec<String> {
        let body = rows
            .saturating_sub(RESERVE)
            .saturating_sub(dock.len())
            .max(1);
        let (content, starts) = self.content(theme, lang, columns, outputs);
        let len = content.len();
        let max_top = len.saturating_sub(body.min(len));
        let top = if self.follow {
            max_top
        } else {
            let mut top = self.top.unwrap_or(max_top).min(max_top);
            if let Some(start) = starts.get(self.selection) {
                let start = *start;
                let end = starts.get(self.selection + 1).copied().unwrap_or(len);
                if start < top {
                    top = start;
                } else if end > top + body {
                    top = end.saturating_sub(body);
                }
            }
            top
        };
        let mut lines: Vec<String> = content[top..(top + body).min(len)].to_vec();
        lines.resize(body, String::new());
        lines.push(String::new());
        lines.push(self.hint(theme, lang, columns));
        for row in dock {
            if lines.len() >= rows {
                break;
            }
            lines.push(row.clone());
        }
        lines.resize(rows, String::new());
        lines
    }

    /// The lines of the records and, per record, the line it starts at.
    fn content(
        &self,
        theme: &Theme,
        lang: Lang,
        columns: usize,
        outputs: &Path,
    ) -> (Vec<String>, Vec<usize>) {
        let mut lines = Vec::new();
        let mut starts = Vec::new();
        for (index, record) in self.records.iter().enumerate() {
            if index > 0 {
                lines.push(String::new());
            }
            starts.push(lines.len());
            let mut block = self.block(record, index, theme, lang, columns, outputs);
            // The focus's marker at the head of the record the
            // navigation points at (D13), on the focus's background (D16).
            if index == self.selection
                && let Some(first) = block.first_mut()
            {
                let head = format!("{} ", theme.paint(Role::InputSelected, "▸"));
                // The user's row pads itself to the width: trim its
                // trailing spaces so the marker fits (D16).
                let row = if super::text::width(first)
                    > columns.saturating_sub(super::text::width(&head))
                {
                    super::text::trim_trailing_spaces(first)
                } else {
                    first.clone()
                };
                let marked = format!("{head}{row}");
                let pad = " ".repeat(columns.saturating_sub(super::text::width(&marked)));
                *first = theme.on_background(Role::FocusBackground, &format!("{marked}{pad}"));
            }
            lines.extend(block);
        }
        (lines, starts)
    }

    /// One record: its compact lines, or its full data when it is
    /// expanded (D1, D3, D4, D9).
    fn block(
        &self,
        record: &Record,
        index: usize,
        theme: &Theme,
        lang: Lang,
        columns: usize,
        outputs: &Path,
    ) -> Vec<String> {
        let expanded = self.expanded.contains(&index);
        let focused = index == self.selection;
        match record {
            Record::User(message) => user_lines(theme, message, columns),
            Record::State(state) => vec![theme.paint(Role::Muted, &format!("[{state}]"))],
            Record::Tool {
                call,
                result,
                hint,
                phases,
                time,
                thinking,
                text,
            } => {
                let mut lines = step_lines(
                    theme,
                    lang,
                    columns,
                    expanded,
                    focused,
                    thinking.as_deref(),
                    text.as_deref(),
                );
                let mut items = records::phase_items(phases);
                if let Some(elapsed) = time {
                    items.push(format!(
                        "{} {}",
                        call.name,
                        texts::duration(elapsed.as_secs())
                    ));
                }
                lines.extend(card_lines(theme, lang, columns, call, result.as_ref(), hint, &items));
                if expanded {
                    // The input, in full, when the hint is not the whole (D4).
                    let input = call_input(call);
                    if input != *hint {
                        for row in hard_wrap(&clean(&input), columns) {
                            lines.push(theme.paint(Role::Muted, &row));
                        }
                    }
                    lines.push(String::new());
                    lines.extend(tool_output(call, result.as_ref(), outputs, columns));
                }
                lines
            }
            Record::Answer {
                text,
                time,
                thinking,
            } => {
                let mut lines = step_lines(
                    theme,
                    lang,
                    columns,
                    expanded,
                    focused,
                    thinking.as_deref(),
                    None,
                );
                lines.extend(markdown::render(text.trim(), columns, theme));
                if expanded {
                    let items: Vec<String> = time
                        .iter()
                        .map(|elapsed| {
                            tr!(
                                lang,
                                m::WORKED_FOR,
                                time = texts::duration(elapsed.as_secs())
                            )
                        })
                        .collect();
                    if !items.is_empty() {
                        // AGL-080 D2, AGL-087 D1: the answer's times are the
                        // tool's color, as `THINKING` and `TOOL bash` are.
                        lines.push(summary(theme, None, &items, Role::Tool));
                    }
                }
                lines
            }
            Record::Notice { line, role, detail } => {
                let mut lines = notice_lines(theme, line, columns, *role);
                if expanded
                    && let Some(detail) = detail
                    && !detail.is_empty()
                {
                    lines.push(String::new());
                    lines.extend(
                        detail
                            .lines()
                            .flat_map(|line| hard_wrap(&clean(line), columns))
                            .map(|line| theme.paint(Role::Muted, &line)),
                    );
                }
                lines
            }
        }
    }

    /// The position, the follow and the keys' hint (D8); the position is
    /// first, so a narrow screen keeps it.
    fn hint(&self, theme: &Theme, lang: Lang, columns: usize) -> String {
        let rest = if self.follow {
            format!("{} · {}", tr!(lang, m::INSPECT_FOLLOW), tr!(lang, m::INSPECT_HINT))
        } else {
            tr!(lang, m::INSPECT_HINT).into()
        };
        let hint = format!("{}/{} · {}", self.selection + 1, self.records.len(), rest);
        theme.paint(Role::Muted, &truncate(&hint, columns))
    }
}

/// Whether the record is a message of the human (D5).
fn is_user(record: &Record) -> bool {
    matches!(record, Record::User(_))
}

/// The thinking and the text of a step's record, before its line (D19):
/// the thinking's line in the tool's style when the record is compact,
/// the full data when it is expanded.
fn step_lines(
    theme: &Theme,
    lang: Lang,
    columns: usize,
    expanded: bool,
    focused: bool,
    thinking: Option<&str>,
    text: Option<&str>,
) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(thinking) = thinking {
        // D23, AGL-085 D3: the thinking's rows are muted, but on the record
        // the navigation points at they take the color of a message's text.
        let role = if focused { Role::InputText } else { Role::Muted };
        if expanded || focused {
            // D24: like the tool's card — the label, and every line of the
            // thinking under the gutter.
            let gutter = theme.paint(Role::InputRule, "  │ ");
            let inner = columns.saturating_sub(4).max(1);
            lines.push(theme.paint(Role::Tool, tr!(lang, m::THINKING)));
            for line in thinking
                .lines()
                .flat_map(|line| hard_wrap(&clean(line), inner))
            {
                lines.push(format!("{gutter}{}", theme.paint(role, &line)));
            }
        } else {
            lines.extend(thinking_lines(
                theme,
                lang,
                columns,
                thinking,
                Role::Tool,
                role,
                Fold::Folded,
            ));
        }
    }
    if let Some(text) = text {
        let mut block = markdown::render(text.trim(), columns, theme);
        if !expanded && block.len() > 3 {
            block.truncate(3);
            block
                .last_mut()
                .expect("three lines")
                .push_str(&theme.paint(Role::Muted, "…"));
        }
        lines.extend(block);
    }
    lines
}

/// The compact card of a call, as the inspect mode draws it (D6; AGL-080,
/// D1): the tool's line with its status, the lines of the input, the row of
/// its phases.
fn card_lines(
    theme: &Theme,
    lang: Lang,
    columns: usize,
    call: &ToolCall,
    result: Option<&ToolResult>,
    hint: &str,
    items: &[String],
) -> Vec<String> {
    let gutter = theme.paint(Role::InputRule, "  │ ");
    let inner = columns.saturating_sub(4).max(1);
    let (status_text, status_role) = records::status(result.map(|result| result.outcome));
    // AGL-068: the action of a forge or a memory call is in the hint row.
    let mut lines = vec![format!(
        "{}  {}",
        theme.paint(
            Role::Tool,
            &format!("{} {}", tr!(lang, m::TOOL_LABEL), call.name)
        ),
        theme.paint(status_role, &status_text)
    )];
    // D26: the tool's row, like the thinking's, in the muted color.
    if !hint.is_empty() {
        for row in hard_wrap(&clean(hint), inner) {
            lines.push(format!("{gutter}{}", theme.paint(Role::Muted, &row)));
        }
    }
    // D27, AGL-080, AGL-087 D1: the phases in the tool's color, no rule.
    lines.push(summary(
        theme,
        Some((status_text.as_str(), status_role)),
        items,
        Role::Tool,
    ));
    lines
}

/// The full output of a call, when it is known (D4): the whole of it from
/// the output's file when the call's output was cut (AGL-049), else the
/// result's content.
fn tool_output(
    call: &ToolCall,
    result: Option<&ToolResult>,
    outputs: &Path,
    columns: usize,
) -> Vec<String> {
    let Some(result) = result else {
        return Vec::new();
    };
    let file = outputs.join(format!("{}.txt", call.id));
    let text = if file.exists() {
        match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(_) => result.content.clone(),
        }
    } else {
        result.content.clone()
    };
    text.lines()
        .flat_map(|line| hard_wrap(&clean(line), columns))
        .collect()
}

/// What a call sent: the command, the edit with its lines, the search's
/// query, the summary of a write to the forge or the memory.
fn call_input(call: &ToolCall) -> String {
    let text = |name: &str| call.text(name).unwrap_or_default();
    if call.name == "edit" {
        let mut out = text("path").to_owned();
        for (sign, name) in [('-', "old"), ('+', "new")] {
            for line in text(name).split('\n') {
                out.push_str(&format!("\n{sign} {line}"));
            }
        }
        return out;
    }
    if call.name == "search" {
        return format!(
            "{}: {}",
            text("source"),
            text("query")
        );
    }
    if call.name == "memory" || call.name == "forge" {
        // The whole input, argument by argument (D11); the summary
        // stays the compact form.
        let mut out = String::new();
        for (key, value) in &call.args {
            let Value::Text(value) = value else {
                continue;
            };
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("{key}: {value}"));
        }
        return out;
    }
    text("command").to_owned()
}

/// The inspect mode on the terminal (D4, D8): the alternate screen, the
/// full redraws, the keys until the escape. `write` is what the mode
/// draws with; `dock` is the dock's rows, drawn at the bottom so the
/// input prompt does not disappear (D14).
#[allow(clippy::too_many_arguments)]
pub fn run(
    write: &dyn Fn(&str),
    theme: &Theme,
    lang: Lang,
    events: Vec<Event>,
    transitions: Vec<TransitionLine>,
    outputs: std::path::PathBuf,
    dock: Vec<String>,
) {
    let mut inspect = Inspect::new(&events, &transitions, lang);
    let mut parser = term::Parser::default();
    let mut buffer = [0u8; 4096];
    let mut size = term::size();
    write(term::ALT_ON);
    loop {
        if term::take_resized() {
            size = term::size();
        }
        draw(write, &inspect, theme, lang, size.0, size.1, &outputs, &dock);
        let keys = loop {
            match term::read_input(&mut buffer, 100) {
                Ok(None) => {
                    if parser.waiting() {
                        break parser.timeout();
                    }
                    continue;
                }
                // The end of input or a failed read leaves the mode.
                Ok(Some(0)) | Err(_) => break vec![Key::Escape],
                Ok(Some(n)) => break parser.feed(&buffer[..n]),
            }
        };
        let mut open = true;
        for key in keys {
            if !inspect.key(key) {
                open = false;
                break;
            }
        }
        if !open {
            break;
        }
    }
    write(term::ALT_OFF);
}

/// One redraw of the screen: the clear, the lines, the cursor at the
/// bottom. The lines carry no trailing newline: a last one would scroll
/// the screen up by a row and cut the window's first row (AGL-066, D15).
#[allow(clippy::too_many_arguments)]
fn draw(
    write: &dyn Fn(&str),
    inspect: &Inspect,
    theme: &Theme,
    lang: Lang,
    columns: usize,
    rows: usize,
    outputs: &Path,
    dock: &[String],
) {
    let lines = inspect.render(theme, lang, columns, rows, outputs, dock);
    let mut text = String::from(term::CLEAR);
    text.push_str(term::HOME);
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        text.push_str(line);
    }
    text.push_str(&term::cursor_at(rows.max(1), 1));
    write(&text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Outcome;

    fn call(id: &str, command: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "bash".into(),
            args: vec![("command".into(), Value::Text(command.into()))],
        }
    }

    /// The records: 0 the user, 1 the step (the thinking, the text and
    /// the call), 2 the answer, 3 the user, 4 the compaction, 5 the
    /// answer.
    fn session() -> Vec<Event> {
        vec![
            Event::User("First".into()),
            Event::Thinking("let me look\nand more".into()),
            Event::Call {
                text: Some("I will run it.\nAnd explain why.\nAnd more.".into()),
                call: call("c1", "ls"),
            },
            Event::Result {
                id: "c1".into(),
                outcome: Outcome::Ok,
                content: "a\nb\nc\nd\ne".into(),
            },
            Event::Answer("Done.".into()),
            Event::User("Second".into()),
            Event::Compact {
                summary: "the summary of the long session".into(),
                question: None,
            },
            Event::Answer("Ok.".into()),
        ]
    }

    fn theme() -> Theme {
        Theme::plain()
    }

    fn inspect() -> Inspect {
        Inspect::new(&session(), &[], Lang::En)
    }

    /// The screen of `rows` rows, so that the records do not all fit.
    fn screen(inspect: &Inspect, rows: usize) -> Vec<String> {
        screen_dock(inspect, rows, &[])
    }

    /// The screen of `rows` rows with the dock's rows at the bottom.
    fn screen_dock(
        inspect: &Inspect,
        rows: usize,
        dock: &[String],
    ) -> Vec<String> {
        inspect.render(&theme(), Lang::En, 40, rows, &std::env::temp_dir(), dock)
    }

    #[test]
    fn the_tail_is_on_screen_at_the_start() {
        let inspect = inspect();
        let lines = screen(&inspect, 10);
        assert_eq!(lines.len(), 10);
        let all = lines.join("\n");
        // The last answer and the compaction's line; the hint has the
        // position and the follow.
        assert!(all.contains("Ok."), "{all}");
        assert!(all.contains("Compacting"), "{all}");
        assert!(lines.last().unwrap().contains("6/6"));
        assert!(lines.last().unwrap().contains("follow"));
    }

    #[test]
    fn the_keys_move_and_expand() {
        let mut inspect = inspect();
        // At the tail (5); up to the compaction (4): its summary is the
        // detail, shown only when it is open.
        assert!(inspect.key(Key::Up));
        assert!(inspect.key(Key::Enter));
        let all = screen(&inspect, 12).join("\n");
        assert!(all.contains("the summary of the long session"), "{all}");
        assert!(inspect.key(Key::Left));
        let all = screen(&inspect, 12).join("\n");
        assert!(!all.contains("the summary"), "{all}");
        // Up to the call (1): its output, in full, when it is open. The
        // step's record is tall now (the thinking, the text, the tool),
        // so a tall screen keeps it whole.
        assert!(inspect.key(Key::Up));
        assert!(inspect.key(Key::Up));
        assert!(inspect.key(Key::Up));
        assert!(inspect.key(Key::Enter));
        let all = screen(&inspect, 24).join("\n");
        assert!(all.contains("TOOL bash  OK"), "{all}");
        assert!(all.contains("a\nb\nc\nd\ne"), "{all}");
    }

    #[test]
    fn the_keys_jump_between_the_users() {
        let mut inspect = inspect();
        // At the tail; `u` to the second message, `u` again to the first,
        // `U` back.
        assert!(inspect.key(Key::Char('u')));
        assert!(inspect.key(Key::Char('u')));
        let lines = screen(&inspect, 10);
        assert!(lines.first().unwrap().contains("First"));
        assert!(lines.last().unwrap().contains("1/6"));
        assert!(inspect.key(Key::Char('U')));
        let lines = screen(&inspect, 10);
        assert!(lines.join("\n").contains("Second"));
        assert!(lines.last().unwrap().contains("4/6"));
    }

    #[test]
    fn the_keys_go_to_the_top_and_the_tail() {
        let mut inspect = inspect();
        assert!(inspect.key(Key::Char('g')));
        let lines = screen(&inspect, 10);
        assert!(lines.first().unwrap().contains("First"));
        assert!(inspect.key(Key::Char('G')));
        let lines = screen(&inspect, 10);
        let all = lines.join("\n");
        assert!(all.contains("Ok."), "{all}");
        assert!(lines.last().unwrap().contains("follow"));
    }

    #[test]
    fn the_expanded_thinking_shows_it_all() {
        let mut inspect = inspect();
        assert!(inspect.key(Key::Char('g')));
        assert!(inspect.key(Key::Down));
        assert!(inspect.key(Key::Enter));
        // D24: the label's line and the thinking under the gutter.
        let all = screen(&inspect, 10).join("\n");
        assert!(all.contains("THINKING"), "{all}");
        assert!(all.contains("  │ let me look"), "{all}");
        assert!(all.contains("  │ and more"), "{all}");
    }

    #[test]
    fn the_focused_thinking_shows_it_all_in_the_texts_color() {
        // AGL-085, D3: the record the navigation points at shows its
        // thinking whole, and its rows are the color of a message's text.
        let events = vec![
            Event::User("Go".into()),
            Event::Thinking(
                (1..=8)
                    .map(|n| format!("line {n}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Event::Call {
                text: None,
                call: call("c1", "ls"),
            },
            Event::Result {
                id: "c1".into(),
                outcome: Outcome::Ok,
                content: "a".into(),
            },
        ];
        let red = crate::style::Style::parse("#FF0000", false).expect("a color");
        let theme = Theme::new(true, &[(Role::InputText, red)]);
        let inspect = Inspect::new(&events, &[], Lang::En);
        // The tail is the step's record, so its thinking is what shows.
        let all = inspect
            .render(&theme, Lang::En, 40, 24, std::path::Path::new("/tmp"), &[])
            .join("\n");
        let row = all
            .lines()
            .find(|line| line.contains("line 8"))
            .unwrap_or_else(|| panic!("the sixth line is cut: {all}"));
        assert!(row.contains(&theme.start(Role::InputText)), "{row}");
    }

    #[test]
    fn the_expanded_text_shows_it_all() {
        // A text that wraps to more than three lines at the width:
        // collapsed, it is the first three lines with the ellipsis (D9).
        let events = vec![
            Event::User("Go".into()),
            Event::Call {
                text: Some(
                    "lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod \
                     tempor incididunt ut labore et dolore magna aliqua, ut enim ad minim \
                     veniam, quis nostrud exercitation ullamco laboris"
                        .into(),
                ),
                call: call("c1", "ls"),
            },
            Event::Result {
                id: "c1".into(),
                outcome: Outcome::Ok,
                content: "ok".into(),
            },
        ];
        let mut inspect = Inspect::new(&events, &[], Lang::En);
        assert!(inspect.key(Key::Char('g')));
        assert!(inspect.key(Key::Down));
        let all = screen(&inspect, 10).join("\n");
        assert!(all.contains("…"), "{all}");
        assert!(!all.contains("ullamco"), "{all}");
        assert!(inspect.key(Key::Enter));
        let lines = screen(&inspect, 10);
        assert!(lines.join("\n").contains("ullamco"), "{lines:?}");
        // The content lines are whole; the hint line may be truncated itself.
        assert!(!lines[..lines.len() - 1].join("\n").contains("…"), "{lines:?}");
    }

    #[test]
    fn the_escape_leaves() {
        let mut inspect = inspect();
        assert!(inspect.key(Key::Down));
        assert!(!inspect.key(Key::Escape));
    }

    #[test]
    fn the_full_output_come_from_the_file_when_the_result_was_cut() {
        let dir = std::env::temp_dir().join(format!("agl-inspect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("c1.txt"), "head\ncut\ntail").unwrap();
        let events = session();
        let mut inspect = Inspect::new(&events, &[], Lang::En);
        assert!(inspect.key(Key::Char('g')));
        // The step's record (the thinking, the text and the call) is now
        // one record after the user's.
        assert!(inspect.key(Key::Down));
        assert!(inspect.key(Key::Enter));
        // The step's record is tall now (the thinking and the text); a
        // tall screen keeps the output on it.
        let lines = inspect.render(&theme(), Lang::En, 40, 24, &dir, &[]);
        let all = lines.join("\n");
        assert!(all.contains("head\ncut\ntail"), "{all}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_focused_record_is_marked() {
        let mut inspect = inspect();
        assert!(inspect.key(Key::Char('g')));
        let all = screen(&inspect, 10).join("\n");
        // The first record (the user's "First") is the focus; the rest
        // is not marked.
        assert!(all.contains("▸ ›  First"), "{all}");
        assert!(!all.contains("▸ Ok."), "{all}");
        assert!(inspect.key(Key::Down));
        let all = screen(&inspect, 10).join("\n");
        assert!(all.contains("▸ THINKING"), "{all}");
        assert!(!all.contains("▸ ›  First"), "{all}");
    }

    #[test]
    fn the_dock_stays_at_the_bottom_of_the_screen() {
        let inspect = inspect();
        let dock = vec!["›  ask".into(), "agentLIBRE · /home/dinozyavier/repos/agentLIBRE".into()];
        let lines = screen_dock(&inspect, 10, &dock);
        assert_eq!(lines.len(), 10);
        // The dock's rows are the last rows of the screen, in order.
        assert!(lines[lines.len() - 2].contains("›  ask"), "{lines:?}");
        assert!(lines[lines.len() - 1].contains("agentLIBRE"), "{lines:?}");
    }

    #[test]
    fn the_expanded_write_shows_its_whole_input() {
        let events = vec![
            Event::User("Go".into()),
            Event::Call {
                text: None,
                call: ToolCall {
                    id: "c1".into(),
                    name: "forge".into(),
                    args: vec![
                        ("action".into(), Value::Text("append".into())),
                        ("name".into(), Value::Text("agl-066".into())),
                        ("section".into(), Value::Text("Progress".into())),
                        ("text".into(), Value::Text("the bullet".into())),
                    ],
                },
            },
            Event::Result {
                id: "c1".into(),
                outcome: Outcome::Ok,
                content: "ok".into(),
            },
        ];
        let mut inspect = Inspect::new(&events, &[], Lang::En);
        assert!(inspect.key(Key::Char('g')));
        assert!(inspect.key(Key::Down));
        assert!(inspect.key(Key::Enter));
        let all = screen(&inspect, 12).join("\n");
        // D28: no summary of its own — the action is on the line, the
        // input argument by argument.
        assert!(all.contains("action: append"), "{all}");
        assert!(all.contains("name: agl-066"), "{all}");
        // The whole input, argument by argument (D11); the lines wrap at
        // the width.
        assert!(all.contains("section: Prog"), "{all}");
        assert!(all.contains("text: the bullet"), "{all}");
    }

    #[test]
    fn draw_does_not_scroll_the_screen() {
        // AGL-066, D15: the clear and the home, the lines without a
        // trailing newline that would scroll the screen up, and the
        // cursor at the bottom.
        let inspect = inspect();
        let written = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = written.clone();
        let write = |text: &str| sink.lock().unwrap().push_str(text);
        draw(
            &write,
            &inspect,
            &Theme::plain(),
            Lang::En,
            40,
            10,
            &std::env::temp_dir(),
            &[],
        );
        let written = written.lock().unwrap();
        let lines = inspect.render(&Theme::plain(), Lang::En, 40, 10, &std::env::temp_dir(), &[]);
        let home = format!("{CLEAR}{HOME}", CLEAR = term::CLEAR, HOME = term::HOME);
        let bottom = term::cursor_at(10, 1);
        assert!(written.starts_with(&home), "{written:?}");
        assert!(written.ends_with(&bottom), "{written:?}");
        let body = written[home.len()..written.len() - bottom.len()].to_owned();
        // The window, whole, as rendered: no line lost at the top, none
        // added at the bottom.
        assert_eq!(body, lines.join("\n"), "{body:?}");
        assert!(!body.ends_with('\n'), "{body:?}");
    }

    #[test]
    fn the_focus_line_sits_on_its_background() {
        // AGL-066, D16: the first line of the focused record, on the
        // focus's background, as wide as the screen; the row's own
        // resets re-assert the background.
        let theme = Theme::new(true, &[]);
        let mut inspect = inspect();
        assert!(inspect.key(Key::Char('g')));
        let lines = inspect.render(&theme, Lang::En, 40, 10, &std::env::temp_dir(), &[]);
        let focus = lines
            .iter()
            .find(|line| line.contains('▸'))
            .expect("the focus's line");
        let start = theme.start(Role::FocusBackground);
        assert!(focus.starts_with(&start), "{focus:?}");
        assert!(focus.ends_with("\x1b[0m"), "{focus:?}");
        assert!(focus.contains(&format!("\x1b[0m{start}")), "{focus:?}");
        assert_eq!(crate::repl::text::width(focus), 40, "{focus:?}");
    }
}
