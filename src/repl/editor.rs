//! The line editor of the REPL: a buffer with a cursor, history and the
//! menu of `/` commands (D116). It knows nothing about the terminal.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use super::term::Key;

/// How many entries the history file keeps (D133).
pub const HISTORY_LIMIT: usize = 1000;

/// A `/` command the menu offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuItem {
    pub name: &'static str,
    pub description: String,
}

/// What a key did.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The buffer or the menu changed, or nothing happened.
    Edited,
    /// Enter: the message to send.
    Submit(String),
    /// Ctrl-D on an empty buffer.
    Quit,
    /// Ctrl-C; the caller decides (D120).
    Interrupt,
}

pub struct Editor {
    buffer: String,
    /// Byte offset of the cursor.
    cursor: usize,
    history: Vec<String>,
    /// The history entry shown, if any.
    position: Option<usize>,
    /// What was typed before walking into the history.
    draft: String,
    commands: Vec<MenuItem>,
    /// The item picked with Up/Down in the menu.
    selected: Option<usize>,
    /// Escape closed the menu; typing opens it again.
    dismissed: bool,
}

impl Editor {
    pub fn new(history: Vec<String>, commands: Vec<MenuItem>) -> Editor {
        Editor {
            buffer: String::new(),
            cursor: 0,
            history,
            position: None,
            draft: String::new(),
            commands,
            selected: None,
            dismissed: false,
        }
    }

    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Empties the buffer (Ctrl-C at the prompt, D120).
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.position = None;
        self.edited();
    }

    /// The commands matching what is typed, when the menu is open.
    pub fn menu(&self) -> Vec<&MenuItem> {
        if self.dismissed
            || !self.buffer.starts_with('/')
            || self.buffer.contains(char::is_whitespace)
        {
            return Vec::new();
        }
        self.commands
            .iter()
            .filter(|item| item.name.starts_with(&self.buffer))
            .collect()
    }

    /// Which menu item is picked.
    pub fn selected(&self) -> Option<usize> {
        let len = self.menu().len();
        self.selected.filter(|index| *index < len)
    }

    pub fn handle(&mut self, key: Key) -> Outcome {
        match key {
            Key::Char(ch) => self.insert(ch.encode_utf8(&mut [0; 4])),
            Key::Paste(text) => {
                let clean: String = text
                    .chars()
                    .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
                    .collect();
                self.insert(&clean);
            }
            Key::Newline => self.insert("\n"),
            Key::Tab => self.complete(),
            Key::Enter => return self.submit(),
            Key::Backspace => self.backspace(),
            Key::Delete => self.delete(),
            Key::Left => self.cursor = self.prev_boundary(self.cursor),
            Key::Right => self.cursor = self.next_boundary(self.cursor),
            Key::WordLeft | Key::Alt('b') => self.cursor = self.word_start(self.cursor),
            Key::WordRight | Key::Alt('f') => self.cursor = self.word_end(self.cursor),
            Key::Home | Key::Ctrl('a') => self.cursor = self.line_start(self.cursor),
            Key::End | Key::Ctrl('e') => self.cursor = self.line_end(self.cursor),
            Key::Ctrl('u') => {
                let start = self.line_start(self.cursor);
                self.remove(start..self.cursor);
            }
            Key::Ctrl('k') => {
                let end = self.line_end(self.cursor);
                self.remove(self.cursor..end);
            }
            Key::Ctrl('w') | Key::Alt('\x7f') => {
                let start = self.word_start(self.cursor);
                self.remove(start..self.cursor);
            }
            Key::Alt('d') => {
                let end = self.word_end(self.cursor);
                self.remove(self.cursor..end);
            }
            Key::Up => self.up(),
            Key::Down => self.down(),
            Key::Ctrl('p') => self.previous_entry(),
            Key::Ctrl('n') => self.next_entry(),
            Key::Escape => {
                if !self.menu().is_empty() {
                    self.dismissed = true;
                    self.selected = None;
                }
            }
            Key::Ctrl('d') => {
                if self.buffer.is_empty() {
                    return Outcome::Quit;
                }
                self.delete();
            }
            Key::Ctrl('c') => return Outcome::Interrupt,
            Key::BackTab | Key::Ctrl(_) | Key::Alt(_) => {}
        }
        Outcome::Edited
    }

    /// Sends the buffer. A command picked in the menu is sent as picked.
    fn submit(&mut self) -> Outcome {
        if let Some(index) = self.selected() {
            let name = self.menu()[index].name;
            self.buffer = name.into();
        }
        let message = std::mem::take(&mut self.buffer);
        self.cursor = 0;
        self.position = None;
        self.draft.clear();
        self.edited();
        Outcome::Submit(message)
    }

    /// Remembers a sent message (D133); true when the list changed.
    pub fn remember(&mut self, message: &str) -> bool {
        if message.trim().is_empty() || self.history.last().is_some_and(|last| last == message) {
            return false;
        }
        self.history.push(message.into());
        if self.history.len() > HISTORY_LIMIT {
            self.history.remove(0);
        }
        true
    }

    pub fn history(&self) -> &[String] {
        &self.history
    }

    fn edited(&mut self) {
        self.selected = None;
        self.dismissed = false;
    }

    fn insert(&mut self, text: &str) {
        self.buffer.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.position = None;
        self.edited();
    }

    fn remove(&mut self, range: std::ops::Range<usize>) {
        self.buffer.drain(range.clone());
        self.cursor = range.start;
        self.position = None;
        self.edited();
    }

    fn backspace(&mut self) {
        let start = self.prev_boundary(self.cursor);
        self.remove(start..self.cursor);
    }

    fn delete(&mut self) {
        let end = self.next_boundary(self.cursor);
        self.remove(self.cursor..end);
    }

    /// Tab: with a menu, fills in the picked or the first command.
    fn complete(&mut self) {
        let menu = self.menu();
        if menu.is_empty() {
            return;
        }
        let name = menu[self.selected().unwrap_or(0)].name;
        self.buffer = name.into();
        self.cursor = self.buffer.len();
        self.selected = None;
        self.dismissed = true;
    }

    fn up(&mut self) {
        let menu_len = self.menu().len();
        if menu_len > 0 {
            self.selected = Some(match self.selected() {
                Some(index) => index.checked_sub(1).unwrap_or(menu_len - 1),
                None => menu_len - 1,
            });
        } else if self.buffer[..self.cursor].contains('\n') {
            self.move_line(-1);
        } else {
            self.previous_entry();
        }
    }

    fn down(&mut self) {
        let menu_len = self.menu().len();
        if menu_len > 0 {
            self.selected = Some(match self.selected() {
                Some(index) => (index + 1) % menu_len,
                None => 0,
            });
        } else if self.buffer[self.cursor..].contains('\n') {
            self.move_line(1);
        } else {
            self.next_entry();
        }
    }

    /// Moves the cursor one line up or down, keeping its column.
    fn move_line(&mut self, direction: i32) {
        let start = self.line_start(self.cursor);
        let column = self.buffer[start..self.cursor].chars().count();
        let target = if direction < 0 {
            let before = start - 1;
            self.line_start(before)..before
        } else {
            let from = self.line_end(self.cursor) + 1;
            from..self.line_end(from)
        };
        let line = &self.buffer[target.clone()];
        let offset = line
            .char_indices()
            .nth(column)
            .map_or(line.len(), |(index, _)| index);
        self.cursor = target.start + offset;
    }

    fn previous_entry(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.position {
            None => {
                self.draft = self.buffer.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(index) => index - 1,
        };
        self.show_entry(Some(index));
    }

    fn next_entry(&mut self) {
        match self.position {
            None => {}
            Some(index) if index + 1 < self.history.len() => self.show_entry(Some(index + 1)),
            Some(_) => self.show_entry(None),
        }
    }

    fn show_entry(&mut self, index: Option<usize>) {
        self.buffer = match index {
            Some(index) => self.history[index].clone(),
            None => std::mem::take(&mut self.draft),
        };
        self.cursor = self.buffer.len();
        self.position = index;
        self.selected = None;
        self.dismissed = true;
    }

    fn prev_boundary(&self, at: usize) -> usize {
        self.buffer[..at]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    fn next_boundary(&self, at: usize) -> usize {
        self.buffer[at..]
            .chars()
            .next()
            .map_or(at, |ch| at + ch.len_utf8())
    }

    fn line_start(&self, at: usize) -> usize {
        self.buffer[..at].rfind('\n').map_or(0, |index| index + 1)
    }

    fn line_end(&self, at: usize) -> usize {
        self.buffer[at..]
            .find('\n')
            .map_or(self.buffer.len(), |index| at + index)
    }

    fn word_start(&self, at: usize) -> usize {
        let before = &self.buffer[..at];
        let trimmed = before.trim_end_matches(char::is_whitespace);
        trimmed.rfind(char::is_whitespace).map_or(0, |index| {
            index + trimmed[index..].chars().next().map_or(1, char::len_utf8)
        })
    }

    fn word_end(&self, at: usize) -> usize {
        let after = &self.buffer[at..];
        let skipped = after.len() - after.trim_start_matches(char::is_whitespace).len();
        let rest = &after[skipped..];
        let word = rest.find(char::is_whitespace).unwrap_or(rest.len());
        at + skipped + word
    }
}

/// Reads the history file, one JSON string per line (D133). A missing or
/// partly broken file gives what could be read.
pub fn load_history(path: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut entries: Vec<String> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<String>(line).ok())
        .filter(|entry| !entry.is_empty())
        .collect();
    let excess = entries.len().saturating_sub(HISTORY_LIMIT);
    entries.drain(..excess);
    entries
}

