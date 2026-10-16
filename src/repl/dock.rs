//! The dock at the bottom of the terminal and the transcript above it
//! (D119, D135, D138): a status line, the input rows on their background,
//! the command menu and a row with the Function and the workspace. The
//! transcript is printed above the dock, which is drawn again after every
//! change.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Mutex, MutexGuard};

use super::editor::{Editor, Outcome};
use super::term::Key;
use super::transcript::{Activity, Screen};
use super::text::{hard_wrap, truncate, truncate_start, width};
use crate::i18n::{Lang, repl as texts};
use crate::theme::{Role, Theme};

/// The most input rows the dock shows.
const MAX_INPUT_ROWS: usize = 6;
/// The most menu rows the dock shows.
const MAX_MENU_ROWS: usize = 6;
/// How many printed lines are remembered for erasing.
const REMEMBERED_LINES: usize = 400;

/// The context usage for the status row and its role (D152): `41 213 /
/// 131 072 (32%)`, with `—` before the first step; yellow from 70%, red
/// from 90%.
fn context_usage(used: Option<usize>, size: usize) -> (String, Role) {
    let Some(used) = used else {
        return (format!("— / {}", grouped(size)), Role::Muted);
    };
    let percent = used.saturating_mul(100) / size.max(1);
    let role = match percent {
        90.. => Role::StatusFailure,
        70.. => Role::StatusPending,
        _ => Role::Muted,
    };
    (
        format!("{} / {} ({percent}%)", grouped(used), grouped(size)),
        role,
    )
}

/// `n` with a no-break space between groups of three digits.
fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push('\u{a0}');
        }
        out.push(digit);
    }
    out
}

/// What the dock looks like: its rows and where the cursor is.
#[derive(Debug, PartialEq, Eq)]
pub struct Layout {
    pub rows: Vec<String>,
    pub cursor_row: usize,
    pub cursor_column: usize,
}

struct State {
    out: Box<dyn Write + Send>,
    theme: Theme,
    lang: Lang,
    function: String,
    workspace: String,
    /// The context size and the tokens of the last prompt (D152).
    context: usize,
    context_used: Option<usize>,
    editor: Editor,
    activity: Activity,
    queued: usize,
    live: String,
    columns: usize,
    rows: usize,
    /// Rows of the dock on the screen, and the row of the cursor in it.
    drawn: Option<(usize, usize)>,
    /// Terminal rows of each printed line, newest last.
    printed: VecDeque<usize>,
}

pub struct Dock {
    state: Mutex<State>,
}

impl Dock {
    pub fn new(
        out: Box<dyn Write + Send>,
        theme: Theme,
        lang: Lang,
        function: String,
        workspace: String,
        editor: Editor,
        size: (usize, usize),
    ) -> Dock {
        Dock {
            state: Mutex::new(State {
                out,
                theme,
                lang,
                function,
                workspace,
                context: 0,
                context_used: None,
                editor,
                activity: Activity::Ready,
                queued: 0,
                live: String::new(),
                columns: size.0.max(8),
                rows: size.1.max(4),
                drawn: None,
                printed: VecDeque::new(),
            }),
        }
    }

    /// The size of the context the status row measures the prompt by
    /// (D152).
    pub fn with_context(mut self, size: usize) -> Dock {
        self.state
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .context = size;
        self
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Draws the dock for the first time.
    pub fn start(&self) {
        let mut state = self.lock();
        state.redraw("");
    }

    /// A key at the prompt; the dock follows the editor.
    pub fn key(&self, key: Key) -> Outcome {
        let mut state = self.lock();
        let outcome = state.editor.handle(key);
        if let Outcome::Submit(message) = &outcome {
            state.editor.remember(message);
        }
        state.redraw("");
        outcome
    }

    /// Empties the input (Ctrl-C at the prompt).
    pub fn clear_input(&self) {
        let mut state = self.lock();
        state.editor.clear();
        state.redraw("");
    }

    pub fn input_is_empty(&self) -> bool {
        self.lock().editor.is_empty()
    }

    pub fn set_queued(&self, queued: usize) {
        let mut state = self.lock();
        state.queued = queued;
        state.redraw("");
    }

    /// The window changed size: the dock is drawn again, and what was
    /// printed can no longer be erased.
    pub fn resize(&self, size: (usize, usize)) {
        let mut state = self.lock();
        state.columns = size.0.max(8);
        state.rows = size.1.max(4);
        state.printed.clear();
        state.redraw("");
    }

    pub fn history(&self) -> Vec<String> {
        self.lock().editor.history().to_vec()
    }

    /// Removes the dock, leaving the transcript, before leaving.
    pub fn finish(&self) {
        let mut state = self.lock();
        state.live.clear();
        state.erase_dock();
        let _ = state.out.flush();
        state.drawn = None;
    }
}

impl Screen for Dock {
    fn columns(&self) -> usize {
        self.lock().columns
    }

