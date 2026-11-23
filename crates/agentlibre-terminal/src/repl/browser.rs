//! The Function browser on Ctrl-F (AGL-109): the alternate screen with
//! the list of the Functions, their sources and what each hides, and a
//! second tab with the settings' files. agl never edits here: `e` hands
//! the file to the external editor, and what the editor left is checked —
//! the same words as `agl function check`, or the settings' own load.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::dock::{Dock, Layout};
use super::term::{self, Key};
use super::text::{hard_wrap, truncate};
use super::ModeKeys;
use crate::function::Function;
use crate::function::catalog::{Catalog, Entry, Source};
use agentlibre_common::i18n::{Lang, m};
use agentlibre_common::settings::UserFile;
use agentlibre_common::style::Role;
use crate::theme::Theme;
use agentlibre_common::tr;

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

/// One line of the settings' tab (AGL-109, D6).
#[derive(Clone)]
pub struct SettingsItem {
    pub label: String,
    pub path: PathBuf,
    /// The plane's file, when the run is in a plane; its path only.
    pub note: String,
}

/// What one key of the list asks of the caller.
pub enum KeyOutcome {
    Stay,
    Leave,
    /// `e`: the file goes to the external editor (AGL-109, D4).
    Edit(PathBuf),
    /// `s`: the main thread leaves the session for that Function.
    Switch(String),
}

/// The browser's state; `key` moves it, `lines` draws it.
pub struct Browser {
    entries: Vec<Entry>,
    at: usize,
    /// The settings' tab and its cursor (AGL-109, D6).
    settings: bool,
    set_at: usize,
    /// The source of the entry being read, with the row it starts at.
    source: Option<(String, usize)>,
    /// The name typed for `c`; the row of keys becomes the question.
    clone: Option<String>,
    /// `d` was pressed once; `d` again erases the file (AGL-109, D3).
    delete: bool,
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
            settings: false,
            set_at: 0,
            source: None,
            clone: None,
            delete: false,
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

    fn note(&mut self, text: String, failed: bool) {
        self.message = Some(text);
        self.message_failed = failed;
    }

    /// Whether the browser owns the key; the rest go to the input
    /// (AGL-098, D7): the arrows and `jkqecsd\t` do, the typing does not.
    pub fn takes(&self, key: &Key) -> bool {
        match key {
            Key::Ctrl('f') | Key::Escape | Key::Enter | Key::Up | Key::Down | Key::Tab => true,
            Key::Char(c) if self.clone.is_some() || self.delete => !matches!(c, '\n' | '\r'),
            Key::Char(c) => matches!(c, 'q' | 'j' | 'k' | 'e' | 'c' | 's' | 'd'),
            _ => false,
        }
    }

