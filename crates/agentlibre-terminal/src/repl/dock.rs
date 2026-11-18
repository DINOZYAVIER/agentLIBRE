//! The dock at the bottom of the terminal and the transcript above it
//! (D119, D135, D138): the activity's row, the input rows on their
//! background, and the command menu. The transcript is printed above the
//! dock, which is drawn again after every change. AGL-104, D3: the dock
//! no longer carries a status row with the Function, the workspace and the
//! context's usage — `/status` says all of that when it is asked.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

use super::editor::{Editor, Outcome};
use super::term::Key;
use super::transcript::{Screen, label};
use super::reading::Reading;
use super::text::{hard_wrap, truncate, width};
use crate::i18n::{Lang, m, repl as texts};
use crate::phase::Phase;
use crate::style::Role;
use crate::theme::Theme;
use crate::tr;

/// The most input rows the dock shows.
const MAX_INPUT_ROWS: usize = 6;
/// The most menu rows the dock shows.
const MAX_MENU_ROWS: usize = 6;
/// How many printed lines are remembered for erasing.
const REMEMBERED_LINES: usize = 400;

/// `n` with a no-break space between groups of three digits.
pub(super) fn grouped(n: usize) -> String {
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

/// The state of the voice input, shown after the activity (AGL-016, D9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoiceLabel {
    /// Seconds recorded so far.
    Recording(u64),
    Transcribing,
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
    /// The voice input, in the separator row (AGL-016).
    voice: Option<VoiceLabel>,
    /// The decoding speed of the current generation (AGL-012).
    speed: Option<f64>,
    /// The reading of the prompt going on (AGL-046, D19).
    reading: Option<Reading>,
    editor: Editor,
    activity: Phase,
    queued: usize,
    live: Vec<String>,
    columns: usize,
    rows: usize,
    /// Rows of the dock on the screen, and the row of the cursor in it.
    drawn: Option<(usize, usize)>,
    /// Terminal rows of each printed line, newest last.
    printed: VecDeque<usize>,
    /// Whether the last printed line was blank: two blank rows never stand
    /// one under the other (AGL-097).
    last_blank: bool,
}

pub struct Dock {
    state: Mutex<State>,
}

impl Dock {
    pub fn new(
        out: Box<dyn Write + Send>,
        theme: Theme,
        lang: Lang,
        editor: Editor,
        size: (usize, usize),
    ) -> Dock {
        Dock {
            state: Mutex::new(State {
                out,
                theme,
                lang,
                voice: None,
                speed: None,
                reading: None,
                editor,
                activity: Phase::Ready,
                queued: 0,
                live: Vec::new(),
                columns: size.0.max(8),
                rows: size.1.max(4),
                drawn: None,
                printed: VecDeque::new(),
                last_blank: false,
            }),
        }
    }

    /// The line being written.
    #[cfg(test)]
    pub fn input_text(&self) -> String {
        self.lock().editor.buffer().to_owned()
    }

    /// Draws the text of the inspect mode (AGL-066) to the output, without
    /// a redraw of the dock.
    pub fn write_raw(&self, text: &str) {
        let mut state = self.lock();
        let _ = state.out.write_all(text.as_bytes());
        let _ = state.out.flush();
    }