    fn print(&self, lines: &[String]) {
        let mut state = self.lock();
        let mut text = String::new();
        for line in lines {
            text.push_str(line);
            text.push_str("\r\n");
            let rows = width(line).div_ceil(state.columns).max(1);
            state.printed.push_back(rows);
        }
        while state.printed.len() > REMEMBERED_LINES {
            state.printed.pop_front();
        }
        state.redraw(&text);
    }

    fn context(&self, used: usize) {
        let mut state = self.lock();
        state.context_used = Some(used);
        state.redraw("");
    }

    fn live(&self, text: &str) {
        let mut state = self.lock();
        if state.live != text {
            state.live = text.into();
            state.redraw("");
        }
    }

    fn erase(&self, count: usize) -> bool {
        let mut state = self.lock();
        if count == 0 {
            return true;
        }
        if count > state.printed.len() {
            return false;
        }
        let rows: usize = state.printed.iter().rev().take(count).sum();
        let dock_rows = state.layout().rows.len();
        if rows + dock_rows >= state.rows {
            return false;
        }
        for _ in 0..count {
            state.printed.pop_back();
        }
        state.redraw_after_moving_up(rows);
        true
    }

    fn activity(&self, activity: Activity) {
        let mut state = self.lock();
        if state.activity != activity {
            state.activity = activity;
            state.redraw("");
        }
    }
}

impl State {
    /// Prints `text` above the dock, then draws the dock below it.
    fn redraw(&mut self, text: &str) {
        let mut out = String::from("\x1b[?2026h");
        self.move_to_dock_top(&mut out, 0);
        out.push_str(text);
        self.draw(&mut out);
        out.push_str("\x1b[?2026l");
        self.write(&out);
    }

    /// Like [`State::redraw`], but first goes `rows` further up, over
    /// lines that are erased.
    fn redraw_after_moving_up(&mut self, rows: usize) {
        let mut out = String::from("\x1b[?2026h");
        self.move_to_dock_top(&mut out, rows);
        self.draw(&mut out);
        out.push_str("\x1b[?2026l");
        self.write(&out);
    }

    fn erase_dock(&mut self) {
        let mut out = String::new();
        self.move_to_dock_top(&mut out, 0);
        self.write(&out);
    }

    /// Goes to the first column of the top row of the dock (plus `extra`
    /// rows up) and clears everything below.
    fn move_to_dock_top(&mut self, out: &mut String, extra: usize) {
        let up = self.drawn.map_or(0, |(_, cursor_row)| cursor_row) + extra;
        out.push('\r');
        if up > 0 {
            out.push_str(&format!("\x1b[{up}A"));
        }
        out.push_str("\x1b[J");
        self.drawn = None;
    }

    fn draw(&mut self, out: &mut String) {
        let layout = self.layout();
        out.push_str("\x1b[?25l");
        out.push_str(&layout.rows.join("\r\n"));
        let up = layout.rows.len() - 1 - layout.cursor_row;
        if up > 0 {
            out.push_str(&format!("\x1b[{up}A"));
        }
        out.push('\r');
        if layout.cursor_column > 0 {
            out.push_str(&format!("\x1b[{}C", layout.cursor_column));
        }
        out.push_str("\x1b[?25h");
        self.drawn = Some((layout.rows.len(), layout.cursor_row));
    }

    fn write(&mut self, text: &str) {
        let _ = self.out.write_all(text.as_bytes());
        let _ = self.out.flush();
    }

