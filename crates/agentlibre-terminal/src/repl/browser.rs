//! The Function browser on Ctrl-F (AGL-109): the alternate screen with
//! the list of the Functions, their sources and what each hides. agl
//! never edits here: `e` hands the file to the external editor, and what
//! the editor left is validated — the same words as `agl function check`,
//! plus what the change means: the restart, the budgets.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::fs;

use super::dock::Dock;
use super::term::{self, Key};
use super::text::{hard_wrap, truncate};
use crate::function::Function;
use crate::function::catalog::{Catalog, Entry, Source};
use crate::i18n::{Lang, m};
use crate::style::Role;
use crate::theme::Theme;
use crate::tr;

/// What leaving the browser means (AGL-109, D3).
pub enum Exit {
    /// Esc, q or Ctrl-F again: nothing was asked.
    Nothing,
    /// `s`: the main thread leaves the session for that Function.
    Switch(String),
}

/// What the editor of [`spawn_editor`] left (AGL-109, D4).
pub enum Edited {
    /// The editor's command did not start.
    Failed(String),
    /// The bytes are what they were.
    Unchanged,
    /// The file holds other bytes; `check` says what a run makes of
    /// them — the words of `agl function check`, or the error.
    Saved { check: String, error: bool },
}

/// What one key of the list asks of the caller.
pub enum KeyOutcome {
    Stay,
    Leave,
    /// `e`: the file goes to the external editor (AGL-109, D4).
    Edit(String, PathBuf),
    /// `s`: the main thread leaves the session for that Function.
    Switch(String),
}

/// The browser's state; `key` moves it, `lines` draws it.
pub struct Browser {
    entries: Vec<Entry>,
    at: usize,
    /// The source of the entry being read, with the row it starts at.
    source: Option<(String, usize)>,
    /// The name typed for `c`; the row of keys becomes the question.
    clone: Option<String>,
    /// The last thing the browser has to say; it lives until the next key.
    message: Option<String>,
    message_failed: bool,
}

impl Browser {
    pub fn new(entries: Vec<Entry>) -> Browser {
        let at = entries.iter().position(|e| e.current).unwrap_or(0);
        Browser {
            entries,
            at,
            source: None,
            clone: None,
            message: None,
            message_failed: false,
        }
    }

    fn selected(&self) -> Option<&Entry> {
        self.entries.get(self.at)
    }

    /// The name of the entry, when it has no error to hide it.
    fn name(&self) -> Option<&str> {
        self.selected()
            .filter(|e| e.error.is_none())
            .map(|e| e.name.as_str())
    }

    fn say(&mut self, text: String, failed: bool) {
        self.message = Some(text);
        self.message_failed = failed;
    }

    /// One key of the mode; what it asks of the caller, if anything.
    pub fn key(&mut self, key: Key, catalog: &Catalog, lang: Lang, busy: bool) -> KeyOutcome {
        self.message = None;
        self.message_failed = false;
        // `Ctrl-F` twice leaves, as Ctrl-O does with itself.
        if key == Key::Ctrl('f') {
            return KeyOutcome::Leave;
        }
        // The question of `c` takes every key while it stands.
        if let Some(typed) = &mut self.clone {
            match key {
                Key::Escape => self.clone = None,
                Key::Backspace => { typed.pop(); }
                Key::Enter | Key::Newline => {
                    let name = std::mem::take(typed);
                    self.clone = None;
                    self.clone_as(&name, catalog, lang);
                }
                Key::Char(c) if !c.is_control() => typed.push(c),
                _ => {}
            }
            return KeyOutcome::Stay;
        }
        // The source being read takes the arrows and nothing else.
        if let Some((_, top)) = &mut self.source {
            match key {
                Key::Escape | Key::Enter => self.source = None,
                Key::Down | Key::Char('j') => *top += 1,
                Key::Up | Key::Char('k') => *top = top.saturating_sub(1),
                _ => {}
            }
            return KeyOutcome::Stay;
        }
        match key {
            Key::Escape | Key::Char('q') => KeyOutcome::Leave,
            Key::Down | Key::Char('j') => {
                self.down();
                KeyOutcome::Stay
            }
            Key::Up | Key::Char('k') => {
                self.at = self.at.saturating_sub(1);
                KeyOutcome::Stay
            }
            Key::Enter => {
                if let Some(name) = self.name() {
                    match catalog.text(name) {
                        Ok(text) => self.source = Some((text, 0)),
                        Err(error) => self.say(error.message(lang), true),
                    }
                }
                KeyOutcome::Stay
            }
            Key::Char('e') => {
                // The three states of the row: an error, a built-in file
                // that belongs to the repository (AGL-102, D4), or a file
                // the editor can take.
                let row = self.selected().map(|e| (e.name.clone(), e.source, e.error.clone()));
                match row {
                    Some((_, _, Some(error))) => {
                        self.say(error, true);
                        KeyOutcome::Stay
                    }
                    Some((name, Source::BuiltIn, None)) => {
                        self.say(tr!(lang, m::BROWSER_BUILTIN, name = name), false);
                        KeyOutcome::Stay
                    }
                    Some((name, _, None)) => match catalog.file(&name) {
                        Ok(file) => KeyOutcome::Edit(name, file.path),
                        Err(error) => {
                            self.say(error.message(lang), true);
                            KeyOutcome::Stay
                        }
                    },
                    None => KeyOutcome::Stay,
                }
            }
            Key::Char('c') if self.name().is_some() => {
                self.clone = Some(String::new());
                KeyOutcome::Stay
            }
            Key::Char('s') => {
                if let Some(name) = self.name() {
                    if busy {
                        // The turn answers first, as `/function <name>` does.
                        self.say(tr!(lang, m::BUSY, name = "Ctrl-F"), true);
                        KeyOutcome::Stay
                    } else {
                        KeyOutcome::Switch(name.to_owned())
                    }
                } else {
                    KeyOutcome::Stay
                }
            }
            _ => KeyOutcome::Stay,
        }
    }

