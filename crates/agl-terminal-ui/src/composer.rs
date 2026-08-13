use std::collections::VecDeque;

use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

pub(super) const MAX_COMPOSER_BYTES: usize = 64 * 1024;
pub(super) const MAX_COMPOSER_LINES: usize = 2_000;
const MAX_UNDO_SNAPSHOTS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ComposerMode {
    Prompt,
    Shell,
    Command,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EditSnapshot {
    mode: ComposerMode,
    buffer: String,
    cursor: usize,
    selection_anchor: Option<usize>,
}

#[derive(Debug)]
pub(super) struct Composer {
    pub(super) mode: ComposerMode,
    pub(super) buffer: String,
    pub(super) cursor: usize,
    pub(super) selection_anchor: Option<usize>,
    pub(super) selected_command: usize,
    pub(super) history_position: Option<usize>,
    pub(super) history_draft: String,
    undo: VecDeque<EditSnapshot>,
    redo: VecDeque<EditSnapshot>,
}

impl Default for Composer {
    fn default() -> Self {
        Self {
            mode: ComposerMode::Prompt,
            buffer: String::new(),
            cursor: 0,
            selection_anchor: None,
            selected_command: 0,
            history_position: None,
            history_draft: String::new(),
            undo: VecDeque::new(),
            redo: VecDeque::new(),
        }
    }
}

impl Composer {
    pub(super) fn label(&self) -> &'static str {
        match self.mode {
            ComposerMode::Prompt => "Prompt >",
            ComposerMode::Shell => "Shell !",
            ComposerMode::Command => "Command /",
        }
    }

    pub(super) fn selection(&self) -> Option<std::ops::Range<usize>> {
        let anchor = self.selection_anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some(anchor.min(self.cursor)..anchor.max(self.cursor))
    }

    pub(super) fn insert_char(&mut self, character: char) {
        if self.mode == ComposerMode::Prompt && self.buffer.is_empty() && self.selection().is_none()
        {
            if character == '/' {
                self.record_edit();
                self.mode = ComposerMode::Command;
                return;
            }
            if character == '!' {
                self.record_edit();
                self.mode = ComposerMode::Shell;
                return;
            }
        } else if self.buffer.is_empty()
            && self.selection().is_none()
            && let (ComposerMode::Command, '/') = (self.mode, character)
        {
            self.record_edit();
            self.mode = ComposerMode::Prompt;
            self.buffer.push('/');
            self.cursor = 1;
            return;
        } else if self.buffer.is_empty()
            && self.selection().is_none()
            && let (ComposerMode::Shell, '!') = (self.mode, character)
        {
            self.record_edit();
            self.mode = ComposerMode::Prompt;
            self.buffer.push('!');
            self.cursor = 1;
            return;
        }
        let removed = self.selection().map_or(0, |range| range.len());
        if self
            .buffer
            .len()
            .saturating_sub(removed)
            .saturating_add(character.len_utf8())
            > MAX_COMPOSER_BYTES
            || (character == '\n'
                && self.buffer.bytes().filter(|byte| *byte == b'\n').count()
                    - self.selection().map_or(0, |range| {
                        self.buffer[range].bytes().filter(|b| *b == b'\n').count()
                    })
                    + 1
                    >= MAX_COMPOSER_LINES)
        {
            return;
        }
        self.record_edit();
        self.replace_selection("");
        self.buffer.insert(self.cursor, character);
        self.cursor += character.len_utf8();
        self.selected_command = 0;
        self.reset_history_navigation();
    }

    #[cfg(test)]
    pub(super) fn insert_text(&mut self, text: &str) {
        for character in text.chars() {
            self.insert_char(character);
        }
    }

    pub(super) fn insert_paste(&mut self, text: &str) {
        let range = self.selection();
        let removed = range.as_ref().map_or(0, |range| range.len());
        let available = MAX_COMPOSER_BYTES.saturating_sub(self.buffer.len() - removed);
        let removed_lines = range.as_ref().map_or(0, |range| {
            self.buffer[range.clone()]
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count()
        });
        let mut accepted = String::new();
        let mut line_count =
            self.buffer.bytes().filter(|byte| *byte == b'\n').count() + 1 - removed_lines;
        for character in text.chars() {
            if accepted.len().saturating_add(character.len_utf8()) > available {
                break;
            }
            if character == '\n' {
                if line_count >= MAX_COMPOSER_LINES {
                    break;
                }
                line_count += 1;
            }
            accepted.push(character);
        }
        if accepted.is_empty() && range.is_none() {
            return;
        }
        self.record_edit();
        self.replace_selection("");
        self.buffer.insert_str(self.cursor, &accepted);
        self.cursor += accepted.len();
        self.selected_command = 0;
        self.reset_history_navigation();
    }

    pub(super) fn move_left(&mut self, selecting: bool) {
        if !selecting && let Some(range) = self.selection() {
            self.cursor = range.start;
            self.selection_anchor = None;
            return;
        }
        self.prepare_selection(selecting);
        self.cursor = previous_grapheme_boundary(&self.buffer, self.cursor);
        self.finish_selection(selecting);
    }

    pub(super) fn move_right(&mut self, selecting: bool) {
        if !selecting && let Some(range) = self.selection() {
            self.cursor = range.end;
            self.selection_anchor = None;
            return;
        }
        self.prepare_selection(selecting);
        self.cursor = next_grapheme_boundary(&self.buffer, self.cursor);
        self.finish_selection(selecting);
    }

    pub(super) fn move_home(&mut self, selecting: bool) {
        self.prepare_selection(selecting);
        self.cursor = self.buffer[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        self.finish_selection(selecting);
    }

    pub(super) fn move_end(&mut self, selecting: bool) {
        self.prepare_selection(selecting);
        self.cursor = self.buffer[self.cursor..]
            .find('\n')
            .map_or(self.buffer.len(), |index| self.cursor + index);
        self.finish_selection(selecting);
    }

    pub(super) fn move_up(&mut self, selecting: bool) -> bool {
        let line_start = self.buffer[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        if line_start == 0 {
            return false;
        }
        let target_end = line_start - 1;
        let target_start = self.buffer[..target_end]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let column = display_width(&self.buffer[line_start..self.cursor]);
        self.prepare_selection(selecting);
        self.cursor = byte_at_display_column(&self.buffer, target_start, target_end, column);
        self.finish_selection(selecting);
        true
    }

    pub(super) fn move_down(&mut self, selecting: bool) -> bool {
        let line_start = self.buffer[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let Some(line_end_offset) = self.buffer[self.cursor..].find('\n') else {
            return false;
        };
        let next_start = self.cursor + line_end_offset + 1;
        let next_end = self.buffer[next_start..]
            .find('\n')
            .map_or(self.buffer.len(), |index| next_start + index);
        let column = display_width(&self.buffer[line_start..self.cursor]);
        self.prepare_selection(selecting);
        self.cursor = byte_at_display_column(&self.buffer, next_start, next_end, column);
        self.finish_selection(selecting);
        true
    }

    pub(super) fn move_word_left(&mut self, selecting: bool) {
        if !selecting && let Some(range) = self.selection() {
            self.cursor = range.start;
            self.selection_anchor = None;
            return;
        }
        self.prepare_selection(selecting);
        self.cursor = word_boundary_left(&self.buffer, self.cursor);
        self.finish_selection(selecting);
    }

    pub(super) fn move_word_right(&mut self, selecting: bool) {
        if !selecting && let Some(range) = self.selection() {
            self.cursor = range.end;
            self.selection_anchor = None;
            return;
        }
        self.prepare_selection(selecting);
        self.cursor = word_boundary_right(&self.buffer, self.cursor);
        self.finish_selection(selecting);
    }

    pub(super) fn select_all(&mut self) {
        self.selection_anchor = Some(0);
        self.cursor = self.buffer.len();
    }

    pub(super) fn backspace(&mut self) {
        if self.selection().is_some() {
            self.record_edit();
            self.replace_selection("");
            self.after_edit();
            return;
        }
        if self.cursor == 0 {
            if self.buffer.is_empty() && self.mode != ComposerMode::Prompt {
                self.reset();
            }
            return;
        }
        self.record_edit();
        let previous = previous_grapheme_boundary(&self.buffer, self.cursor);
        self.buffer.replace_range(previous..self.cursor, "");
        self.cursor = previous;
        self.after_edit();
    }

    pub(super) fn delete(&mut self) {
        if self.selection().is_some() {
            self.record_edit();
            self.replace_selection("");
            self.after_edit();
            return;
        }
        if self.cursor == self.buffer.len() {
            return;
        }
        self.record_edit();
        let next = next_grapheme_boundary(&self.buffer, self.cursor);
        self.buffer.replace_range(self.cursor..next, "");
        self.after_edit();
    }

    pub(super) fn delete_word_left(&mut self) {
        if self.selection().is_some() {
            self.backspace();
            return;
        }
        let start = word_boundary_left(&self.buffer, self.cursor);
        if start == self.cursor {
            return;
        }
        self.record_edit();
        self.buffer.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.after_edit();
    }

    pub(super) fn delete_word_right(&mut self) {
        if self.selection().is_some() {
            self.delete();
            return;
        }
        let end = word_boundary_right(&self.buffer, self.cursor);
        if end == self.cursor {
            return;
        }
        self.record_edit();
        self.buffer.replace_range(self.cursor..end, "");
        self.after_edit();
    }

    pub(super) fn undo(&mut self) {
        let Some(previous) = self.undo.pop_back() else {
            return;
        };
        let current = self.snapshot();
        push_bounded(&mut self.redo, current);
        self.restore(previous);
        self.reset_history_navigation();
    }

    pub(super) fn redo(&mut self) {
        let Some(next) = self.redo.pop_back() else {
            return;
        };
        let current = self.snapshot();
        push_bounded(&mut self.undo, current);
        self.restore(next);
        self.reset_history_navigation();
    }

    pub(super) fn reset(&mut self) {
        self.mode = ComposerMode::Prompt;
        self.buffer.clear();
        self.cursor = 0;
        self.selection_anchor = None;
        self.selected_command = 0;
        self.reset_history_navigation();
        self.undo.clear();
        self.redo.clear();
    }

    pub(super) fn history_previous(&mut self, entries: &[String]) {
        if self.mode != ComposerMode::Prompt || entries.is_empty() {
            return;
        }
        let position = match self.history_position {
            None => {
                self.history_draft = self.buffer.clone();
                entries.len() - 1
            }
            Some(position) => position.saturating_sub(1),
        };
        self.history_position = Some(position);
        self.buffer = entries[position].clone();
        self.cursor = self.buffer.len();
        self.selection_anchor = None;
    }

    pub(super) fn history_next(&mut self, entries: &[String]) {
        if self.mode != ComposerMode::Prompt {
            return;
        }
        let Some(position) = self.history_position else {
            return;
        };
        if position + 1 < entries.len() {
            self.history_position = Some(position + 1);
            self.buffer = entries[position + 1].clone();
        } else {
            self.history_position = None;
            self.buffer = std::mem::take(&mut self.history_draft);
        }
        self.cursor = self.buffer.len();
        self.selection_anchor = None;
    }

    pub(super) fn replace_buffer(&mut self, buffer: String) {
        self.record_edit();
        self.buffer = buffer;
        self.cursor = self.buffer.len();
        self.selection_anchor = None;
    }

    fn prepare_selection(&mut self, selecting: bool) {
        if selecting {
            self.selection_anchor.get_or_insert(self.cursor);
        } else if let Some(range) = self.selection() {
            self.cursor = range.start;
            self.selection_anchor = None;
        }
    }

    fn finish_selection(&mut self, selecting: bool) {
        if !selecting || self.selection_anchor == Some(self.cursor) {
            self.selection_anchor = None;
        }
    }

    fn replace_selection(&mut self, replacement: &str) {
        let Some(range) = self.selection() else {
            return;
        };
        self.cursor = range.start;
        self.buffer.replace_range(range, replacement);
        self.cursor += replacement.len();
        self.selection_anchor = None;
    }

    fn after_edit(&mut self) {
        self.selection_anchor = None;
        self.selected_command = 0;
        self.reset_history_navigation();
    }

    fn reset_history_navigation(&mut self) {
        self.history_position = None;
        self.history_draft.clear();
    }

    fn record_edit(&mut self) {
        let current = self.snapshot();
        push_bounded(&mut self.undo, current);
        self.redo.clear();
    }

    fn snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            mode: self.mode,
            buffer: self.buffer.clone(),
            cursor: self.cursor,
            selection_anchor: self.selection_anchor,
        }
    }

    fn restore(&mut self, snapshot: EditSnapshot) {
        self.mode = snapshot.mode;
        self.buffer = snapshot.buffer;
        self.cursor = snapshot.cursor;
        self.selection_anchor = snapshot.selection_anchor;
        self.selected_command = 0;
    }
}

fn push_bounded(queue: &mut VecDeque<EditSnapshot>, snapshot: EditSnapshot) {
    if queue.len() == MAX_UNDO_SNAPSHOTS {
        queue.pop_front();
    }
    queue.push_back(snapshot);
}

fn previous_grapheme_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .grapheme_indices(true)
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_grapheme_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .graphemes(true)
        .next()
        .map_or(cursor, |grapheme| cursor + grapheme.len())
}