    /// The rows of the dock and the cursor (D135, D138).
    fn layout(&self) -> Layout {
        let theme = &self.theme;
        let columns = self.columns;
        let mut rows = Vec::new();
        if !self.live.is_empty() {
            rows.push(self.live.clone());
        }
        rows.push(self.separator());
        rows.push(background_row(theme, &[], columns));

        let avail = columns.saturating_sub(2).max(1);
        let (input, cursor_row, cursor_column) = self.input_rows(avail);
        let cursor_row = rows.len() + cursor_row;
        let empty = self.editor.is_empty();
        for (marker, row) in &input {
            let text_role = if empty {
                Role::InputHint
            } else {
                Role::InputText
            };
            rows.push(background_row(
                theme,
                &[(Role::InputPrompt, *marker), (text_role, row)],
                columns,
            ));
        }
        let menu = self.editor.menu();
        let selected = self.editor.selected();
        for (index, item) in menu.iter().take(MAX_MENU_ROWS).enumerate() {
            let name = format!("  {:<9} ", item.name);
            let picked = selected == Some(index);
            let (name_role, description_role) = if picked {
                (Role::InputSelected, Role::InputSelected)
            } else {
                (Role::InputText, Role::Muted)
            };
            let description = truncate(&item.description, columns.saturating_sub(width(&name) + 1));
            rows.push(background_row(
                theme,
                &[(name_role, &name), (description_role, &description)],
                columns,
            ));
        }
        rows.push(background_row(theme, &[], columns));
        rows.push(self.status_row());
        Layout {
            rows,
            cursor_row,
            cursor_column,
        }
    }

    /// `─ Ready ────` with the number of waiting messages.
    fn separator(&self) -> String {
        let lang = self.lang;
        let mut label = match &self.activity {
            Activity::Ready => texts::ready(lang).to_owned(),
            Activity::Generating => texts::generating(lang).to_owned(),
            Activity::Running(tool) => texts::running(tool, lang),
            Activity::Compacting => texts::compacting(lang).to_owned(),
        };
        if self.queued > 0 {
            label.push_str(&format!(" · {}", texts::queued(self.queued, lang)));
        }
        let label = truncate(&format!(" {label} "), self.columns.saturating_sub(1));
        let fill = self.columns.saturating_sub(1 + width(&label));
        format!(
            "{}{}{}",
            self.theme.paint(Role::InputRule, "─"),
            self.theme.paint(Role::InputActivity, &label),
            self.theme.paint(Role::InputRule, &"─".repeat(fill))
        )
    }

    fn status_row(&self) -> String {
        let columns = self.columns;
        let usage = context_usage(self.context_used, self.context);
        // The usage is dropped when the row is too narrow for it.
        let usage = Some(usage).filter(|(text, _)| columns >= 3 * width(text));
        let usage_width = usage.as_ref().map_or(0, |(text, _)| width(text) + 2);
        let room = columns.saturating_sub(4 + usage_width);
        let workspace = truncate_start(&self.workspace, room / 2 + room % 2);
        let function = truncate(&self.function, room.saturating_sub(width(&workspace)));
        let left = format!("  {function}");
        let right = format!("{workspace}  ");
        let gap = columns.saturating_sub(width(&left) + usage_width + width(&right));
        let usage = usage
            .map(|(text, role)| format!("{}{}", self.theme.paint(role, &text), "  "))
            .unwrap_or_default();
        format!(
            "{}{}{}{}",
            self.theme.paint(Role::Muted, &left),
            " ".repeat(gap),
            usage,
            self.theme.paint(Role::Muted, &right)
        )
    }

