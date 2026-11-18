//! The inspect mode of the session (AGL-066, D4, D5, D8; AGL-098): the
//! alternate screen on which the records of the session are drawn. The
//! focus is a block — a thinking, a tool, a result — and the block it
//! stands on prints whole. `↑`/`↓` turn the screen, and so does the wheel,
//! since Alacritty sends it as those arrows (D1); `Shift`+arrows walk the
//! blocks and `Alt`+arrows move to the neighbouring record (D2, D3); the
//! dock below stays live, and leaving puts the block he was reading above
//! it again (D6, D7).

use std::path::Path;

use super::dock::{Dock, Layout};
use super::markdown;
use super::term::{self, Key};
use super::text::{clean, hard_wrap, truncate};
use super::transcript::{Fold, Render, Screen, block_lines, notice_lines, summary, user_lines};
use crate::backend::ToolCall;
use crate::i18n::{Lang, m, repl as texts};
use crate::records::{self, BlockKind, Record};
use crate::session::{Event, TransitionLine};
use crate::style::Role;
use crate::theme::Theme;
use crate::tools::ToolResult;
use crate::tr;

/// The rows the screen keeps for the blank one and the hint (D8).
const RESERVE: usize = 2;
/// The rows one tick of the wheel turns (AGL-098, D1).
const WHEEL_STEP: usize = 3;

/// What leaving the mode gives back (AGL-098, D6, D7): the block he was
/// reading, to redraw above the dock, and the input's text when `Enter`
/// sent it.
pub struct Exit {
    pub block: Vec<String>,
    pub submit: Option<String>,
}

/// The mode's answer to a key (AGL-098, D1).
pub enum Mode {
    /// The mode took it.
    Stay,
    /// The mode is over.
    Exit,
    /// The key belongs to the dock: its text, and `Enter` sending it (D7).
    Dock,
}

/// The records of a session and the navigation over their blocks
/// (D4, D5, D8; AGL-098).
pub struct Inspect {
    records: Vec<Record>,
    /// The record the focus stands on.
    selection: usize,
    /// The block of the record the focus stands on (AGL-098, D2).
    block: usize,
    /// The tail of the session is kept on screen (D8: `G`).
    follow: bool,
    /// The first row of the window (AGL-098, D1).
    top: usize,
    /// A page: the rows of the body at the last redraw.
    body: usize,
    /// The focus moved: the next redraw brings its block onto the screen.
    reveal: bool,
}

impl Inspect {
    pub fn new(events: &[Event], transitions: &[TransitionLine], lang: Lang) -> Inspect {
        let records = records::group(events, transitions, lang);
        let selection = records.len().saturating_sub(1);
        let block = records
            .get(selection)
            .map_or(0, |record| record.blocks().len() - 1);
        Inspect {
            records,
            selection,
            block,
            follow: true,
            top: 0,
            body: 0,
            reveal: false,
        }
    }

    /// Takes a key (D8, AGL-098, D1).
    pub fn key(&mut self, key: Key) -> Mode {
        match key {
            Key::Escape => Mode::Exit,
            // D1, the human's word of the 18th: the mode answers to the
            // arrows as the REPL does — the plain ones are the input's
            // (its history), `Ctrl`+arrows turn the page, and the wheel,
            // known apart from them by the mouse reporting, scrolls.
            Key::Up | Key::Down => Mode::Dock,
            Key::CtrlUp => {
                self.follow = false;
                self.top = self.top.saturating_sub(self.body);
                Mode::Stay
            }
            Key::CtrlDown => {
                self.follow = false;
                self.top += self.body;
                Mode::Stay
            }
            Key::WheelUp => {
                self.follow = false;
                self.top = self.top.saturating_sub(WHEEL_STEP);
                Mode::Stay
            }
            Key::WheelDown => {
                self.follow = false;
                self.top += WHEEL_STEP;
                Mode::Stay
            }
            Key::ShiftUp => {
                self.take_block(false);
                Mode::Stay
            }
            Key::ShiftDown => {
                self.take_block(true);
                Mode::Stay
            }
            Key::AltUp => {
                self.neighbour(false);
                Mode::Stay
            }
            Key::AltDown => {
                self.neighbour(true);
                Mode::Stay
            }
            Key::Alt('u') => {
                self.jump_to_user(false);
                Mode::Stay
            }
            Key::Alt('U') => {
                self.jump_to_user(true);
                Mode::Stay
            }
            Key::Alt('g') => {
                self.selection = 0;
                self.block = 0;
                self.follow = false;
                self.reveal = true;
                Mode::Stay
            }
            Key::Alt('G') => {
                self.go_tail();
                Mode::Stay
            }
            _ => Mode::Dock,
        }
    }

    /// The tail: the last record, its last block (D8).
    fn go_tail(&mut self) {
        self.selection = self.records.len().saturating_sub(1);
        self.block = self
            .records
            .get(self.selection)
            .map_or(0, |record| record.blocks().len() - 1);
        self.follow = true;
    }