    /// Whether the browser owns the key; the rest go to the input
    /// (AGL-098, D7): the arrows and `jkqec s` do, the typing does not.
    pub fn takes(&self, key: &Key) -> bool {
        match key {
            Key::Ctrl('f') | Key::Escape | Key::Enter | Key::Up | Key::Down => true,
            Key::Char(c) if self.clone.is_some() => !matches!(c, '\n' | '\r'),
            Key::Char(c) => matches!(c, 'q' | 'j' | 'k' | 'e' | 'c' | 's'),
            _ => false,
        }
    }

    fn down(&mut self) {
        if !self.entries.is_empty() {
            self.at = (self.at + 1).min(self.entries.len() - 1);
        }
    }

    /// `c` answered: the copy goes to the workspace's `functions/`, the
    /// same way `agl function new` puts it (AGL-102, D3).
    fn clone_as(&mut self, name: &str, catalog: &Catalog, lang: Lang) {
        let from = self.entries[self.at].name.clone();
        match catalog.text(&from).and_then(|text| catalog.create(name, &text)) {
            Ok(path) => {
                self.say(
                    tr!(lang, m::BROWSER_CLONED, name = name.to_owned(), path = path.display()),
                    false,
                );
                self.entries = catalog.entries(lang);
                if let Some(at) = self.entries.iter().position(|e| e.name == name) {
                    self.at = at;
                }
            }
            Err(error) => self.say(error.message(lang), true),
        }
    }

    /// What the editor left (AGL-109, D4).
    pub fn note_edited(&mut self, name: &str, edited: Edited, catalog: &Catalog, lang: Lang) {
        match edited {
            Edited::Failed(reason) => {
                self.say(tr!(lang, m::BROWSER_EDITOR_FAILED, editor = editor_command().join(" "), reason = reason), true);
            }
            Edited::Unchanged => {
                self.say(tr!(lang, m::BROWSER_UNCHANGED, name = name.to_owned()), false);
            }
            Edited::Saved { check, error } => {
                self.say(check, error);
                self.entries = catalog.entries(lang);
            }
        }
    }

    /// The rows of the screen above the dock: the list, or the source
    /// being read, and below them the row of keys — or what the browser
    /// has to say, until the next key.
    pub fn lines(&self, theme: &Theme, lang: Lang, columns: usize, room: usize) -> Vec<String> {
        let room = room.max(2);
        let shown = room - 1;
        let mut lines: Vec<String> = Vec::new();
        if let Some((text, top)) = &self.source {
            let all: Vec<String> = text
                .split('\n')
                .flat_map(|line| hard_wrap(line, columns))
                .collect();
            let top = (*top).min(all.len().saturating_sub(1));
            lines.extend(all.into_iter().skip(top).take(shown));
        } else {
            let from = self.at.saturating_sub(shown.saturating_sub(1));
            for entry in self.entries.iter().skip(from).take(shown) {
                lines.push(self.entry_line(theme, lang, columns, entry));
            }
        }
        let hint = match (&self.clone, &self.message) {
            (Some(typed), _) => tr!(
                lang,
                m::BROWSER_CLONE_ASK,
                name = self.entries[self.at].name.clone(),
                typed = typed.clone()
            )
            .to_string(),
            (None, Some(message)) => message.clone(),
            (None, None) => tr!(lang, m::BROWSER_KEYS).to_string(),
        };
        let role = if self.message_failed { Role::StatusFailure } else { Role::Muted };
        lines.push(theme.paint(role, &truncate(&hint, columns)));
        lines
    }