    /// The visible input rows, each with its marker, and the row and
    /// column of the cursor among them.
    fn input_rows(&self, avail: usize) -> (Vec<(&'static str, String)>, usize, usize) {
        let buffer = self.editor.buffer();
        if buffer.is_empty() {
            return (vec![("› ", truncate(texts::hint(self.lang), avail))], 0, 2);
        }
        let cursor = self.editor.cursor();
        let mut rows: Vec<String> = Vec::new();
        let (mut cursor_row, mut cursor_column) = (0, 2);
        let mut line_start = 0;
        for line in buffer.split('\n') {
            let line_end = line_start + line.len();
            let mut chunks = hard_wrap(line, avail);
            // A cursor at the end of a full row goes to a row of its own.
            if cursor == line_end && chunks.last().is_some_and(|chunk| width(chunk) >= avail) {
                chunks.push(String::new());
            }
            let mut chunk_start = line_start;
            for (index, chunk) in chunks.iter().enumerate() {
                let chunk_end = chunk_start + chunk.len();
                let last = index + 1 == chunks.len();
                if cursor >= chunk_start && (cursor < chunk_end || (last && cursor <= chunk_end)) {
                    cursor_row = rows.len();
                    cursor_column = 2 + width(&buffer[chunk_start..cursor]);
                }
                rows.push(chunk.clone());
                chunk_start = chunk_end;
            }
            line_start = line_end + 1;
        }
        // Show the rows around the cursor when there are too many.
        let total = rows.len();
        let start = (cursor_row + 1)
            .saturating_sub(MAX_INPUT_ROWS)
            .min(total.saturating_sub(MAX_INPUT_ROWS));
        let visible = rows
            .into_iter()
            .enumerate()
            .skip(start)
            .take(MAX_INPUT_ROWS)
            .map(|(index, row)| (if index == 0 { "› " } else { "  " }, row))
            .collect();
        (visible, cursor_row - start, cursor_column)
    }
}

/// A row on the input background with styled segments, padded to the
/// width of the terminal.
fn background_row(theme: &Theme, segments: &[(Role, &str)], columns: usize) -> String {
    let background = theme.start(Role::InputBackground);
    let reset = theme.reset();
    let mut row = String::new();
    let mut used = 0;
    for (role, text) in segments {
        let text = truncate(text, columns.saturating_sub(used));
        used += width(&text);
        row.push_str(&format!("{background}{}{text}{reset}", theme.start(*role)));
    }
    let pad = " ".repeat(columns.saturating_sub(used));
    row.push_str(&format!("{background}{pad}{reset}"));
    row
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::repl::editor::MenuItem;

    /// A writer whose bytes the test reads.
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Sink {
        fn take(&self) -> String {
            String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap())).unwrap()
        }
    }

    fn dock(columns: usize, rows: usize) -> (Dock, Sink) {
        let sink = Sink::default();
        let commands = ["/quit", "/help"]
            .into_iter()
            .map(|name| MenuItem {
                name,
                description: "does it".into(),
            })
            .collect();
        let dock = Dock::new(
            Box::new(sink.clone()),
            Theme::plain(),
            Lang::En,
            "default".into(),
            "/home/u/project".into(),
            Editor::new(Vec::new(), commands),
            (columns, rows),
        )
        .with_context(131_072);
        (dock, sink)
    }

    fn type_text(dock: &Dock, text: &str) {
        for ch in text.chars() {
            dock.key(Key::Char(ch));
        }
    }

    #[test]
    fn empty_dock() {
        let (dock, _) = dock(40, 20);
        let layout = dock.lock().layout();
        assert_eq!(
            layout.rows,
            [
                format!("─ Ready {}", "─".repeat(32)),
                " ".repeat(40),
                format!("› {}", pad("Type a message or / for commands", 38)),
                " ".repeat(40),
                "  default    — / 131\u{a0}072  …e/u/project  ".to_string(),
            ]
        );
        assert_eq!((layout.cursor_row, layout.cursor_column), (2, 2));
    }

    fn pad(text: &str, columns: usize) -> String {
        format!("{text}{}", " ".repeat(columns - width(text)))
    }

    #[test]
    fn typed_text_many_lines_and_queue() {
        let (dock, _) = dock(30, 20);
        type_text(&dock, "hello");
        dock.key(Key::Newline);
        type_text(&dock, "ab");
        dock.set_queued(2);
        dock.activity(Activity::Running("bash".into()));
        let layout = dock.lock().layout();
        assert_eq!(
            layout.rows[0],
            format!("─ Running bash · queued 2 {}", "─".repeat(4))
        );
        assert_eq!(layout.rows[2], pad("› hello", 30));
        assert_eq!(layout.rows[3], pad("  ab", 30));
        assert_eq!((layout.cursor_row, layout.cursor_column), (3, 4));
    }

    #[test]
    fn cursor_follows_wrapped_rows() {
        let (dock, _) = dock(12, 20);
        // 10 columns of text per row.
        type_text(&dock, "0123456789ab");
        let layout = dock.lock().layout();
        assert_eq!(layout.rows[2], pad("› 0123456789", 12));
        assert_eq!(layout.rows[3], pad("  ab", 12));
        assert_eq!((layout.cursor_row, layout.cursor_column), (3, 4));
        // At the end of a full row the cursor starts a row of its own.
        dock.clear_input();
        type_text(&dock, "0123456789");
        let layout = dock.lock().layout();
        assert_eq!(layout.rows[3], pad("  ", 12));
        assert_eq!((layout.cursor_row, layout.cursor_column), (3, 2));
        dock.key(Key::Home);
        let layout = dock.lock().layout();
        assert_eq!((layout.cursor_row, layout.cursor_column), (2, 2));
    }