    /// `Shift`+`↑`/`↓` (D2): the blocks in the order they stand, round
    /// about.
    fn take_block(&mut self, next: bool) {
        let total: usize = self.total_blocks();
        if total == 0 {
            return;
        }
        let flat = self.flat();
        let flat = if next {
            (flat + 1) % total
        } else {
            (flat + total - 1) % total
        };
        self.set_flat(flat);
        self.follow = false;
        self.reveal = true;
    }

    /// `Alt`+`↑`/`↓` (D3): the neighbouring record, keeping the place in
    /// its block list — and the last block of a record keeps the last
    /// place, so a step's `result` steps to an answer and the answer
    /// steps to a step's result.
    fn neighbour(&mut self, next: bool) {
        let index = if next {
            self.selection + 1
        } else {
            match self.selection.checked_sub(1) {
                Some(index) => index,
                None => return,
            }
        };
        if index >= self.records.len() {
            return;
        }
        let here = self.records[self.selection].blocks().len();
        let there = self.records[index].blocks().len();
        self.block = if self.block + 1 >= here {
            there - 1
        } else {
            self.block.min(there - 1)
        };
        self.selection = index;
        self.follow = false;
        self.reveal = true;
    }

    /// `Alt`+`u`/`U`: the previous and the next message of the human (D5);
    /// the focus stays when there is none.
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
            self.block = 0;
            self.follow = false;
            self.reveal = true;
        }
    }

    /// The place of the focused block in the flat list of all of them.
    fn flat(&self) -> usize {
        self.records[..self.selection]
            .iter()
            .map(|record| record.blocks().len())
            .sum::<usize>()
            + self.block
    }

    fn set_flat(&mut self, mut flat: usize) {
        for (index, record) in self.records.iter().enumerate() {
            let count = record.blocks().len();
            if flat < count {
                self.selection = index;
                self.block = flat;
                return;
            }
            flat -= count;
        }
    }

    fn total_blocks(&self) -> usize {
        self.records.iter().map(|record| record.blocks().len()).sum()
    }

    /// The whole screen: the window of the records, the blank row and the
    /// hint (D4, D8), the dock's rows at the bottom (D14), exactly
    /// `rows` rows.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
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
        self.body = body;
        let (content, starts) = self.content(theme, lang, columns, outputs);
        let len = content.len();
        let max_top = len.saturating_sub(body.min(len));
        if self.follow {
            self.top = max_top;
        } else {
            let mut top = self.top.min(max_top);
            if self.reveal {
                let flat = self.flat();
                if let Some(start) = starts.get(flat) {
                    let start = *start;
                    let end = starts.get(flat + 1).copied().unwrap_or(len);
                    if start < top {
                        top = start;
                    } else if end > top + body {
                        top = end.saturating_sub(body);
                    }
                }
            }
            self.top = top;
        }
        self.reveal = false;
        let mut lines: Vec<String> = content[self.top..(self.top + body).min(len)].to_vec();
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

    /// The lines of the blocks and, per block, the line it starts at.
    fn content(
        &self,
        theme: &Theme,
        lang: Lang,
        columns: usize,
        outputs: &Path,
    ) -> (Vec<String>, Vec<usize>) {
        let mut lines = Vec::new();
        let mut starts: Vec<usize> = Vec::new();
        for (index, record) in self.records.iter().enumerate() {
            if index > 0 {
                lines.push(String::new());
            }
            for block in 0..record.blocks().len() {
                starts.push(lines.len());
                let focused = index == self.selection && block == self.block;
                let mut rows =
                    self.record_block(record, block, focused, theme, lang, columns, outputs);
                // The focus's marker at the head of the block it stands
                // on (D13), on the focus's background (D16).
                if focused
                    && let Some(first) = rows.first_mut()
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
                    *first = theme.on_style(Role::FocusBackground, &format!("{marked}{pad}"));
                }
                lines.extend(rows);
            }
        }
        (lines, starts)
    }

    /// One block of a record (AGL-098, D2): whole when the focus stands
    /// on it, compact otherwise; a block with no text shows the
    /// placeholder (AGL-095, D4).
    #[allow(clippy::too_many_arguments)]
    fn record_block(
        &self,
        record: &Record,
        block: usize,
        focused: bool,
        theme: &Theme,
        lang: Lang,
        columns: usize,
        outputs: &Path,
    ) -> Vec<String> {
        match record {
            Record::User(message) => user_lines(theme, message, columns, Role::InputText),
            Record::State(state) => vec![theme.paint(Role::Muted, &format!("[{state}]"))],
            Record::Notice { line, role, detail } => {
                // A command keeps the message's look in the mode too,
                // as in the transcript (AGL-101).
                let mut lines = if line.starts_with('/') {
                    user_lines(theme, line, columns, Role::Tool)
                } else {
                    notice_lines(theme, line, columns, *role)
                };
                if focused
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
            Record::Tool {
                call,
                result,
                hint,
                phases,
                time,
                thinking,
                text,
            } => match record.blocks().get(block).copied() {
                Some(BlockKind::Tool) => tool_block(
                    theme,
                    lang,
                    columns,
                    call,
                    result.as_ref(),
                    hint,
                    phases,
                    *time,
                    text.as_deref(),
                    focused,
                ),
                Some(BlockKind::Result) => {
                    result_block(theme, lang, columns, call, result.as_ref(), outputs, focused)
                }
                _ => thinking_block(theme, lang, columns, thinking.as_deref(), focused),
            },
            Record::Answer {
                text,
                time,
                thinking,
            } => {
                if record.blocks().get(block).copied() == Some(BlockKind::Answer) {
                    answer_block(theme, lang, columns, text, time.as_ref().copied(), focused)
                } else {
                    thinking_block(theme, lang, columns, thinking.as_deref(), focused)
                }
            }
        }
    }

    /// The block the focus stands on, whole and without its marker
    /// (AGL-098, D6): what the screen draws again above the dock when the
    /// mode is over.
    pub fn focused_block(&self, theme: &Theme, lang: Lang, columns: usize, outputs: &Path) -> Vec<String> {
        let Some(record) = self.records.get(self.selection) else {
            return Vec::new();
        };
        self.record_block(record, self.block, true, theme, lang, columns, outputs)
    }

    /// The position, the follow and the keys' hint (D8); the position is
    /// first, so a narrow screen keeps it.
    fn hint(&self, theme: &Theme, lang: Lang, columns: usize) -> String {
        let rest = if self.follow {
            format!("{} · {}", tr!(lang, m::INSPECT_FOLLOW), tr!(lang, m::INSPECT_HINT))
        } else {
            tr!(lang, m::INSPECT_HINT).into()
        };
        let hint = format!("{}/{} · {}", self.flat() + 1, self.total_blocks(), rest);
        theme.paint(Role::Muted, &truncate(&hint, columns))
    }
}