    /// One key of the mode; what it asks of the caller, if anything.
    pub fn key(
        &mut self,
        key: Key,
        catalog: &Catalog,
        settings: &[SettingsItem],
        lang: Lang,
        busy: bool,
    ) -> KeyOutcome {
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
        // `d` answers itself: the second press erases, any other keeps.
        if self.delete {
            self.delete = false;
            if key == Key::Char('d') {
                self.erase(catalog, lang);
            }
            return KeyOutcome::Stay;
        }
        if self.settings {
            return self.key_settings(key, settings, lang);
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
            Key::Tab => {
                self.settings = true;
                KeyOutcome::Stay
            }
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
                        Err(error) => self.note(error.message(lang), true),
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
                        self.note(error, true);
                        KeyOutcome::Stay
                    }
                    Some((name, Source::BuiltIn, None)) => {
                        self.note(tr!(lang, m::BROWSER_BUILTIN, name = name), false);
                        KeyOutcome::Stay
                    }
                    Some((_, _, None)) => match self.selected_file(catalog) {
                        Ok(path) => KeyOutcome::Edit(path),
                        Err(error) => {
                            self.note(error.message(lang), true);
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
            Key::Char('d') => {
                match self.selected().map(|e| e.source) {
                    Some(Source::BuiltIn) => {
                        let name = self.entries[self.at].name.clone();
                        self.note(tr!(lang, m::BROWSER_DELETE_BUILTIN, name = name), false);
                    }
                    Some(_) if self.name().is_some() => self.delete = true,
                    _ => {}
                }
                KeyOutcome::Stay
            }
            Key::Char('s') => {
                if let Some(name) = self.name() {
                    if busy {
                        // The turn answers first, as `/function <name>` does.
                        self.note(tr!(lang, m::BUSY, name = "Ctrl-F"), true);
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

    /// One key of the settings' tab (AGL-109, D6).
    fn key_settings(&mut self, key: Key, settings: &[SettingsItem], lang: Lang) -> KeyOutcome {
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
            Key::Tab => {
                self.settings = false;
                KeyOutcome::Stay
            }
            Key::Escape | Key::Char('q') => KeyOutcome::Leave,
            Key::Down | Key::Char('j') => {
                self.set_at = (self.set_at + 1).min(settings.len().saturating_sub(1));
                KeyOutcome::Stay
            }
            Key::Up | Key::Char('k') => {
                self.set_at = self.set_at.saturating_sub(1);
                KeyOutcome::Stay
            }
            Key::Enter => {
                if let Some(item) = settings.get(self.set_at) {
                    match fs::read_to_string(&item.path) {
                        Ok(text) => self.source = Some((text, 0)),
                        Err(error) => self.note(format!("{}: {error}", item.path.display()), true),
                    }
                }
                KeyOutcome::Stay
            }
            Key::Char('e') => {
                let item = settings.get(self.set_at).cloned();
                match item {
                    Some(item) => {
                        if !item.path.is_file() {
                            self.note(
                                tr!(lang, m::BROWSER_SETTINGS_MISSING, path = item.path.display()),
                                false,
                            );
                            return KeyOutcome::Stay;
                        }
                        KeyOutcome::Edit(item.path)
                    }
                    None => KeyOutcome::Stay,
                }
            }
            _ => KeyOutcome::Stay,
        }
    }

    fn down(&mut self) {
        if !self.entries.is_empty() {
            self.at = (self.at + 1).min(self.entries.len() - 1);
        }
    }

    /// The file the row stands on: the one a run would load (AGL-102, D2).
    fn selected_file(&self, catalog: &Catalog) -> Result<PathBuf, crate::error::Error> {
        Ok(catalog.file(&self.entries[self.at].name)?.path)
    }

    /// `d` answered twice: the file goes, the list is read anew
    /// (AGL-109, D3). The settings' files are not deletable here.
    fn erase(&mut self, catalog: &Catalog, lang: Lang) {
        let name = match self.name() {
            Some(name) => name.to_owned(),
            None => return,
        };
        let path = match catalog.file(&name) {
            Ok(file) => file.path,
            Err(error) => {
                self.note(error.message(lang), true);
                return;
            }
        };
        match fs::remove_file(&path) {
            Ok(()) => {
                self.note(tr!(lang, m::BROWSER_DELETED, name = name.clone()), false);
                self.entries = catalog.entries(lang);
                self.at = self
                    .entries
                    .iter()
                    .position(|e| e.name == name)
                    .unwrap_or_else(|| self.at.min(self.entries.len().saturating_sub(1)));
            }
            Err(error) => self.note(format!("{}: {error}", path.display()), true),
        }
    }

    /// `c` answered: the copy goes to the workspace's `functions/`, the
    /// same way `agl function new` puts it (AGL-102, D3).
    fn clone_as(&mut self, name: &str, catalog: &Catalog, lang: Lang) {
        let from = self.entries[self.at].name.clone();
        match catalog.text(&from).and_then(|text| catalog.create(name, &text)) {
            Ok(path) => {
                self.note(
                    tr!(lang, m::BROWSER_CLONED, name = name.to_owned(), path = path.display()),
                    false,
                );
                self.entries = catalog.entries(lang);
                if let Some(at) = self.entries.iter().position(|e| e.name == name) {
                    self.at = at;
                }
            }
            Err(error) => self.note(error.message(lang), true),
        }
    }

    /// What the editor left of a Function (AGL-109, D4).
    pub fn note_edited(&mut self, name: &str, edited: Edited, catalog: &Catalog, lang: Lang) {
        match edited {
            Edited::Failed(reason) => {
                self.note(tr!(lang, m::BROWSER_EDITOR_FAILED, editor = editor_command().join(" "), reason = reason), true);
            }
            Edited::Unchanged => {
                self.note(tr!(lang, m::BROWSER_UNCHANGED, name = name.to_owned()), false);
            }
            Edited::Saved { check, error } => {
                self.note(check, error);
                self.entries = catalog.entries(lang);
            }
        }
    }

    /// What the editor left of a settings' file (AGL-109, D6): its own
    /// load is the check, and a fresh file is the settings' default.
    pub fn note_settings_edited(&mut self, path: &Path, edited: Edited, lang: Lang) {
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let edited = match edited {
            Edited::Saved { check: _, error: false } => match UserFile::load(path) {
                Ok(_) => Edited::Saved {
                    check: tr!(lang, m::BROWSER_SETTINGS_SAVED, name = name).to_string(),
                    error: false,
                },
                Err(error) => Edited::Saved { check: error.message(lang), error: true },
            },
            other => other,
        };
        match edited {
            Edited::Failed(reason) => {
                self.note(tr!(lang, m::BROWSER_EDITOR_FAILED, editor = editor_command().join(" "), reason = reason), true);
            }
            Edited::Unchanged => {
                self.note(tr!(lang, m::BROWSER_UNCHANGED, name = name), false);
            }
            Edited::Saved { check, error } => self.note(check, error),
        }
    }

    /// The rows of the screen above the dock: the list of the Functions
    /// or of the settings, and below them the row of keys — or what the
    /// browser has to say, until the next key.
    pub fn lines(&self, theme: &Theme, lang: Lang, columns: usize, room: usize, settings: &[SettingsItem]) -> Vec<String> {
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
        } else if self.settings {
            let from = self.set_at.saturating_sub(shown.saturating_sub(1));
            for (n, item) in settings.iter().skip(from).take(shown).enumerate() {
                lines.push(self.settings_line(theme, columns, item, from + n == self.set_at));
            }
        } else {
            let from = self.at.saturating_sub(shown.saturating_sub(1));
            for entry in self.entries.iter().skip(from).take(shown) {
                lines.push(self.entry_line(theme, lang, columns, entry));
            }
        }
        let (hint, failed) = match (&self.clone, &self.delete, &self.message) {
            (Some(typed), _, _) => (
                tr!(
                    lang,
                    m::BROWSER_CLONE_ASK,
                    name = self.entries[self.at].name.clone(),
                    typed = typed.clone()
                )
                .to_string(),
                false,
            ),
            (None, true, _) => (
                tr!(lang, m::BROWSER_DELETE_ASK, name = self.entries[self.at].name.clone()).to_string(),
                false,
            ),
            (None, _, Some(message)) => (message.clone(), self.message_failed),
            (None, _, None) => (
                tr!(
                    lang,
                    if self.settings { m::BROWSER_SETTINGS_KEYS } else { m::BROWSER_KEYS }
                )
                .to_string(),
                false,
            ),
        };
        let role = if failed { Role::StatusFailure } else { Role::Muted };
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
        let line = truncate(
            &format!("{mark} {:<22} {:<9}{rest}", entry.name, entry.source.label(lang)),
            columns,
        );
        let role = if entry.error.is_some() {
            Role::StatusFailure
        } else if entry.current {
            Role::Tool
        } else {
            Role::InputText
        };
        theme.paint(role, &line)
    }

    fn settings_line(&self, theme: &Theme, columns: usize, item: &SettingsItem, picked: bool) -> String {
        let mark = if picked { '>' } else { ' ' };
        let line = truncate(
            &format!("{mark} {} — {} {}", item.label, item.note, item.path.display()),
            columns,
        );
        theme.paint(Role::InputText, &line)
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
pub fn spawn_editor(write: &dyn Fn(&str), path: &Path, lang: Lang) -> Edited {
    let before = fs::read_to_string(path).unwrap_or_default();
    let command = editor_command();
    let shown = command.join(" ");
    let (Some(program), args) = (command.first(), &command[1..]) else {
        return Edited::Failed(shown);
    };
    write(term::MOUSE_OFF);
    write(term::ALT_OFF);
    term::suspend_raw();
    let status = Command::new(program)
        .args(args)
        .arg(path)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    term::resume_raw();
    write(term::ALT_ON);
    write(term::MOUSE_ON);
    match status {
        Err(error) => Edited::Failed(format!("{shown}: {error}")),
        Ok(_) => {
            let after = fs::read_to_string(path).unwrap_or_default();
            if after == before {
                return Edited::Unchanged;
            }
            // A `settings.nix` is the settings' own to answer; a Function
            // is checked the way `agl function check` checks it (D5).
            if path.file_name().is_some_and(|name| name == "settings.nix") {
                return Edited::Saved { check: String::new(), error: false };
            }
            let checked = agentlibre_common::nix::eval_file(path)
                .map_err(Into::into)
                .and_then(|value| Function::from_value(path, value))
                .map_err(|error| error.message(lang));
            match checked {
                Ok(function) => {
                    let name = path
                        .file_stem()
                        .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
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
                        )
                        .to_string(),
                        error: false,
                    }
                }
                Err(problem) => Edited::Saved { check: problem, error: true },
            }
        }
    }
}

/// One frame on the alternate screen: the cleared field, the browser's
/// rows padded to `room`, the dock at the bottom, the caret on the dock's
/// input cell. Clearing every frame is what keeps a shorter screen — a
/// short list, a one-line message, a closed source view — from leaving the
/// rows of a taller one behind, which the relative redraw smeared
/// (AGL-109, D1). No trailing newline: it would scroll the field up a row.
fn frame(body: &[String], room: usize, dock: &Layout) -> String {
    let mut lines = body.to_vec();
    lines.truncate(room.max(1));
    lines.resize(room, String::new());
    lines.extend(dock.rows.iter().cloned());
    let mut text = String::from(term::CLEAR);
    text.push_str(term::HOME);
    text.push_str(&lines.join("\n"));
    text.push_str(&term::cursor_at(room + dock.cursor_row + 1, 1 + dock.cursor_column));
    text.push_str("\x1b[?25h");
    text
}

/// The mode on the terminal, as `inspect::run`: the alternate screen,
/// the full redraws, the dock's rows at the bottom, the input live
/// (AGL-098, D7): its text goes to `submit`, as from the inspect mode.
#[allow(clippy::too_many_arguments)]
pub fn run(
    write: &dyn Fn(&str),
    theme: &Theme,
    lang: Lang,
    catalog: &Catalog,
    settings: &[SettingsItem],
    dock: &Dock,
    busy: &dyn Fn() -> bool,
    submit: &dyn Fn(&str),
    keys: &ModeKeys,
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
        write(&frame(&browser.lines(theme, lang, size.0, room, settings), room, &rows));
        let pressed = loop {
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
        for key in pressed {
            // A key the browser does not take belongs to the input, and
            // its Enter sends and leaves (AGL-098, D7); its Ctrl-C stops
            // the turn and keeps the browser (AGL-112, D1), its Ctrl-D
            // leaves the browser and nothing else (D2).
            if !browser.takes(&key) {
                match dock.key(key) {
                    super::editor::Outcome::Submit(message) => {
                        submit(&message);
                        open = false;
                        break;
                    }
                    super::editor::Outcome::Interrupt => keys.interrupt(),
                    super::editor::Outcome::Quit => {
                        open = false;
                        break;
                    }
                    super::editor::Outcome::Edited => {}
                }
                continue;
            }
            // On the settings' tab `e` answers through the settings' load.
            let on_settings = browser.settings;
            match browser.key(key, catalog, settings, lang, busy()) {
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
                KeyOutcome::Edit(path) => {
                    let edited = spawn_editor(write, &path, lang);
                    if on_settings {
                        browser.note_settings_edited(&path, edited, lang);
                    } else {
                        let name = path
                            .file_stem()
                            .map_or_else(String::new, |s| s.to_string_lossy().into_owned());
                        browser.note_edited(&name, edited, catalog, lang);
                    }
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
    use super::super::dock::Layout;
    use agentlibre_common::testutil::TempDir;
    use agentlibre_common::settings::ServerFile;

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
            kind: agentlibre_common::settings::ServerKind::Llama,
            model: Some("m.gguf".into()),
            description: None,
            error: None,
            current: false,
        }
    }

    fn setting(path: &Path) -> SettingsItem {
        SettingsItem {
            label: "settings.nix".into(),
            path: path.to_path_buf(),
            note: "the user's".into(),
        }
    }

    fn press(browser: &mut Browser, catalog: &Catalog, keys: &[Key]) -> KeyOutcome {
        let mut last = KeyOutcome::Stay;
        for key in keys {
            last = browser.key(key.clone(), catalog, &[], Lang::En, false);
            if !matches!(last, KeyOutcome::Stay) {
                return last;
            }
        }
        last
    }

    fn hint(browser: &Browser, settings: &[SettingsItem]) -> String {
        browser.lines(&Theme::plain(), Lang::En, 80, 10, settings).last().unwrap().clone()
    }

    fn dock_layout(rows: &[&str], cursor_row: usize) -> Layout {
        Layout {
            rows: rows.iter().map(|r| r.to_string()).collect(),
            cursor_row,
            cursor_column: 2,
        }
    }

    #[test]
    fn a_frame_clears_and_pads_to_the_dock_and_homes_the_caret() {
        let dock = dock_layout(&["› hi", "  status"], 0);
        let text = frame(&["one".to_string(), "two".to_string()], 4, &dock);
        // Cleared, homed; no relative move that could smear (AGL-109, D1).
        assert!(text.starts_with(&format!("{}{}", term::CLEAR, term::HOME)), "{text}");
        assert!(!text.contains('A'), "no relative cursor-up: {text}");
        // `room` rows (two body, two blank) then the dock; caret on it.
        let body = text
            .strip_prefix(term::CLEAR)
            .and_then(|t| t.strip_prefix(term::HOME))
            .and_then(|t| t.strip_suffix("\x1b[?25h"))
            .expect("the frame own ends");
        let rows: Vec<&str> = body.split('\n').collect();
        assert_eq!(rows, ["one", "two", "", "", "› hi", "  status\x1b[5;3H"]);
        // A shorter body than the last frame pads, it does not stack.
        let short = frame(&["one".to_string()], 4, &dock);
        assert!(short.contains("one\n\n\n\n› hi"), "{short}");
        assert!(!short.contains("two"));
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
        assert!(browser.takes(&Key::Tab));
    }

    #[test]
    fn a_switch_is_asked_only_when_the_thread_is_free() {
        let dir = TempDir::new();
        let catalog = fixture(&dir.0);
        let mut browser = Browser::new(catalog.entries(Lang::En));
        assert!(matches!(browser.key(Key::Char('s'), &catalog, &[], Lang::En, false), KeyOutcome::Switch(name) if name == "mine"));
        // The turn answers first, as `/function <name>` does.
        let mut browser = Browser::new(catalog.entries(Lang::En));
        assert!(matches!(browser.key(Key::Char('s'), &catalog, &[], Lang::En, true), KeyOutcome::Stay));
        // The words of `/function` on a busy thread (AGL-036, D9).
        assert!(hint(&browser, &[]).contains("Ctrl-F"));
    }

    #[test]
    fn a_builtin_opens_nowhere_and_offers_the_clone() {
        let dir = TempDir::new();
        let catalog = fixture(&dir.0);
        let mut browser = Browser::new(vec![builtin("stock")]);
        let outcome = browser.key(Key::Char('e'), &catalog, &[], Lang::En, false);
        assert!(matches!(outcome, KeyOutcome::Stay));
        assert!(hint(&browser, &[]).contains("clones"), "{}", hint(&browser, &[]));
    }

    #[test]
    fn the_clone_asks_a_name_and_writes_the_copy() {
        let dir = TempDir::new();
        let catalog = fixture(&dir.0);
        let mut browser = Browser::new(catalog.entries(Lang::En));
        press(&mut browser, &catalog, &[Key::Char('c')]);
        // The row of keys became the question.
        assert!(hint(&browser, &[]).contains("clone `mine` as:"), "{}", hint(&browser, &[]));
        press(&mut browser, &catalog, &[Key::Char('m'), Key::Char('y'), Key::Enter]);
        assert!(
            catalog.workspace_dir.join("my.nix").is_file(),
            "the copy went to the workspace's functions/"
        );
        assert!(hint(&browser, &[]).contains("cloned to my"), "{}", hint(&browser, &[]));
        assert_eq!(browser.name(), Some("my"));
    }

    #[test]
    fn an_empty_clone_name_writes_nothing() {
        let dir = TempDir::new();
        let catalog = fixture(&dir.0);
        let mut browser = Browser::new(catalog.entries(Lang::En));
        press(&mut browser, &catalog, &[Key::Char('c'), Key::Enter]);
        assert!(!hint(&browser, &[]).contains("cloned"));
    }

    #[test]
    fn a_delete_asks_twice_and_erases_the_file() {
        let dir = TempDir::new();
        let catalog = fixture(&dir.0);
        let mut browser = Browser::new(catalog.entries(Lang::En));
        press(&mut browser, &catalog, &[Key::Char('d')]);
        assert!(hint(&browser, &[]).contains("delete mine?"), "{}", hint(&browser, &[]));
        // Any other key keeps the file, and its answer spends the question.
        press(&mut browser, &catalog, &[Key::Char('x')]);
        assert!(catalog.user_dir.join("mine.nix").is_file());
        // A fresh question, and its second press erases and says so.
        press(&mut browser, &catalog, &[Key::Char('d'), Key::Char('d')]);
        assert!(!catalog.user_dir.join("mine.nix").is_file(), "the second `d` erased it");
        assert!(hint(&browser, &[]).contains("deleted mine"), "{}", hint(&browser, &[]));
    }

    #[test]
    fn a_builtin_has_nothing_to_delete() {
        let dir = TempDir::new();
        let catalog = fixture(&dir.0);
        let mut browser = Browser::new(vec![builtin("stock")]);
        press(&mut browser, &catalog, &[Key::Char('d')]);
        assert!(hint(&browser, &[]).contains("repository"), "{}", hint(&browser, &[]));
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
        assert!(hint(&browser, &[]).contains("loads"), "{}", hint(&browser, &[]));
        // A broken save says so, and the list stands.
        browser.note_edited("mine", Edited::Saved { check: "no `model`".into(), error: true }, &catalog, Lang::En);
        assert!(hint(&browser, &[]).contains("no `model`"));
        browser.note_edited("mine", Edited::Unchanged, &catalog, Lang::En);
        assert!(hint(&browser, &[]).contains("as it was"));
    }

    #[test]
    fn the_tab_crosses_to_the_settings_and_back() {
        let dir = TempDir::new();
        let catalog = fixture(&dir.0);
        let items = vec![setting(&dir.0.join("settings.nix"))];
        let mut browser = Browser::new(catalog.entries(Lang::En));
        browser.key(Key::Tab, &catalog, &items, Lang::En, false);
        assert!(browser.settings);
        let lines = browser.lines(&Theme::plain(), Lang::En, 80, 10, &items);
        assert!(lines.last().unwrap().contains("Tab functions"), "{}", lines.last().unwrap());
        assert!(lines[0].contains("settings.nix"), "{}", lines[0]);
        browser.key(Key::Tab, &catalog, &items, Lang::En, false);
        assert!(!browser.settings);
    }

    #[test]
    fn a_settings_file_answers_with_its_own_load() {
        let dir = TempDir::new();
        let catalog = fixture(&dir.0);
        let path = dir.0.join("settings.nix");
        fs::write(&path, r#"{ agl, ... }: { function = "mine"; language = "en"; }"#).unwrap();
        let mut browser = Browser::new(catalog.entries(Lang::En));
        browser.settings = true;
        browser.note_settings_edited(&path, Edited::Saved { check: String::new(), error: false }, Lang::En);
        assert!(hint(&browser, &[]).contains("it loads"), "{}", hint(&browser, &[]));
        // A file that does not evaluate is the error's own.
        fs::write(&path, "{ agl, ... }: { nope = ; }").unwrap();
        browser.note_settings_edited(&path, Edited::Saved { check: String::new(), error: false }, Lang::En);
        assert!(!hint(&browser, &[]).contains("it loads"), "{}", hint(&browser, &[]));
    }

    #[test]
    fn the_enter_shows_the_source_and_the_arrows_walk_it() {
        let dir = TempDir::new();
        let catalog = fixture(&dir.0);
        let mut browser = Browser::new(catalog.entries(Lang::En));
        assert!(matches!(browser.key(Key::Enter, &catalog, &[], Lang::En, false), KeyOutcome::Stay));
        let lines = browser.lines(&Theme::plain(), Lang::En, 40, 6, &[]);
        assert!(lines.iter().any(|l| l.contains("policy")), "the source shows: {lines:?}");
        assert!(browser.source.is_some());
        browser.key(Key::Escape, &catalog, &[], Lang::En, false);
        assert!(browser.source.is_none());
    }
}