    #[test]
    fn many_input_rows_scroll_around_the_cursor() {
        let (dock, _) = dock(20, 30);
        for n in 0..9 {
            type_text(&dock, &format!("line{n}"));
            dock.key(Key::Newline);
        }
        let layout = dock.lock().layout();
        // 10 logical lines, 6 shown: the cursor is on the last one.
        let input: Vec<&String> = layout.rows[2..8].iter().collect();
        assert!(input[0].starts_with("  line4"));
        assert!(input[5].starts_with("  "));
        assert_eq!(layout.cursor_row, 7);
    }

    #[test]
    fn menu_rows_under_the_input() {
        let (dock, _) = dock(30, 20);
        type_text(&dock, "/");
        let layout = dock.lock().layout();
        assert_eq!(layout.rows[3], pad("  /quit     does it", 30));
        assert_eq!(layout.rows[4], pad("  /help     does it", 30));
        assert_eq!(layout.rows.len(), 7);
        // The cursor stays in the input.
        assert_eq!((layout.cursor_row, layout.cursor_column), (2, 3));
    }

    #[test]
    fn printing_moves_above_the_dock() {
        let (dock, sink) = dock(20, 20);
        dock.start();
        let first = sink.take();
        // First draw: no moving up; the cursor ends on the input row.
        assert!(first.contains("─ Ready"));
        assert!(first.contains("\x1b[2A"));
        dock.print(&["one".into(), "two".into()]);
        let out = sink.take();
        // Back to the top of the dock, clear, lines, the dock again.
        assert!(out.starts_with("\x1b[?2026h\r\x1b[2A\x1b[J"));
        assert!(out.contains("one\r\ntwo\r\n\x1b[?25l─ Ready"));
        assert!(out.ends_with("\x1b[?25h\x1b[?2026l"));
    }

    #[test]
    fn erasing_needs_the_lines_on_the_screen() {
        let (dock, sink) = dock(20, 12);
        dock.start();
        dock.print(&["a".into(), "b".into(), "c".into()]);
        sink.take();
        assert!(dock.erase(2));
        // The dock is 5 rows; the cursor is 2 rows below its top: up 2 + 2.
        assert!(sink.take().contains("\r\x1b[4A\x1b[J"));
        assert!(!dock.erase(5));
        // Lines wider than the window take more rows.
        let wide = "x".repeat(60);
        dock.print(&[wide.clone(), wide.clone(), wide]);
        assert!(!dock.erase(3));
        // A resize forgets what was printed.
        dock.print(&["y".into()]);
        dock.resize((30, 12));
        assert!(!dock.erase(1));
    }

    #[test]
    fn context_usage_is_grouped_and_colored_by_fill() {
        assert_eq!(
            context_usage(None, 131_072),
            ("— / 131\u{a0}072".into(), Role::Muted)
        );
        assert_eq!(
            context_usage(Some(41_213), 131_072),
            ("41\u{a0}213 / 131\u{a0}072 (31%)".into(), Role::Muted)
        );
        assert_eq!(context_usage(Some(999), 1000).1, Role::StatusFailure);
        assert_eq!(context_usage(Some(899), 1000).1, Role::StatusPending);
        assert_eq!(context_usage(Some(699), 1000).1, Role::Muted);
        assert_eq!(grouped(7), "7");
        assert_eq!(grouped(1000), "1\u{a0}000");
    }

    #[test]
    fn the_status_row_follows_the_last_prompt() {
        let (dock, _) = dock(80, 20);
        dock.context(41_213);
        let layout = dock.lock().layout();
        assert!(
            layout
                .rows
                .last()
                .unwrap()
                .contains("41\u{a0}213 / 131\u{a0}072 (31%)")
        );
        assert_eq!(width(layout.rows.last().unwrap()), 80);
    }

    #[test]
    fn long_texts_are_cut_to_fit() {
        let (dock, _) = dock(16, 20);
        {
            let mut state = dock.lock();
            state.function = "a-very-long-function-name".into();
            state.workspace = "/a/very/long/workspace/path".into();
        }
        let layout = dock.lock().layout();
        let status = layout.rows.last().unwrap();
        assert_eq!(width(status), 16);
        assert!(status.contains('…'));
        dock.live("partial");
        assert_eq!(dock.lock().layout().rows[0], "partial");
    }
}