/// Whether the record is a message of the human (D5).
fn is_user(record: &Record) -> bool {
    matches!(record, Record::User(_))
}

/// The thinking's block (D19; AGL-098, D2): whole on the focus, folded off
/// it; the placeholder when the step has no thinking (AGL-095, D4). D23,
/// AGL-085 D3, AGL-093 D2: the rows carry their own style, and the block
/// the focus stands on takes the color of a message's text.
fn thinking_block(
    theme: &Theme,
    lang: Lang,
    columns: usize,
    thinking: Option<&str>,
    focused: bool,
) -> Vec<String> {
    block_lines(
        theme,
        lang,
        columns,
        tr!(lang, m::THINKING),
        &clean(thinking.unwrap_or_default()),
        if focused { Role::InputText } else { Role::Thinking },
        if focused { Fold::Whole } else { Fold::Folded },
        Render::Markdown,
    )
}

/// The tool's block (D19; AGL-098, D2): the text the model wrote before
/// the call — its head off the focus — and the card, whose input row is
/// the hint off the focus and the whole of what was sent on it (D4).
#[allow(clippy::too_many_arguments)]
fn tool_block(
    theme: &Theme,
    lang: Lang,
    columns: usize,
    call: &ToolCall,
    result: Option<&ToolResult>,
    hint: &str,
    phases: &[records::PhaseSpan],
    time: Option<std::time::Duration>,
    text: Option<&str>,
    focused: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(text) = text {
        let mut block = markdown::render(text.trim(), columns, theme);
        if !focused && block.len() > 3 {
            block.truncate(3);
            block
                .last_mut()
                .expect("three lines")
                .push_str(&theme.paint(Role::Muted, "…"));
        }
        lines.extend(block);
    }
    let mut items = records::phase_items(phases);
    if let Some(elapsed) = time {
        items.push(format!(
            "{} {}",
            call.name,
            texts::duration(elapsed.as_secs())
        ));
    }
    let input = if focused {
        records::call_input(call)
    } else {
        hint.to_owned()
    };
    // AGL-098: as the thinking's on the focus, the whole input on it
    // stands in the color of a message's text.
    lines.extend(card_lines(
        theme,
        lang,
        columns,
        call,
        result,
        &input,
        &items,
        focused,
    ));
    lines
}

/// The result's block (AGL-095, D2; AGL-098, D2): whole on the focus —
/// from the output's file when the call's output was cut (AGL-049) — and
/// folded as in the transcript off it; the placeholder when the result is
/// not known.
#[allow(clippy::too_many_arguments)]
fn result_block(
    theme: &Theme,
    lang: Lang,
    columns: usize,
    call: &ToolCall,
    result: Option<&ToolResult>,
    outputs: &Path,
    focused: bool,
) -> Vec<String> {
    let content = match result {
        None => String::new(),
        Some(result) if !focused => clean(&result.content),
        Some(result) => {
            let file = outputs.join(format!("{}.txt", call.id));
            std::fs::read_to_string(&file).unwrap_or_else(|_| result.content.clone())
        }
    };
    block_lines(
        theme,
        lang,
        columns,
        tr!(lang, m::RESULT_LABEL),
        &content,
        // AGL-098: as the thinking's, the block under the focus stands in
        // the color of a message's text.
        if focused {
            Role::InputText
        } else {
            Role::Muted
        },
        if focused { Fold::Whole } else { Fold::Folded },
        Render::Plain,
    )
}