/// Writes the whole history (D133); the state directory is created.
pub fn save_history(path: &Path, entries: &[String]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut text = String::new();
    for entry in entries {
        text.push_str(&serde_json::to_string(entry).expect("strings serialize"));
        text.push('\n');
    }
    let mut file = fs::File::create(path)?;
    file.write_all(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> Editor {
        let commands = ["/quit", "/exit", "/help", "/status", "/new"]
            .into_iter()
            .map(|name| MenuItem {
                name,
                description: String::new(),
            })
            .collect();
        Editor::new(vec!["first".into(), "second\nline".into()], commands)
    }

    fn type_text(editor: &mut Editor, text: &str) {
        for ch in text.chars() {
            editor.handle(Key::Char(ch));
        }
    }

    #[test]
    fn typing_and_cursor_keys_with_utf8() {
        let mut e = editor();
        type_text(&mut e, "привіт");
        assert_eq!((e.buffer(), e.cursor()), ("привіт", 12));
        e.handle(Key::Left);
        e.handle(Key::Left);
        e.handle(Key::Backspace);
        assert_eq!(e.buffer(), "приіт");
        e.handle(Key::Delete);
        assert_eq!(e.buffer(), "прит");
        e.handle(Key::Home);
        type_text(&mut e, "> ");
        assert_eq!(e.buffer(), "> прит");
        e.handle(Key::End);
        assert_eq!(e.cursor(), e.buffer().len());
    }

    #[test]
    fn line_and_word_editing() {
        let mut e = editor();
        type_text(&mut e, "one two three");
        e.handle(Key::Ctrl('w'));
        assert_eq!(e.buffer(), "one two ");
        e.handle(Key::WordLeft);
        assert_eq!(e.cursor(), 4);
        e.handle(Key::WordRight);
        assert_eq!(e.cursor(), 7);
        e.handle(Key::Ctrl('u'));
        assert_eq!(e.buffer(), " ");
        type_text(&mut e, "abc");
        e.handle(Key::Left);
        e.handle(Key::Ctrl('k'));
        assert_eq!(e.buffer(), "ab");
    }

    #[test]
    fn multi_line_messages() {
        let mut e = editor();
        type_text(&mut e, "ab");
        e.handle(Key::Newline);
        type_text(&mut e, "cdef");
        assert_eq!(e.buffer(), "ab\ncdef");
        // Up moves between lines, keeping the column, before history.
        e.handle(Key::Up);
        assert_eq!(e.cursor(), 2);
        e.handle(Key::Down);
        assert_eq!(e.cursor(), 5);
        e.handle(Key::Home);
        assert_eq!(e.cursor(), 3);
        assert_eq!(e.handle(Key::Enter), Outcome::Submit("ab\ncdef".into()));
        assert!(e.is_empty());
        e.handle(Key::Paste("x\ny\u{7}z".into()));
        assert_eq!(e.buffer(), "x\nyz");
    }

    #[test]
    fn history_walk_keeps_the_draft() {
        let mut e = editor();
        type_text(&mut e, "draft");
        e.handle(Key::Up);
        assert_eq!(e.buffer(), "second\nline");
        // In a many-line entry Up first moves between its lines.
        e.handle(Key::Up);
        assert_eq!(e.buffer(), "second\nline");
        e.handle(Key::Up);
        assert_eq!(e.buffer(), "first");
        e.handle(Key::Up);
        assert_eq!(e.buffer(), "first");
        e.handle(Key::Down);
        assert_eq!(e.buffer(), "second\nline");
        e.handle(Key::Down);
        assert_eq!(e.buffer(), "draft");
        e.handle(Key::Ctrl('p'));
        e.handle(Key::Ctrl('n'));
        assert_eq!(e.buffer(), "draft");
    }

    #[test]
    fn remembering() {
        let mut e = editor();
        assert!(e.remember("third"));
        assert!(!e.remember("third"));
        assert!(!e.remember("  \n"));
        assert_eq!(e.history().len(), 3);
        for n in 0..HISTORY_LIMIT + 5 {
            e.remember(&format!("m{n}"));
        }
        assert_eq!(e.history().len(), HISTORY_LIMIT);
        assert_eq!(
            e.history().last().unwrap(),
            &format!("m{}", HISTORY_LIMIT + 4)
        );
    }

    #[test]
    fn command_menu() {
        let mut e = editor();
        type_text(&mut e, "/");
        assert_eq!(e.menu().len(), 5);
        type_text(&mut e, "e");
        let names: Vec<_> = e.menu().iter().map(|item| item.name).collect();
        assert_eq!(names, ["/exit"]);
        e.handle(Key::Tab);
        assert_eq!(e.buffer(), "/exit");
        // Filled in: the menu stays closed until the next edit.
        assert!(e.menu().is_empty());
        e.clear();
        type_text(&mut e, "/");
        e.handle(Key::Down);
        e.handle(Key::Down);
        assert_eq!(e.selected(), Some(1));
        e.handle(Key::Up);
        e.handle(Key::Up);
        assert_eq!(e.selected(), Some(4));
        // Enter sends the picked command.
        assert_eq!(e.handle(Key::Enter), Outcome::Submit("/new".into()));
        // Typed text is sent as typed.
        type_text(&mut e, "/he");
        assert_eq!(e.handle(Key::Enter), Outcome::Submit("/he".into()));
        type_text(&mut e, "/status now");
        assert!(e.menu().is_empty());
        e.clear();
        type_text(&mut e, "/");
        e.handle(Key::Escape);
        assert!(e.menu().is_empty());
        type_text(&mut e, "h");
        assert_eq!(e.menu().len(), 1);
        e.clear();
        type_text(&mut e, "text");
        assert!(e.menu().is_empty());
    }

    #[test]
    fn quit_and_interrupt_keys() {
        let mut e = editor();
        assert_eq!(e.handle(Key::Ctrl('d')), Outcome::Quit);
        assert_eq!(e.handle(Key::Ctrl('c')), Outcome::Interrupt);
        type_text(&mut e, "ab");
        e.handle(Key::Left);
        assert_eq!(e.handle(Key::Ctrl('d')), Outcome::Edited);
        assert_eq!(e.buffer(), "a");
    }

    #[test]
    fn history_file_round_trip() {
        let dir = std::env::temp_dir().join(format!("agentLIBRE-history-{}", std::process::id()));
        let path = dir.join("state/history.jsonl");
        assert!(load_history(&path).is_empty());
        let entries = vec!["a".to_owned(), "two\nlines \"q\"".to_owned()];
        save_history(&path, &entries).unwrap();
        assert_eq!(load_history(&path), entries);
        fs::write(&path, "\"ok\"\nnot json\n\"\"\n\"also\"\n").unwrap();
        assert_eq!(load_history(&path), ["ok", "also"]);
        let _ = fs::remove_dir_all(&dir);
    }
}