    fn entry_line(&self, theme: &Theme, lang: Lang, columns: usize, entry: &Entry) -> String {
        let mark = if entry.current { '*' } else { ' ' };
        let rest = match &entry.error {
            Some(error) => error.clone(),
            None => {
                let model = entry
                    .model
                    .clone()
                    .unwrap_or_else(|| tr!(lang, m::FUNCTION_NO_MODEL).to_string());
                format!("{}  {model}", entry.kind.name())
            }
        };
        let line = truncate(&format!("{mark} {:<22} {:<9}{rest}", entry.name, entry.source.label(lang)), columns);
        let role = if entry.error.is_some() {
            Role::StatusFailure
        } else if entry.current {
            Role::Tool
        } else {
            Role::InputText
        };
        theme.paint(role, &line)
    }
}

/// The editor's command: `VISUAL`, then `EDITOR`, then `vi` (AGL-109, D4).
pub fn editor_command() -> Vec<String> {
    env::var("VISUAL")
        .or_else(|_| env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into())
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

/// Hands the screen to the editor and checks what it left (AGL-109, D4,
/// D5): the cooked terminal and the main screen go to the child, as to
/// `git commit`, and the raw mode and the alternate screen return after.
/// The second of the pair says that the screen is the browser's again.
pub fn spawn_editor(write: &dyn Fn(&str), path: &Path, lang: Lang) -> (Edited, bool) {
    let before = fs::read_to_string(path).unwrap_or_default();
    let command = editor_command();
    let shown = command.join(" ");
    let (Some(program), args) = (command.first(), &command[1..]) else {
        return (Edited::Failed(shown), false);
    };
    write(term::MOUSE_OFF);
    write(term::ALT_OFF);
    term::suspend_raw();
    match Command::new(program)
        .args(args)
        .arg(path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
    {
        Err(error) => {
            term::resume_raw();
            write(term::ALT_ON);
            write(term::MOUSE_ON);
            (Edited::Failed(format!("{shown}: {error}")), true)
        }
        Ok(_) => {
            term::resume_raw();
            write(term::ALT_ON);
            write(term::MOUSE_ON);
            let after = fs::read_to_string(path).unwrap_or_default();
            if after == before {
                return (Edited::Unchanged, true);
            }
            let checked = crate::nix::eval_file(path)
                .and_then(|value| Function::from_value(path, value))
                .map_err(|error| error.message(lang));
            match checked {
                Ok(function) => {
                    let name = path
                        .file_stem()
                        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
                    (
                        Edited::Saved {
                            check: tr!(
                                lang,
                                m::BROWSER_SAVED,
                                name = name,
                                kind = function.model.kind().name(),
                                model = crate::engine::model_of(&function.model)
                                    .ok()
                                    .flatten()
                                    .unwrap_or_default(),
                                context = function.model.context.unwrap_or_default()
                            ),
                            error: false,
                        },
                        true,
                    )
                }
                Err(problem) => (Edited::Saved { check: problem, error: true }, true),
            }
        }
    }
}

/// The mode on the terminal, as `inspect::run`: the alternate screen,
/// the full redraws, the dock's rows at the bottom, the input live
/// (AGL-098, D7): its text goes to `submit`, as from the inspect mode.
pub fn run(
    write: &dyn Fn(&str),
    theme: &Theme,
    lang: Lang,
    catalog: &Catalog,
    dock: &Dock,
    busy: &dyn Fn() -> bool,
    submit: &dyn Fn(&str),
) -> Exit {
    let mut browser = Browser::new(catalog.entries(lang));
    let mut parser = term::Parser::default();
    let mut buffer = [0u8; 4096];
    let mut size = term::size();
    write(term::ALT_ON);
    write(term::MOUSE_ON);
    let mut exit = Exit::Nothing;
    loop {
        if term::take_resized() {
            size = term::size();
        }
        let rows = dock.layout();
        let room = size.1.saturating_sub(rows.rows.len());
        let mut lines = browser.lines(theme, lang, size.0, room);
        lines.extend(rows.rows);
        write(&lines.join("\r\n"));
        let up = lines.len() - 1 - (room + rows.cursor_row);
        if up > 0 {
            write(&format!("\x1b[{up}A"));
        }
        write("\r");
        if rows.cursor_column > 0 {
            write(&format!("\x1b[{}C", rows.cursor_column));
        }
        let keys = loop {
            match term::read_input(&mut buffer, 100) {
                Ok(None) => {
                    if parser.waiting() {
                        break parser.timeout();
                    }
                    continue;
                }
                Ok(Some(0)) | Err(_) => break vec![Key::Escape],
                Ok(Some(n)) => break parser.feed(&buffer[..n]),
            }
        };
        let mut open = true;
        for key in keys {
            // A key the browser does not take belongs to the input, and
            // its Enter sends and leaves (AGL-098, D7).
            if !browser.takes(&key) {
                if let super::editor::Outcome::Submit(message) = dock.key(key) {
                    submit(&message);
                    open = false;
                    break;
                }
                continue;
            }
            match browser.key(key, catalog, lang, busy()) {
                KeyOutcome::Stay => {}
                KeyOutcome::Leave => {
                    open = false;
                    break;
                }
                KeyOutcome::Switch(name) => {
                    exit = Exit::Switch(name);
                    open = false;
                    break;
                }
                KeyOutcome::Edit(name, path) => {
                    let (edited, _) = spawn_editor(write, &path, lang);
                    browser.note_edited(&name, edited, catalog, lang);
                }
            }
        }
        if !open {
            break;
        }
    }
    write(term::MOUSE_OFF);
    write(term::ALT_OFF);
    exit
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::testutil::TempDir;
    use crate::settings::ServerFile;

    const LLAMA: &str = r#"{ kind = "llama"; format = agl.formats."qwen3.8"; weights = "p.gguf"; context = 4096; }"#;

    fn file() -> String {
        r#"{ agl, ... }: { policy = { system = "s"; initial = "work"; state.work.answer = true; }; model = MODEL_PLACEHOLDER; }"#
            .replace("MODEL_PLACEHOLDER", LLAMA)
    }

    /// Two user Functions, `mine` the current one; the order of `entries`.
    fn fixture(dir: &Path) -> Catalog {
        let catalog = Catalog {
            plane_dir: None,
            workspace_dir: dir.join("workspace"),
            user_dir: dir.join("user"),
            settings: ServerFile::default(),
            current: "mine".into(),
        };
        fs::create_dir_all(&catalog.user_dir).unwrap();
        for name in ["mine", "other"] {
            fs::write(catalog.user_dir.join(format!("{name}.nix")), file()).unwrap();
        }
        catalog
    }

    fn builtin(name: &str) -> Entry {
        Entry {
            name: name.into(),
            source: Source::BuiltIn,
            shadows: Vec::new(),
            kind: crate::settings::ServerKind::Llama,
            model: Some("m.gguf".into()),
            description: None,
            error: None,
            current: false,
        }
    }

    fn press(browser: &mut Browser, catalog: &Catalog, keys: &[Key]) -> KeyOutcome {
        let mut last = KeyOutcome::Stay;
        for key in keys {
            last = browser.key(key.clone(), catalog, Lang::En, false);
            if !matches!(last, KeyOutcome::Stay) {
                return last;
            }
        }
        last
    }

    #[test]
    fn the_cursor_stands_on_the_current_function() {
        let dir = TempDir::new();
        let catalog = fixture(&dir.0);
        // The cursor stands on the current one, wherever it sorts.
        let current = catalog
            .entries(Lang::En)
            .iter()
            .position(|e| e.current)
            .unwrap();
        let browser = Browser::new(catalog.entries(Lang::En));
        assert_eq!(browser.at, current);
        assert_eq!(browser.name(), Some("mine"));
    }

    #[test]
    fn a_key_the_browser_does_not_take_belongs_to_the_input() {
        let dir = TempDir::new();
        let catalog = fixture(&dir.0);
        let browser = Browser::new(catalog.entries(Lang::En));
        assert!(!browser.takes(&Key::Char('x')));
        assert!(browser.takes(&Key::Char('j')));
        assert!(browser.takes(&Key::Up));
    }

    #[test]
    fn a_switch_is_asked_only_when_the_thread_is_free() {
        let dir = TempDir::new();
        let catalog = fixture(&dir.0);
        let mut browser = Browser::new(catalog.entries(Lang::En));
        assert!(matches!(browser.key(Key::Char('s'), &catalog, Lang::En, false), KeyOutcome::Switch(name) if name == "mine"));
        // The turn answers first, as `/function <name>` does.
        let mut browser = Browser::new(catalog.entries(Lang::En));
        assert!(matches!(browser.key(Key::Char('s'), &catalog, Lang::En, true), KeyOutcome::Stay));
        let lines = browser.lines(&Theme::plain(), Lang::En, 80, 10);
        // The words of `/function` on a busy thread (AGL-036, D9).
        assert!(lines.last().unwrap().contains("Ctrl-F"), "{}", lines.last().unwrap());
    }

    #[test]
    fn a_builtin_opens_nowhere_and_offers_the_clone() {
        let dir = TempDir::new();
        let catalog = fixture(&dir.0);
        let mut browser = Browser::new(vec![builtin("stock")]);
        let outcome = browser.key(Key::Char('e'), &catalog, Lang::En, false);
        assert!(matches!(outcome, KeyOutcome::Stay));
        let lines = browser.lines(&Theme::plain(), Lang::En, 80, 10);
        assert!(lines.last().unwrap().contains("clones"), "{}", lines.last().unwrap());
    }

    #[test]
    fn the_clone_asks_a_name_and_writes_the_copy() {
        let dir = TempDir::new();
        let catalog = fixture(&dir.0);
        let mut browser = Browser::new(catalog.entries(Lang::En));
        press(&mut browser, &catalog, &[Key::Char('c')]);
        // The row of keys became the question.
        let lines = browser.lines(&Theme::plain(), Lang::En, 80, 10);
        assert!(lines.last().unwrap().contains("clone `mine` as:"), "{}", lines.last().unwrap());
        press(&mut browser, &catalog, &[Key::Char('m'), Key::Char('y'), Key::Enter]);
        assert!(
            catalog.workspace_dir.join("my.nix").is_file(),
            "the copy went to the workspace's functions/"
        );
        let lines = browser.lines(&Theme::plain(), Lang::En, 80, 10);
        assert!(lines.last().unwrap().contains("cloned to my"), "{}", lines.last().unwrap());
        assert_eq!(browser.name(), Some("my"));
    }

    #[test]
    fn an_empty_clone_name_writes_nothing() {
        let dir = TempDir::new();
        let catalog = fixture(&dir.0);
        let mut browser = Browser::new(catalog.entries(Lang::En));
        press(&mut browser, &catalog, &[Key::Char('c'), Key::Enter]);
        assert!(!browser.lines(&Theme::plain(), Lang::En, 80, 10).last().unwrap().contains("cloned"));
    }

    #[test]
    fn what_the_editor_left_rules_the_row() {
        let dir = TempDir::new();
        let catalog = fixture(&dir.0);
        let mut browser = Browser::new(catalog.entries(Lang::En));
        browser.note_edited(
            "mine",
            Edited::Saved { check: "mine: loads, llama, p.gguf, 4096 of context".into(), error: false },
            &catalog,
            Lang::En,
        );
        let lines = browser.lines(&Theme::plain(), Lang::En, 80, 10);
        assert!(lines.last().unwrap().contains("loads"), "{}", lines.last().unwrap());
        // A broken save says so, and the list stands.
        browser.note_edited("mine", Edited::Saved { check: "no `model`".into(), error: true }, &catalog, Lang::En);
        assert!(browser.lines(&Theme::plain(), Lang::En, 80, 10).last().unwrap().contains("no `model`"));
        browser.note_edited("mine", Edited::Unchanged, &catalog, Lang::En);
        assert!(browser.lines(&Theme::plain(), Lang::En, 80, 10).last().unwrap().contains("as it was"));
    }

    #[test]
    fn the_enter_shows_the_source_and_the_arrows_walk_it() {
        let dir = TempDir::new();
        let catalog = fixture(&dir.0);
        let mut browser = Browser::new(catalog.entries(Lang::En));
        assert!(matches!(browser.key(Key::Enter, &catalog, Lang::En, false), KeyOutcome::Stay));
        let lines = browser.lines(&Theme::plain(), Lang::En, 40, 6);
        assert!(lines.iter().any(|l| l.contains("policy")), "the source shows: {lines:?}");
        assert!(browser.source.is_some());
        browser.key(Key::Escape, &catalog, Lang::En, false);
        assert!(browser.source.is_none());
    }
}