fn word_boundary_left(text: &str, cursor: usize) -> usize {
    let mut boundary = cursor;
    let mut saw_word = false;
    for (index, grapheme) in text[..cursor].grapheme_indices(true).rev() {
        let word = grapheme.chars().any(char::is_alphanumeric) || grapheme == "_";
        if saw_word && !word {
            break;
        }
        if word {
            saw_word = true;
        }
        boundary = index;
    }
    boundary
}

fn word_boundary_right(text: &str, cursor: usize) -> usize {
    let mut boundary = cursor;
    let mut saw_word = false;
    for (offset, grapheme) in text[cursor..].grapheme_indices(true) {
        let word = grapheme.chars().any(char::is_alphanumeric) || grapheme == "_";
        if saw_word && !word {
            break;
        }
        if word {
            saw_word = true;
        }
        boundary = cursor + offset + grapheme.len();
    }
    boundary
}

fn display_width(text: &str) -> usize {
    let mut column = 0;
    for grapheme in text.graphemes(true) {
        column += if grapheme == "\t" {
            4 - column % 4
        } else {
            grapheme.width()
        };
    }
    column
}

fn byte_at_display_column(text: &str, start: usize, end: usize, target: usize) -> usize {
    let mut cursor = start;
    let mut column = 0;
    for (offset, grapheme) in text[start..end].grapheme_indices(true) {
        let width = if grapheme == "\t" {
            4 - column % 4
        } else {
            grapheme.width()
        };
        if column.saturating_add(width) > target {
            break;
        }
        column += width;
        cursor = start + offset + grapheme.len();
    }
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_replacement_and_undo_are_grapheme_safe() {
        let mut editor = Composer::default();
        editor.insert_paste("A👩‍💻e\u{301}界");
        editor.move_left(true);
        editor.move_left(true);
        assert_eq!(&editor.buffer[editor.selection().unwrap()], "e\u{301}界");
        editor.insert_char('λ');
        assert_eq!(editor.buffer, "A👩‍💻λ");
        editor.undo();
        assert_eq!(editor.buffer, "A👩‍💻e\u{301}界");
        editor.redo();
        assert_eq!(editor.buffer, "A👩‍💻λ");
    }

    #[test]
    fn word_actions_and_multiline_boundaries_are_deterministic() {
        let mut editor = Composer::default();
        editor.insert_paste("alpha  βeta\n界_word");
        editor.move_word_left(false);
        assert_eq!(&editor.buffer[editor.cursor..], "界_word");
        editor.delete_word_left();
        assert_eq!(editor.buffer, "alpha  界_word");
        editor.move_home(false);
        assert_eq!(editor.cursor, 0);
        editor.move_end(false);
        assert_eq!(editor.cursor, editor.buffer.len());
    }

    #[test]
    fn shell_and_command_history_are_never_navigated() {
        let entries = vec!["private prompt".to_owned()];
        let mut editor = Composer {
            mode: ComposerMode::Shell,
            ..Composer::default()
        };
        editor.history_previous(&entries);
        assert!(editor.buffer.is_empty());
        editor.mode = ComposerMode::Command;
        editor.history_previous(&entries);
        assert!(editor.buffer.is_empty());
    }

    #[test]
    fn paste_is_one_bounded_undo_transaction() {
        let mut editor = Composer::default();
        editor.insert_paste("line one\n👨‍👩‍👧‍👦\t界");
        assert_eq!(editor.buffer, "line one\n👨‍👩‍👧‍👦\t界");
        editor.undo();
        assert!(editor.buffer.is_empty());
        editor.redo();
        assert_eq!(editor.buffer, "line one\n👨‍👩‍👧‍👦\t界");
    }

    #[test]
    fn vertical_movement_preserves_visual_column_across_unicode_and_tabs() {
        let mut editor = Composer::default();
        editor.insert_paste("a\t界\n123456\n👩‍💻z");
        assert!(editor.move_up(false));
        assert_eq!(&editor.buffer[editor.cursor..], "456\n👩‍💻z");
        assert!(editor.move_up(true));
        assert!(editor.selection().is_some());
        assert!(editor.move_down(false));
        assert!(editor.selection().is_none());
    }
}