    /// Shows the state of the voice input in the separator row, or
    /// removes it (AGL-016).
    pub fn set_voice(&self, voice: Option<VoiceLabel>) {
        let mut state = self.lock();
        if state.voice != voice {
            state.voice = voice;
            state.redraw("");
        }
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

impl Dock {
    /// The dock's rows, computed without drawing (the inspect mode
    /// draws them at the bottom of its screen, AGL-066 D14).
    pub fn layout(&self) -> Layout {
        self.lock().layout()
    }

    /// The time moves while a prompt is read, though nothing else tells the
    /// dock to draw (AGL-046, D19).
    pub fn tick(&self) {
        let mut state = self.lock();
        if state.reading.is_some() {
            state.redraw("");
        }
    }
}

impl Screen for Dock {
    fn columns(&self) -> usize {
        self.lock().columns
    }

    fn room(&self) -> usize {
        let state = self.lock();
        // The dock's own rows are the layout's less the live block, which
        // the room is meant for (AGL-092, D3).
        state.rows.saturating_sub(state.layout().rows.len() - state.live.len())
    }

    fn print(&self, lines: &[String]) {
        let mut state = self.lock();
        let mut text = String::new();
        for line in lines {
            // A block begins with a blank row and another ends with one:
            // two blank rows in a row say nothing (AGL-097).
            if line.is_empty() && state.last_blank {
                continue;
            }
            state.last_blank = line.is_empty();
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

    fn reading(&self, reading: Option<Reading>) {
        let mut state = self.lock();
        state.reading = reading;
        state.redraw("");
    }

    fn speed(&self, speed: Option<f64>) {
        let mut state = self.lock();
        if state.speed != speed {
            state.speed = speed;
            state.redraw("");
        }
    }

    fn live(&self, lines: &[String]) {
        let mut state = self.lock();
        if state.live != lines {
            state.live = lines.to_vec();
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

    fn activity(&self, activity: Phase) {
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
        rows.extend(self.live.iter().cloned());
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
                // D4: a command shines in the tool's color, its description
                // is the plain color of a message's text.
                (Role::Tool, Role::InputText)
            };
            let description = truncate(&item.description, columns.saturating_sub(width(&name) + 1));
            rows.push(background_row(
                theme,
                &[(name_role, &name), (description_role, &description)],
                columns,
            ));
        }
        // AGL-104, D3: the status row and the blank above it are gone;
        // `/status` keeps that information. AGL-107, D2: one free row stays
        // under the input, so the terminal never clips the row the cursor
        // stands on.
        rows.push(background_row(theme, &[], columns));
        Layout {
            rows,
            cursor_row,
            cursor_column,
        }
    }

    /// The activity's row (AGL-104, D2): the words alone — `Ready · queued
    /// 2` — with the number of waiting messages, the reading, the speed and
    /// the voice; no rule around them.
    fn separator(&self) -> String {
        let lang = self.lang;
        let mut label = label(&self.activity, lang);
        if let Some(reading) = &self.reading {
            label.push_str(&format!(" · {}", reading.label(Instant::now(), lang)));
        }
        if self.queued > 0 {
            label.push_str(&format!(" · {}", tr!(lang, m::QUEUED, n = self.queued)));
        }
        if let Some(speed) = self.speed {
            label.push_str(&format!(" · {}", texts::speed(speed, lang)));
        }
        match self.voice {
            Some(VoiceLabel::Recording(seconds)) => {
                let time = format!("{}:{:02}", seconds / 60, seconds % 60);
                label.push_str(&format!(" · {}", tr!(lang, m::VOICE_RECORDING, time = time)));
            }
            Some(VoiceLabel::Transcribing) => {
                label.push_str(&format!(" · {}", tr!(lang, m::VOICE_TRANSCRIBING)));
            }
            None => {}
        }
        self.theme
            .paint(Role::Tool, &truncate(&label, self.columns))
    }

    /// The visible input rows, each with its marker, and the row and
    /// column of the cursor among them. The marker is `› `, two columns, and
    /// the text begins right after it (AGL-094, D1). At rest the cursor
    /// stands on the text's first column — one further from the arrow than
    /// the marker's last column of AGL-088, D1, at the human's word — and
    /// the hint begins one further again, so neither crowns the arrow.
    fn input_rows(&self, avail: usize) -> (Vec<(&'static str, String)>, usize, usize) {
        let buffer = self.editor.buffer();
        if buffer.is_empty() {
            let hint = format!(" {}", truncate(tr!(self.lang, m::HINT), avail.saturating_sub(1)));
            return (vec![("› ", hint)], 0, 2);
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
                    cursor_column = if cursor == 0 {
                        2
                    } else {
                        2 + width(&buffer[chunk_start..cursor])
                    };
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
            Editor::new(Vec::new(), commands),
            (columns, rows),
        );
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
                "Ready".to_string(),
                " ".repeat(40),
                format!("› {}", pad(" Type a message or / for commands", 38)),
                " ".repeat(40),
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
        dock.activity(Phase::Running("bash".into()));
        let layout = dock.lock().layout();
        assert_eq!(
            layout.rows[0],
            "Running bash · queued 2" 
        );
        assert_eq!(layout.rows[2], pad("› hello", 30));
        assert_eq!(layout.rows[3], pad("  ab", 30));
        assert_eq!((layout.cursor_row, layout.cursor_column), (3, 4));
    }

    #[test]
    fn the_separator_row_carries_the_decoding_speed() {
        let (dock, _) = dock(40, 20);
        dock.activity(Phase::Writing);
        let layout = dock.lock().layout();
        assert_eq!(layout.rows[0], "Writing");
        drop(layout);
        dock.speed(Some(46.3));
        let layout = dock.lock().layout();
        assert_eq!(layout.rows[0], "Writing · 46.3 tok/s");
        drop(layout);
        dock.speed(None);
        let layout = dock.lock().layout();
        assert_eq!(layout.rows[0], "Writing");
    }

    #[test]
    fn cursor_follows_wrapped_rows() {
        let (dock, _) = dock(12, 20);
        // 10 columns of text per row: 12, less the marker's two.
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
        // AGL-088, D1: at the start the cursor rests on the marker's last
        // column, left of the first letter.
        assert_eq!((layout.cursor_row, layout.cursor_column), (2, 2));
    }

    #[test]
    fn the_cursor_at_the_start_rests_left_of_the_text() {
        // AGL-088, D1: the hint's first letter is never under the block:
        // empty, at the start of a typed line and at the end of it.
        let (dock, _) = dock(40, 20);
        let layout = dock.lock().layout();
        assert_eq!((layout.cursor_row, layout.cursor_column), (2, 2));
        drop(layout);
        type_text(&dock, "hi");
        let layout = dock.lock().layout();
        assert_eq!((layout.cursor_row, layout.cursor_column), (2, 4));
        dock.key(Key::Home);
        assert_eq!(dock.lock().layout().cursor_column, 2);
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
        assert_eq!(layout.rows.len(), 6);
        // The cursor stays in the input.
        assert_eq!((layout.cursor_row, layout.cursor_column), (2, 3));
    }

    #[test]
    fn a_menu_row_divides_its_two_colors() {
        // D4: a command is the tool's bright color, its description the
        // color of a message's text; the picked row is the selection's.
        let color = |hex: &str| crate::style::Style::parse(hex, false).expect("a color");
        let theme = Theme::new(
            true,
            &[
                (Role::Tool, color("#FF0000")),
                (Role::InputText, color("#00FF00")),
                (Role::InputSelected, color("#0000FF")),
            ],
        );
        let commands = ["/quit", "/help"]
            .into_iter()
            .map(|name| MenuItem { name, description: "does it".into() })
            .collect();
        let dock = Dock::new(
            Box::new(Sink::default()),
            theme,
            Lang::En,
            Editor::new(Vec::new(), commands),
            (30, 20),
        );
        let start = |role: Role| dock.lock().theme.start(role);
        let rows = || dock.lock().layout().rows.clone();
        type_text(&dock, "/");
        let row = rows()[3].clone();
        let name = row.find(&start(Role::Tool)).expect("the tool's color");
        let text = row.find(&start(Role::InputText)).expect("the text's color");
        assert!(name < text, "{row}");
        dock.key(Key::Down);
        let rows = rows();
        let picked = rows[3].clone();
        assert!(picked.contains(&start(Role::InputSelected)), "{picked}");
        assert!(!picked.contains(&start(Role::InputText)), "{picked}");
        assert!(rows[4].contains(&start(Role::Tool)), "{}", rows[4]);
    }

    #[test]
    fn the_room_above_the_dock() {
        // AGL-092, D3, AGL-104, D3, AGL-107, D2: the dock's own rows — the
        // separator, a blank, the input and the free row under it — are what
        // the terminal has less those; the live block's rows are not.
        let (dock, _) = dock(40, 20);
        assert_eq!(dock.room(), 16);
        dock.live(&["one".into(), "two".into()]);
        assert_eq!(dock.room(), 16);
    }

    #[test]
    fn printing_moves_above_the_dock() {
        let (dock, sink) = dock(20, 20);
        dock.start();
        let first = sink.take();
        // First draw: no moving up; the cursor ends on the input row, with
        // the dock's free row below it (AGL-107, D2).
        assert!(first.contains("Ready"));
        assert!(!first.contains("\x1b[2A"));
        assert!(first.contains("›"));
        dock.print(&["one".into(), "two".into()]);
        let out = sink.take();
        // Back to the top of the dock, clear, lines, the dock again.
        assert!(out.starts_with("\x1b[?2026h\r\x1b[2A\x1b[J"));
        assert!(out.contains("one\r\ntwo\r\n\x1b[?25lReady"));
        assert!(out.ends_with("\x1b[?25h\x1b[?2026l"));
    }

    #[test]
    fn two_blank_rows_never_stand_one_under_the_other() {
        // AGL-097: a block that ends with a blank row and a block that
        // begins with one give a single blank row between them.
        let (dock, sink) = dock(20, 20);
        dock.start();
        sink.take();
        dock.print(&["one".into(), String::new(), String::new(), "two".into()]);
        assert!(sink.take().contains("one\r\n\r\ntwo\r\n"));
        dock.print(&[String::new(), "x".into(), String::new()]);
        assert!(sink.take().contains("x\r\n"));
        // The blank row that would follow a blank row brings no rows at all.
        dock.print(&[String::new()]);
        assert!(sink.take().contains("\x1b[J\x1b[?25l"));
    }

    #[test]
    fn erasing_needs_the_lines_on_the_screen() {
        let (dock, sink) = dock(20, 12);
        dock.start();
        dock.print(&["a".into(), "b".into(), "c".into()]);
        sink.take();
        assert!(dock.erase(2));
        // The dock is 4 rows, the cursor on its third: up 2 for the cursor
        // and 2 for the printed rows: up 4.
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
    fn grouped_numbers_use_a_no_break_space() {
        assert_eq!(grouped(7), "7");
        assert_eq!(grouped(1000), "1\u{a0}000");
    }

    #[test]
    fn the_separator_shows_the_voice_input_after_the_activity() {
        let (dock, _) = dock(80, 20);
        let separator = |dock: &Dock| dock.lock().separator();
        dock.set_voice(Some(VoiceLabel::Recording(7)));
        assert!(
            separator(&dock).contains("Ready · Recording 0:07 · Ctrl-R to stop"),
            "{}",
            separator(&dock)
        );
        dock.set_voice(Some(VoiceLabel::Recording(61)));
        assert!(separator(&dock).contains("Recording 1:01"));
        dock.set_voice(Some(VoiceLabel::Transcribing));
        assert!(separator(&dock).contains("Ready · Transcribing…"));
        // A turn runs beside it (AGL-016, D7): both are shown.
        dock.activity(Phase::Writing);
        assert!(separator(&dock).contains("Writing · Transcribing…"));
        dock.set_voice(None);
        assert!(!separator(&dock).contains("Transcribing"));
        assert!(separator(&dock).contains("Writing"));
    }

    #[test]
    fn the_separators_activity_takes_the_tools_color() {
        // AGL-091, AGL-104, D2: `Ready`, `Starting server` and the rest of
        // the separator's words are the tool's color alone — there is no
        // rule left to color.
        let color = |hex: &str| crate::style::Style::parse(hex, false).expect("a color");
        let theme = Theme::new(
            true,
            &[
                (Role::Tool, color("#FF0000")),
                (Role::InputRule, color("#00FF00")),
            ],
        );
        let dock = Dock::new(
            Box::new(Sink::default()),
            theme,
            Lang::En,
            Editor::new(Vec::new(), Vec::new()),
            (80, 20),
        );
        let start = |role: Role| dock.lock().theme.start(role);
        dock.activity(Phase::StartingServer);
        let row = dock.lock().separator();
        assert!(row.starts_with(&start(Role::Tool)), "{row}");
        assert!(row.contains("Starting the server"), "{row}");
        assert!(!row.contains('─'), "{row}");
        assert!(!row.contains(&start(Role::InputRule)), "{row}");
    }

    #[test]
    fn the_separator_is_cut_to_the_screen() {
        // AGL-104, D2: the words alone, and never more of them than the
        // screen has columns.
        let (dock, _) = dock(16, 20);
        dock.activity(Phase::Running("a-very-long-tool-name".into()));
        let row = dock.lock().separator();
        assert_eq!(width(&row), 16);
        assert!(row.contains('…'), "{row}");
        dock.live(&["partial".into()]);
        assert_eq!(dock.lock().layout().rows[0], "partial");
    }

    #[test]
    fn the_dock_has_no_status_row() {
        // AGL-104, D3: the dock ends with its input and its menu over a
        // free row; the Function, the workspace and the context's usage
        // live in `/status`.
        let (dock, _) = dock(60, 20);
        let layout = dock.lock().layout();
        assert_eq!(layout.rows.len(), 4);
        assert_eq!(layout.rows[3], " ".repeat(60));
        assert!(!layout.rows.join("\n").contains("131\u{a0}072"));
        assert!(!layout.rows.join("\n").contains("minimal"));
    }
}