/// The answer's block (AGL-098, D2; AGL-100): a head for the focus to
/// stand on, as the other blocks have, and its text, in full (D1), under
/// the gutter; on the focus the time the turn took joins it (AGL-080, D2).
fn answer_block(
    theme: &Theme,
    lang: Lang,
    columns: usize,
    text: &str,
    time: Option<std::time::Duration>,
    focused: bool,
) -> Vec<String> {
    let mut lines = vec![theme.paint(Role::Tool, tr!(lang, m::ANSWER_LABEL))];
    let gutter = theme.paint(Role::InputRule, "  │ ");
    let inner = columns.saturating_sub(4).max(1);
    for row in markdown::render(text.trim(), inner, theme) {
        lines.push(format!("{gutter}{row}"));
    }
    if focused {
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
            lines.push(summary(theme, None, &items, Role::Tool));
        }
    }
    lines
}

/// The compact card of a call, as the inspect mode draws it (D6; AGL-080,
/// D1): the tool's line with its status, the lines of the input, the row of
/// its phases.
#[allow(clippy::too_many_arguments)]
fn card_lines(
    theme: &Theme,
    lang: Lang,
    columns: usize,
    call: &ToolCall,
    result: Option<&ToolResult>,
    input: &str,
    items: &[String],
    focused: bool,
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
    // D26: the tool's row, like the thinking's, in the muted color — on
    // the focus, as the thinking's there, in the color of a message's text.
    let input_role = if focused {
        Role::InputText
    } else {
        Role::Muted
    };
    for line in clean(input).lines() {
        for row in hard_wrap(line, inner) {
            lines.push(format!("{gutter}{}", theme.paint(input_role, &row)));
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

/// The inspect mode on the terminal (D4, D8): the alternate screen, the
/// full redraws, the keys until the escape. `write` is what the mode
/// draws with; the dock's rows come from `layout()` anew at each redraw,
/// and every key the mode does not take goes to the dock (AGL-098, D7).
#[allow(clippy::too_many_arguments)]
pub fn run(
    write: &dyn Fn(&str),
    theme: &Theme,
    lang: Lang,
    events: Vec<Event>,
    transitions: Vec<TransitionLine>,
    outputs: std::path::PathBuf,
    dock: &Dock,
) -> Exit {
    let mut inspect = Inspect::new(&events, &transitions, lang);
    let mut parser = term::Parser::default();
    let mut buffer = [0u8; 4096];
    let mut size = term::size();
    write(term::ALT_ON);
    write(term::MOUSE_ON);
    let mut submit: Option<String> = None;
    loop {
        if term::take_resized() {
            size = term::size();
        }
        // The dock's rows of the moment (D14): the input prompt is live,
        // so its rows change with its keys (AGL-098, D7).
        let rows = dock.layout();
        draw(write, &mut inspect, theme, lang, size.0, size.1, &outputs, &rows);
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
            match inspect.key(key.clone()) {
                Mode::Exit => {
                    open = false;
                    break;
                }
                Mode::Dock => {
                    if let super::editor::Outcome::Submit(message) = dock.key(key) {
                        // `Enter` in the input sends and leaves (D7).
                        submit = Some(message);
                        open = false;
                        break;
                    }
                }
                Mode::Stay => {}
            }
        }
        if !open {
            break;
        }
    }
    write(term::MOUSE_OFF);
    write(term::ALT_OFF);
    // Leaving puts the block he was reading above the dock again (D6);
    // a send needs no redraw — its own line says it.
    let block = if submit.is_some() {
        Vec::new()
    } else {
        inspect.focused_block(theme, lang, dock.columns(), &outputs)
    };
    Exit { block, submit }
}

/// One redraw of the screen: the clear, the lines, no trailing newline.
/// A last newline would scroll the screen up by a row and cut the
/// window's first row (AGL-066, D15). The cursor rests on the input's
/// caret and shows: the mode leaves the input live, so the human reads
/// with the prompt under his hands (AGL-098, D7).
#[allow(clippy::too_many_arguments)]
fn draw(
    write: &dyn Fn(&str),
    inspect: &mut Inspect,
    theme: &Theme,
    lang: Lang,
    columns: usize,
    rows: usize,
    outputs: &Path,
    dock: &Layout,
) {
    let lines = inspect.render(theme, lang, columns, rows, outputs, &dock.rows);
    let mut text = String::from(term::CLEAR);
    text.push_str(term::HOME);
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        text.push_str(line);
    }
    // The dock's rows stand at the screen's bottom, as many as fit below
    // the hint; the caret sits on its own row among them.
    let placed = dock.rows.len().min(rows.saturating_sub(RESERVE));
    let row = if dock.cursor_row < placed {
        (rows + dock.cursor_row + 1).saturating_sub(placed).max(1)
    } else {
        rows.max(1)
    };
    text.push_str(&term::cursor_at(row, 1 + dock.cursor_column));
    text.push_str("\x1b[?25h");
    write(&text);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Outcome, Value};

    fn call(id: &str, command: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "bash".into(),
            args: vec![("command".into(), Value::Text(command.into()))],
        }
    }

    /// The records: 0 the user, 1 the step (the thinking, the text and
    /// the call), 2 the answer, 3 the user, 4 the compaction, 5 the
    /// answer — 10 blocks between them.
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

    /// A key that leaves the mode on.
    fn press(inspect: &mut Inspect, key: Key) {
        assert!(matches!(inspect.key(key), Mode::Stay));
    }

    /// The screen of `rows` rows, so that the records do not all fit.
    fn screen(inspect: &mut Inspect, rows: usize) -> Vec<String> {
        inspect.render(&theme(), Lang::En, 40, rows, &std::env::temp_dir(), &[])
    }

    #[test]
    fn the_tail_is_on_screen_at_the_start() {
        let mut inspect = inspect();
        let lines = screen(&mut inspect, 10);
        assert_eq!(lines.len(), 10);
        let all = lines.join("\n");
        // The last answer and the compaction's line; the hint has the
        // block's position and the follow.
        assert!(all.contains("Ok."), "{all}");
        assert!(all.contains("Compacting"), "{all}");
        assert!(lines.last().unwrap().contains("10/10"));
        assert!(lines.last().unwrap().contains("follow"));
    }

    #[test]
    fn the_plain_arrows_are_the_inputs_and_ctrl_pages() {
        // AGL-098, D1: the mode answers to the arrows as the REPL does —
        // the plain ones go to the input; `Ctrl`+arrows turn the screen.
        let mut inspect = inspect();
        assert!(matches!(inspect.key(Key::Up), Mode::Dock));
        assert!(matches!(inspect.key(Key::Down), Mode::Dock));
        // A redraw first: it is what gives the mode its page size.
        screen(&mut inspect, 10);
        press(&mut inspect, Key::CtrlUp);
        let all = screen(&mut inspect, 10).join("\n");
        assert!(!all.contains("Ok."), "{all}");
        assert!(all.contains("Done."), "{all}");
        // The position stayed on the last block; only the follow went.
        let hint = screen(&mut inspect, 10).pop().unwrap();
        assert!(hint.contains("10/10"), "{hint}");
        assert!(!hint.contains("follow"), "{hint}");
        press(&mut inspect, Key::CtrlDown);
        let all = screen(&mut inspect, 10).join("\n");
        assert!(all.contains("Ok."), "{all}");
    }

    #[test]
    fn the_wheel_scrolls_by_rows() {
        // AGL-098, D1: the wheel, known apart from the arrows by the
        // mouse reporting, turns the screen by rows, not by a page.
        let mut inspect = inspect();
        let before = screen(&mut inspect, 10);
        press(&mut inspect, Key::WheelUp);
        let after = screen(&mut inspect, 10);
        assert_ne!(after, before);
        // The page shows three fewer rows than Ctrl+arrow cuts.
        let mut by_page = Inspect::new(&session(), &[], Lang::En);
        press(&mut by_page, Key::CtrlUp);
        assert_ne!(screen(&mut by_page, 10), after);
        // And back to the tail: the rows, and `G` brings the follow too.
        press(&mut inspect, Key::WheelDown);
        press(&mut inspect, Key::WheelDown);
        press(&mut inspect, Key::WheelDown);
        assert!(!screen(&mut inspect, 10).join("\n").contains("follow"));
        press(&mut inspect, Key::Alt('G'));
        assert_eq!(screen(&mut inspect, 10), before);
    }

    #[test]
    fn shift_arrows_walk_the_blocks_round_about() {
        // AGL-098, D2: the focus moves block by block, and wraps at the
        // ends.
        let mut inspect = inspect();
        press(&mut inspect, Key::ShiftDown);
        let hint = screen(&mut inspect, 10).pop().unwrap();
        assert!(hint.contains("1/10"), "{hint}");
        press(&mut inspect, Key::ShiftUp);
        let hint = screen(&mut inspect, 10).pop().unwrap();
        assert!(hint.contains("10/10"), "{hint}");
        press(&mut inspect, Key::ShiftUp);
        let hint = screen(&mut inspect, 10).pop().unwrap();
        assert!(hint.contains("9/10"), "{hint}");
    }

    #[test]
    fn alt_arrows_keep_the_place_in_the_neighbour() {
        // AGL-098, D3: from the answer back over the compaction and the
        // user's message to the step's result — the last block always
        // keeps the last place.
        let mut inspect = inspect();
        press(&mut inspect, Key::AltUp);
        let hint = screen(&mut inspect, 10).pop().unwrap();
        assert!(hint.contains("8/10"), "{hint}");
        press(&mut inspect, Key::AltUp);
        let hint = screen(&mut inspect, 10).pop().unwrap();
        assert!(hint.contains("7/10"), "{hint}");
        press(&mut inspect, Key::AltUp);
        let hint = screen(&mut inspect, 10).pop().unwrap();
        assert!(hint.contains("6/10"), "{hint}");
        press(&mut inspect, Key::AltUp);
        let hint = screen(&mut inspect, 10).pop().unwrap();
        assert!(hint.contains("4/10"), "{hint}");
        // Forward: the step's result steps to the answer.
        press(&mut inspect, Key::AltDown);
        let hint = screen(&mut inspect, 10).pop().unwrap();
        assert!(hint.contains("6/10"), "{hint}");
    }

    #[test]
    fn alt_from_an_answer_takes_the_step_result() {
        // AGL-098, D3: the answer, having no tool and no result of its
        // own, steps to the last block of the neighbouring step — its
        // result, printed whole.
        let events = vec![
            Event::User("Go".into()),
            Event::Answer("First.".into()),
            Event::Call {
                text: None,
                call: call("c1", "ls"),
            },
            Event::Result {
                id: "c1".into(),
                outcome: Outcome::Ok,
                content: "out".into(),
            },
            Event::Answer("Second.".into()),
        ];
        let mut inspect = Inspect::new(&events, &[], Lang::En);
        press(&mut inspect, Key::AltUp);
        let lines = screen(&mut inspect, 24);
        let all = lines.join("\n");
        assert!(
            lines.iter().any(|line| line.contains("6/8")),
            "the result block: {all}"
        );
        assert!(all.contains("out"), "{all}");
        // And the way back: from the result to the answer.
        press(&mut inspect, Key::AltUp);
        let all = screen(&mut inspect, 24).join("\n");
        assert!(all.contains("First."), "{all}");
    }

    #[test]
    fn a_tool_block_steps_to_the_answer_text() {
        // AGL-098, D3: a step's `tool` — block 1 of its 3 — keeps place
        // 1 of the answer, which is the answer's text.
        let mut inspect = inspect();
        press(&mut inspect, Key::ShiftDown); // flat 0: the first message
        press(&mut inspect, Key::ShiftDown); // flat 1: the thinking
        press(&mut inspect, Key::ShiftDown); // flat 2: the tool
        let hint = screen(&mut inspect, 10).pop().unwrap();
        assert!(hint.contains("3/10"), "{hint}");
        press(&mut inspect, Key::AltDown);
        let hint = screen(&mut inspect, 10).pop().unwrap();
        assert!(hint.contains("6/10"), "{hint}");
    }

    #[test]
    fn the_focused_block_prints_it_whole_from_the_file() {
        // AGL-098, D4; AGL-049: the whole output of the focused block,
        // from the output's file when the result was cut.
        let dir = std::env::temp_dir().join(format!("agl-inspect-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("c1.txt"), "head\ncut\ntail").unwrap();
        let events = vec![
            Event::User("Go".into()),
            Event::Call {
                text: None,
                call: call("c1", "ls"),
            },
            Event::Result {
                id: "c1".into(),
                outcome: Outcome::Ok,
                content: "head".into(),
            },
            Event::Answer("Done.".into()),
        ];
        let mut inspect = Inspect::new(&events, &[], Lang::En);
        // The focus stands on the answer; the result is folded to its own
        // content.
        let all = screen(&mut inspect, 24).join("\n");
        assert!(all.contains("RESULT"), "{all}");
        assert!(!all.contains("cut"), "{all}");
        // The answer steps back to the step's result: its whole file.
        press(&mut inspect, Key::AltUp);
        let all = inspect
            .render(&theme(), Lang::En, 40, 24, &dir, &[])
            .join("\n");
        assert!(all.contains("head"), "{all}");
        assert!(all.contains("cut"), "{all}");
        assert!(all.contains("tail"), "{all}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_answers_head_takes_the_focus_marker() {
        // AGL-100: the answer's block has its own head, so the focus's
        // marker stands on `ANSWER`, not on the answer's first line.
        let all = screen(&mut inspect(), 10).join("\n");
        assert!(all.contains("▸ ANSWER"), "{all}");
        assert!(all.contains("  │ Ok."), "{all}");
    }

    #[test]
    fn a_bare_step_shows_the_placeholders() {
        // AGL-095, D4: a step stays the same triple — a thinking and a
        // result with no text show their placeholder.
        let events = vec![
            Event::User("Go".into()),
            Event::Call {
                text: None,
                call: call("c1", "ls"),
            },
        ];
        let mut inspect = Inspect::new(&events, &[], Lang::En);
        let all = screen(&mut inspect, 24).join("\n");
        assert!(all.contains("RESULT"), "{all}");
        assert!(all.contains("(empty)"), "{all}");
        press(&mut inspect, Key::ShiftUp);
        press(&mut inspect, Key::ShiftUp);
        let all = screen(&mut inspect, 24).join("\n");
        assert!(all.contains("THINKING"), "{all}");
        assert!(all.contains("(empty)"), "{all}");
    }

    #[test]
    fn the_text_before_a_call_shows_whole_on_its_block() {
        // AGL-098, D4: off the focus three lines and the ellipsis, on it
        // the whole.
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
        press(&mut inspect, Key::Alt('g'));
        let all = screen(&mut inspect, 24).join("\n");
        assert!(all.contains("…"), "{all}");
        assert!(!all.contains("ullamco"), "{all}");
        // The tool's block: thinking, then text, then the call.
        press(&mut inspect, Key::ShiftDown);
        press(&mut inspect, Key::ShiftDown);
        let all = screen(&mut inspect, 24).join("\n");
        assert!(all.contains("ullamco"), "{all}");
    }

    #[test]
    fn the_focused_thinking_shows_it_all_in_the_texts_color() {
        // AGL-085, D3: the block the focus stands on shows its thinking
        // whole, and its rows are the color of a message's text.
        let events = vec![
            Event::User("Go".into()),
            Event::Thinking(
                ["one", "two", "three", "four", "five", "six", "seven", "eight"]
                    .join("\n\n"),
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
        let mut inspect = Inspect::new(&events, &[], Lang::En);
        // The focus is on the result; two blocks up stands the thinking.
        press(&mut inspect, Key::ShiftUp);
        press(&mut inspect, Key::ShiftUp);
        let all = inspect
            .render(&theme, Lang::En, 40, 24, std::path::Path::new("/tmp"), &[])
            .join("\n");
        let row = all
            .lines()
            .find(|line| line.contains("eight"))
            .unwrap_or_else(|| panic!("the last row is cut: {all}"));
        assert!(row.contains(&theme.start(Role::InputText)), "{row}");
    }

    #[test]
    fn the_focused_tool_and_result_are_in_the_texts_color() {
        // AGL-098: as the thinking's, the input of the focused tool block
        // and the rows of the focused result take the color of a
        // message's text; off the focus they stay muted.
        let events = vec![
            Event::User("Go".into()),
            Event::Call {
                text: None,
                call: call("c1", "ls"),
            },
            Event::Result {
                id: "c1".into(),
                outcome: Outcome::Ok,
                content: "out".into(),
            },
        ];
        let red = crate::style::Style::parse("#FF0000", false).expect("a color");
        let theme = Theme::new(true, &[(Role::InputText, red)]);
        let mut inspect = Inspect::new(&events, &[], Lang::En);
        // The focus is on the result.
        let all = inspect
            .render(&theme, Lang::En, 40, 24, std::path::Path::new("/tmp"), &[])
            .join("\n");
        let row = all.lines().find(|line| line.contains("out")).expect("out");
        assert!(row.contains(&theme.start(Role::InputText)), "{row}");
        // Up one: the tool's block, its whole input in the text's color.
        press(&mut inspect, Key::ShiftUp);
        let all = inspect
            .render(&theme, Lang::En, 40, 24, std::path::Path::new("/tmp"), &[])
            .join("\n");
        let row = all
            .lines()
            .find(|line| line.contains("ls"))
            .expect("the input row");
        assert!(row.contains(&theme.start(Role::InputText)), "{row}");
    }

    #[test]
    fn the_tool_block_shows_its_whole_input() {
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
        // D11: the whole input, argument by argument, on the tool's block.
        press(&mut inspect, Key::ShiftUp);
        let all = screen(&mut inspect, 24).join("\n");
        assert!(all.contains("action: append"), "{all}");
        assert!(all.contains("name: agl-066"), "{all}");
        assert!(all.contains("section: Prog"), "{all}");
        assert!(all.contains("text: the bullet"), "{all}");
    }

    #[test]
    fn the_keys_jump_between_the_users() {
        // AGL-098, D5: `Alt`+`u`/`U`.
        let mut inspect = inspect();
        press(&mut inspect, Key::Alt('u'));
        let lines = screen(&mut inspect, 10);
        assert!(lines.join("\n").contains("Second"));
        assert!(lines.last().unwrap().contains("7/10"));
        press(&mut inspect, Key::Alt('u'));
        let lines = screen(&mut inspect, 10);
        assert!(lines.first().unwrap().contains("First"));
        assert!(lines.last().unwrap().contains("1/10"));
        press(&mut inspect, Key::Alt('U'));
        let lines = screen(&mut inspect, 10);
        assert!(lines.join("\n").contains("Second"));
        assert!(lines.last().unwrap().contains("7/10"));
    }

    #[test]
    fn the_keys_go_to_the_top_and_the_tail() {
        let mut inspect = inspect();
        press(&mut inspect, Key::Alt('g'));
        let lines = screen(&mut inspect, 10);
        assert!(lines.first().unwrap().contains("First"));
        press(&mut inspect, Key::Alt('G'));
        let lines = screen(&mut inspect, 10);
        let all = lines.join("\n");
        assert!(all.contains("Ok."), "{all}");
        assert!(lines.last().unwrap().contains("follow"));
    }

    #[test]
    fn the_plain_letter_belongs_to_the_dock() {
        // AGL-098, D7: what the mode does not take goes to the input.
        let mut inspect = inspect();
        assert!(matches!(inspect.key(Key::Char('a')), Mode::Dock));
        assert!(matches!(inspect.key(Key::Enter), Mode::Dock));
    }

    #[test]
    fn the_escape_leaves() {
        let mut inspect = inspect();
        assert!(matches!(inspect.key(Key::Down), Mode::Dock));
        assert!(matches!(inspect.key(Key::Escape), Mode::Exit));
    }

    #[test]
    fn the_focused_block_is_marked() {
        let mut inspect = inspect();
        press(&mut inspect, Key::Alt('g'));
        let all = screen(&mut inspect, 10).join("\n");
        // The first record's only block is the focus; the rest is not
        // marked.
        assert!(all.contains("▸ › First"), "{all}");
        assert!(!all.contains("▸ Ok."), "{all}");
        press(&mut inspect, Key::ShiftDown);
        let all = screen(&mut inspect, 10).join("\n");
        assert!(all.contains("▸ THINKING"), "{all}");
        assert!(!all.contains("▸ › First"), "{all}");
    }

    #[test]
    fn the_dock_stays_at_the_bottom_of_the_screen() {
        let mut inspect = inspect();
        let dock = vec![
            "› ask".into(),
            "agentLIBRE · /home/dinozyavier/repos/agentLIBRE".into(),
        ];
        let lines = inspect.render(
            &theme(),
            Lang::En,
            40,
            10,
            &std::env::temp_dir(),
            &dock,
        );
        assert_eq!(lines.len(), 10);
        // The dock's rows are the last rows of the screen, in order.
        assert!(lines[lines.len() - 2].contains("› ask"), "{lines:?}");
        assert!(lines[lines.len() - 1].contains("agentLIBRE"), "{lines:?}");
    }

    #[test]
    fn draw_does_not_scroll_the_screen() {
        // AGL-066, D15: the clear and the home, the lines without a
        // trailing newline that would scroll the screen up, and the
        // cursor at the bottom.
        let mut inspect = inspect();
        let written = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = written.clone();
        let write = |text: &str| sink.lock().unwrap().push_str(text);
        draw(
            &write,
            &mut inspect,
            &Theme::plain(),
            Lang::En,
            40,
            10,
            &std::env::temp_dir(),
            &Layout {
                rows: Vec::new(),
                cursor_row: 0,
                cursor_column: 0,
            },
        );
        let written = written.lock().unwrap();
        let lines =
            Inspect::new(&session(), &[], Lang::En).render(&Theme::plain(), Lang::En, 40, 10, &std::env::temp_dir(), &[]);
        let home = format!("{CLEAR}{HOME}", CLEAR = term::CLEAR, HOME = term::HOME);
        // The cursor at the bottom of an empty dock, and shown: the input
        // stays live through the mode (AGL-098, D7).
        let bottom = format!("{}\x1b[?25h", term::cursor_at(10, 1));
        assert!(written.starts_with(&home), "{written:?}");
        assert!(written.ends_with(&bottom), "{written:?}");
        let body = written[home.len()..written.len() - bottom.len()].to_owned();
        // The window, whole, as rendered: no line lost at the top, none
        // added at the bottom.
        assert_eq!(body, lines.join("\n"), "{body:?}");
        assert!(!body.ends_with('\n'), "{body:?}");
    }

    #[test]
    fn the_cursor_rests_on_the_inputs_caret() {
        // AGL-098, D7: the input stays live — the cursor stands on its
        // caret, among the dock's rows at the bottom, and shows.
        let mut inspect = inspect();
        let written = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = written.clone();
        let write = |text: &str| sink.lock().unwrap().push_str(text);
        draw(
            &write,
            &mut inspect,
            &Theme::plain(),
            Lang::En,
            40,
            10,
            &std::env::temp_dir(),
            &Layout {
                rows: vec!["› ask".into(), "─ Ready".into(), "ctx".into()],
                cursor_row: 1,
                cursor_column: 5,
            },
        );
        let written = written.lock().unwrap();
        assert!(written.ends_with("\x1b[9;6H\x1b[?25h"), "{written:?}");
    }

    #[test]
    fn the_focus_line_sits_on_its_background() {
        // AGL-066, D16: the first line of the focused block, on the
        // focus's background, as wide as the screen; the row's own resets
        // re-assert the background.
        let theme = Theme::new(true, &[]);
        let mut inspect = inspect();
        press(&mut inspect, Key::Alt('g'));
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
