use super::super::render::*;
use super::*;

#[derive(Debug)]
pub(crate) enum TtyInputEvent {
    Key(KeyEvent),
    Paste(String),
    Resize,
}

#[derive(Debug)]
pub(crate) enum TtyEditorAction {
    Continue,
    Submit(String),
    Eof,
    Interrupted,
}

pub(crate) struct TtyEditor {
    pub(crate) buffer: String,
    pub(crate) cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    completion: Option<(Vec<Suggestion>, usize)>,
    draft: Option<String>,
}

impl TtyEditor {
    pub(crate) fn new(history_path: &Path) -> Result<Self> {
        let history = if history_path.is_file() {
            std::fs::read_to_string(history_path)?
                .lines()
                .filter_map(|line| serde_json::from_str::<String>(line).ok())
                .filter(|line| !line.is_empty())
                .collect()
        } else {
            Vec::new()
        };
        Ok(Self {
            buffer: String::new(),
            cursor: 0,
            history,
            history_index: None,
            completion: None,
            draft: None,
        })
    }

    pub(crate) fn reset(&mut self) {
        self.buffer.clear();
        self.cursor = 0;
        self.history_index = None;
        self.completion = None;
        self.draft = None;
    }

    pub(crate) fn insert(&mut self, text: &str) {
        self.buffer.insert_str(self.cursor, text);
        self.cursor += text.len();
        if self.buffer.starts_with('/') {
            self.open_completion();
        } else {
            self.completion = None;
        }
    }

    pub(crate) fn insert_paste(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let safe = normalized
            .chars()
            .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
            .collect::<String>();
        self.insert(&safe);
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> TtyEditorAction {
        if matches!(key.kind, KeyEventKind::Release) {
            return TtyEditorAction::Continue;
        }
        if key.modifiers.contains(CtermKeyModifiers::CONTROL) {
            match key.code {
                CtermKeyCode::Char('c') => return TtyEditorAction::Interrupted,
                CtermKeyCode::Char('d') if self.buffer.is_empty() => return TtyEditorAction::Eof,
                CtermKeyCode::Char('a') => self.cursor = 0,
                CtermKeyCode::Char('e') => self.cursor = self.buffer.len(),
                CtermKeyCode::Char('u') => {
                    self.buffer.drain(..self.cursor);
                    self.cursor = 0;
                }
                CtermKeyCode::Char('w') => {
                    let before = self.buffer[..self.cursor].trim_end();
                    let start = before.rfind(char::is_whitespace).map_or(0, |i| i + 1);
                    self.buffer.drain(start..self.cursor);
                    self.cursor = start;
                }
                CtermKeyCode::Char('n') => self.next_history(),
                CtermKeyCode::Char('p') => self.previous_history(),
                _ => {}
            }
            return TtyEditorAction::Continue;
        }
        match key.code {
            CtermKeyCode::Char('/') if self.buffer.is_empty() => {
                self.insert("/");
                self.open_completion();
            }
            CtermKeyCode::Char(ch) => self.insert(&ch.to_string()),
            CtermKeyCode::Backspace => {
                if self.cursor > 0 {
                    let start = self.buffer[..self.cursor]
                        .char_indices()
                        .next_back()
                        .map_or(0, |(index, _)| index);
                    self.buffer.drain(start..self.cursor);
                    self.cursor = start;
                }
                self.completion = None;
            }
            CtermKeyCode::Delete => {
                if self.cursor < self.buffer.len() {
                    let end = self.buffer[self.cursor..]
                        .char_indices()
                        .nth(1)
                        .map_or(self.buffer.len(), |(index, _)| self.cursor + index);
                    self.buffer.drain(self.cursor..end);
                }
            }
            CtermKeyCode::Left => {
                self.cursor = self.buffer[..self.cursor]
                    .char_indices()
                    .next_back()
                    .map_or(0, |(index, _)| index);
            }
            CtermKeyCode::Right => {
                self.cursor = self.buffer[self.cursor..]
                    .char_indices()
                    .nth(1)
                    .map_or(self.buffer.len(), |(index, _)| self.cursor + index);
            }
            CtermKeyCode::Home => self.cursor = 0,
            CtermKeyCode::End => self.cursor = self.buffer.len(),
            CtermKeyCode::Up if self.completion.is_some() => self.cycle_completion(false),
            CtermKeyCode::Down if self.completion.is_some() => self.cycle_completion(true),
            CtermKeyCode::Up => self.previous_history(),
            CtermKeyCode::Down => self.next_history(),
            CtermKeyCode::Tab => self.complete(),
            CtermKeyCode::Enter => {
                if self.completion.is_some() {
                    self.accept_completion();
                    return TtyEditorAction::Continue;
                }
                let line = std::mem::take(&mut self.buffer);
                self.cursor = 0;
                self.history_index = None;
                self.completion = None;
                self.draft = None;
                return TtyEditorAction::Submit(line);
            }
            CtermKeyCode::Esc => self.completion = None,
            _ => {}
        }
        TtyEditorAction::Continue
    }

    pub(crate) fn previous_history(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_index.is_none() {
            self.draft = Some(self.buffer.clone());
        }
        let index = self
            .history_index
            .unwrap_or(self.history.len())
            .saturating_sub(1);
        self.history_index = Some(index);
        self.buffer = self.history[index].clone();
        self.cursor = self.buffer.len();
        self.completion = None;
    }

    pub(crate) fn next_history(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 >= self.history.len() {
            self.history_index = None;
            self.buffer = self.draft.take().unwrap_or_default();
            self.cursor = 0;
        } else {
            self.history_index = Some(index + 1);
            self.buffer = self.history[index + 1].clone();
            self.cursor = self.buffer.len();
        }
        self.completion = None;
    }

    pub(crate) fn complete(&mut self) {
        let mut completer = SlashCompleter;
        let suggestions = completer.complete(&self.buffer, self.cursor);
        if suggestions.is_empty() {
            return;
        }
        let next = self.completion.as_ref().map_or(0, |(_, index)| index + 1);
        let index = next % suggestions.len();
        let suggestion = &suggestions[index];
        self.buffer.replace_range(
            suggestion.span.start..suggestion.span.end,
            &suggestion.value,
        );
        self.cursor = suggestion.span.start + suggestion.value.len();
        self.completion = Some((suggestions, index));
    }

    pub(crate) fn open_completion(&mut self) {
        let mut completer = SlashCompleter;
        let suggestions = completer.complete(&self.buffer, self.cursor);
        if !suggestions.is_empty() {
            self.completion = Some((suggestions, 0));
        }
    }

    pub(crate) fn accept_completion(&mut self) {
        let Some((suggestions, index)) = self.completion.take() else {
            return;
        };
        let Some(suggestion) = suggestions.get(index) else {
            return;
        };
        self.buffer.replace_range(
            suggestion.span.start..suggestion.span.end,
            &suggestion.value,
        );
        self.cursor = suggestion.span.start + suggestion.value.len();
    }

    pub(crate) fn cycle_completion(&mut self, forward: bool) {
        let Some((suggestions, index)) = self.completion.as_mut() else {
            return;
        };
        if suggestions.is_empty() {
            return;
        }
        *index = if forward {
            (*index + 1) % suggestions.len()
        } else {
            (*index + suggestions.len() - 1) % suggestions.len()
        };
    }

    pub(crate) fn save_history(&mut self, path: &Path, line: &str) -> Result<()> {
        if line.is_empty() {
            return Ok(());
        }
        self.history.push(line.to_owned());
        if self.history.len() > 1000 {
            let excess = self.history.len() - 1000;
            self.history.drain(..excess);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let serialized = self
            .history
            .iter()
            .map(|entry| serde_json::to_string(entry).expect("history strings are serializable"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, serialized + "\n")?;
        Ok(())
    }
}

pub(crate) struct TtyRenderer {
    pub(crate) theme: TerminalTheme,
    pub(crate) activity: String,
    pub(crate) continuation: bool,
    pub(crate) editor: TtyEditor,
    function: String,
    cwd: String,
    initialized: bool,
    streaming: String,
}

impl TtyRenderer {
    pub(crate) fn new(
        theme: TerminalTheme,
        editor: TtyEditor,
        function: impl Into<String>,
        cwd: impl Into<String>,
    ) -> Self {
        Self {
            theme,
            activity: "Ready".to_owned(),
            continuation: false,
            editor,
            function: function.into(),
            cwd: cwd.into(),
            initialized: false,
            streaming: String::new(),
        }
    }

    pub(crate) fn repaint(&mut self) -> Result<()> {
        let (width, _) = terminal::size().unwrap_or((80, 24));
        let width = usize::from(width.max(1));
        let mut output = std::io::stderr();
        queue!(output, BeginSynchronizedUpdate)?;
        if self.initialized {
            queue!(
                output,
                cursor::MoveToColumn(0),
                cursor::MoveUp(2),
                Clear(ClearType::CurrentLine),
                Print(self.separator(width)),
                Print("\r\n"),
                Clear(ClearType::CurrentLine),
                Print(self.input_padding(width)),
                Print("\r\n"),
                Clear(ClearType::CurrentLine),
                Print(self.prompt_line(width)),
                Print("\r\n"),
                Clear(ClearType::CurrentLine),
                Print(self.input_padding(width)),
                Print("\r\n"),
                Clear(ClearType::CurrentLine),
                Print(self.status_row(width)),
                cursor::MoveUp(2),
                cursor::MoveToColumn(self.cursor_column(width))
            )?;
        } else {
            queue!(
                output,
                Print(self.separator(width)),
                Print("\r\n"),
                Print(self.input_padding(width)),
                Print("\r\n"),
                Print(self.prompt_line(width)),
                Print("\r\n"),
                Print(self.input_padding(width)),
                Print("\r\n"),
                Print(self.status_row(width)),
                cursor::MoveUp(2),
                cursor::MoveToColumn(self.cursor_column(width))
            )?;
            self.initialized = true;
        }
        queue!(output, EndSynchronizedUpdate)?;
        output.flush()?;
        Ok(())
    }

    pub(crate) fn input_padding(&self, width: usize) -> String {
        format!(
            "{}{}{}",
            self.theme.start("input_background"),
            " ".repeat(width),
            self.theme.reset()
        )
    }

    pub(crate) fn prompt_line(&self, width: usize) -> String {
        let bg = self.theme.start("input_background");
        let reset = self.theme.reset();
        let indicator = truncate_by_cells(if self.continuation { "… " } else { "› " }, width);
        let available = width.saturating_sub(UnicodeWidthStr::width(indicator.as_str()));
        let buffer = visible_tail(&self.editor.buffer, self.editor.cursor, available);
        let content = if buffer.text.is_empty() {
            self.theme
                .paint("input_hint", &truncate_by_cells(INPUT_HINT, available))
        } else {
            self.theme.paint("input_text", &buffer.text)
        };
        let content_width = terminal_visible_width(&content);
        format!(
            "{bg}{}{bg}{}{bg}{}{}",
            self.theme.paint("input_prompt", &indicator),
            content,
            " ".repeat(available.saturating_sub(content_width)),
            reset
        )
    }

    pub(crate) fn cursor_column(&self, width: usize) -> u16 {
        let indicator = truncate_by_cells(if self.continuation { "… " } else { "› " }, width);
        let available = width.saturating_sub(UnicodeWidthStr::width(indicator.as_str()));
        let buffer = visible_tail(&self.editor.buffer, self.editor.cursor, available);
        (UnicodeWidthStr::width(indicator.as_str()) + buffer.cursor_cells)
            .min(width.saturating_sub(1)) as u16
    }

    pub(crate) fn status_row(&self, width: usize) -> String {
        let left = format!("  {}", self.function);
        let right = format!("{}  ", self.cwd);
        let (left, right) = fit_status_values(&left, &right, width);
        let gap = width.saturating_sub(
            UnicodeWidthStr::width(left.as_str()) + UnicodeWidthStr::width(right.as_str()),
        );
        format!(
            "{}{}{}",
            self.theme.paint("muted", &left),
            " ".repeat(gap),
            self.theme.paint("muted", &right)
        )
    }

    pub(crate) fn separator(&self, width: usize) -> String {
        let activity = truncate_by_cells(&format!(" {} ", self.activity), width.saturating_sub(1));
        let used = UnicodeWidthStr::width(activity.as_str());
        format!(
            "{}{}{}",
            self.theme.paint("input_rule", "─"),
            self.theme.paint("input_activity", &activity),
            self.theme
                .paint("input_rule", &"─".repeat(width.saturating_sub(1 + used)))
        )
    }

    pub(crate) fn append_text(&mut self, text: &str) -> Result<()> {
        if !self.streaming.is_empty() {
            self.streaming.clear();
        }
        self.replace_input_with_transcript(text)
    }

    pub(crate) fn append_stream(&mut self, delta: &str) -> Result<()> {
        let first = self.streaming.is_empty();
        self.streaming.push_str(delta);
        let text = if first {
            format!("{}\r\n{}", answer_rule(&self.theme), delta)
        } else {
            delta.to_owned()
        };
        self.replace_input_with_transcript(&text)
    }

    pub(crate) fn finalize_answer(&mut self, text: &str, elapsed: Duration) -> Result<()> {
        if self.streaming.is_empty() && !text.is_empty() {
            return self.append_text(&format!(
                "\n{}\n{}\n{}\n",
                answer_rule(&self.theme),
                markdown_to_terminal(text, &self.theme),
                answer_footer(&self.theme, elapsed)
            ));
        }
        // The model deltas already rendered the answer body. Only commit the
        // footer here; reprinting `text` would duplicate the streamed answer.
        let _ = text;
        self.streaming.clear();
        self.replace_input_with_transcript(&format!(
            "\r\n{}\r\n",
            answer_footer(&self.theme, elapsed)
        ))
    }

    pub(crate) fn append_submitted(&mut self, text: &str) -> Result<()> {
        let width = usize::from(terminal::size().unwrap_or((80, 24)).0.max(1));
        self.replace_input_with_transcript(&submitted_message_frame(&self.theme, text, width))
    }

    pub(crate) fn replace_input_with_transcript(&mut self, text: &str) -> Result<()> {
        let width = usize::from(terminal::size().unwrap_or((80, 24)).0.max(1));
        let mut output = std::io::stderr();
        queue!(output, BeginSynchronizedUpdate, cursor::MoveToColumn(0))?;
        if self.initialized {
            queue!(
                output,
                cursor::MoveUp(2),
                Clear(ClearType::CurrentLine),
                cursor::MoveDown(1),
                Clear(ClearType::CurrentLine),
                cursor::MoveDown(1),
                Clear(ClearType::CurrentLine),
                cursor::MoveDown(1),
                Clear(ClearType::CurrentLine),
                cursor::MoveDown(1),
                Clear(ClearType::CurrentLine),
                cursor::MoveUp(4),
                cursor::MoveToColumn(0)
            )?;
        }
        let mut transcript = wrap_external_text(text, width);
        if !transcript.ends_with('\n') {
            transcript.push('\n');
        }
        queue!(
            output,
            Print(terminal_crlf(&transcript)),
            Print(self.separator(width)),
            Print("\r\n"),
            Print(self.input_padding(width)),
            Print("\r\n"),
            Print(self.prompt_line(width)),
            Print("\r\n"),
            Print(self.input_padding(width)),
            Print("\r\n"),
            Print(self.status_row(width)),
            cursor::MoveUp(2),
            cursor::MoveToColumn(self.cursor_column(width)),
            EndSynchronizedUpdate
        )?;
        self.initialized = true;
        output.flush()?;
        Ok(())
    }
}

pub(crate) struct VisibleTail {
    pub(crate) text: String,
    pub(crate) cursor_cells: usize,
}

pub(crate) fn visible_tail(text: &str, cursor: usize, width: usize) -> VisibleTail {
    let cursor = cursor.min(text.len());
    let line_start = text[..cursor].rfind('\n').map_or(0, |index| index + 1);
    let line_end = text[cursor..]
        .find('\n')
        .map_or(text.len(), |index| cursor + index);
    let line = text[line_start..line_end].replace('\t', "    ");
    let line_cursor = text[line_start..cursor].replace('\t', "    ").len();
    visible_single_line_tail(&line, line_cursor, width)
}

fn visible_single_line_tail(text: &str, cursor: usize, width: usize) -> VisibleTail {
    let before = &text[..cursor];
    let mut start = 0;
    let mut cells = 0;
    for (index, ch) in before.char_indices().rev() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cells + w > width {
            break;
        }
        cells += w;
        start = index;
    }
    let mut visible = String::new();
    let mut used = 0;
    for ch in text[start..].chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            break;
        }
        visible.push(ch);
        used += w;
    }
    VisibleTail {
        text: visible,
        cursor_cells: UnicodeWidthStr::width(&text[start..cursor]),
    }
}

fn truncate_by_cells(text: &str, width: usize) -> String {
    let mut cells = 0;
    text.chars()
        .take_while(|ch| {
            let next = cells + UnicodeWidthChar::width(*ch).unwrap_or(0);
            if next > width {
                false
            } else {
                cells = next;
                true
            }
        })
        .collect()
}

fn truncate_from_end_by_cells(text: &str, width: usize) -> String {
    let mut used = 0;
    let mut start = text.len();
    for (index, ch) in text.char_indices().rev() {
        let next = used + UnicodeWidthChar::width(ch).unwrap_or(0);
        if next > width {
            break;
        }
        used = next;
        start = index;
    }
    text[start..].to_owned()
}

fn fit_status_values(left: &str, right: &str, width: usize) -> (String, String) {
    let left_width = UnicodeWidthStr::width(left);
    let right_width = UnicodeWidthStr::width(right);
    if left_width + right_width <= width {
        return (left.to_owned(), right.to_owned());
    }
    let left_budget = width / 2;
    let right_budget = width.saturating_sub(left_budget);
    (
        truncate_by_cells(left, left_budget),
        truncate_from_end_by_cells(right, right_budget),
    )
}

fn wrapped_input_lines(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            rows.push(String::new());
            continue;
        }
        let mut row = String::new();
        let mut used = 0;
        for ch in line.chars() {
            let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if char_width > width {
                if !row.is_empty() {
                    rows.push(std::mem::take(&mut row));
                }
                row.push('…');
                used = 1;
                continue;
            }
            if !row.is_empty() && used + char_width > width {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push(ch);
            used += char_width;
        }
        rows.push(row);
    }
    rows
}

pub(crate) fn submitted_message_frame(theme: &TerminalTheme, text: &str, width: usize) -> String {
    let width = width.max(1);
    let text = safe_terminal_text(text);
    let marker = truncate_by_cells("› ", width);
    let marker_width = UnicodeWidthStr::width(marker.as_str());
    if marker_width >= width {
        let background = theme.start("input_background");
        let reset = theme.reset();
        let mut rows = vec![format!(
            "{background}{}{reset}",
            theme.paint("input_prompt", &marker)
        )];
        rows.extend(wrapped_input_lines(&text, width).into_iter().map(|row| {
            format!(
                "{background}{}{background}{}{reset}",
                theme.paint("input_text", &row),
                " ".repeat(width.saturating_sub(UnicodeWidthStr::width(row.as_str())))
            )
        }));
        return rows.join("\r\n");
    }
    let continuation = " ".repeat(marker_width);
    let rows = wrapped_input_lines(&text, width.saturating_sub(marker_width));
    let background = theme.start("input_background");
    let reset = theme.reset();
    rows.into_iter()
        .enumerate()
        .map(|(index, row)| {
            let prefix = if index == 0 {
                marker.as_str()
            } else {
                &continuation
            };
            let prefix_width = UnicodeWidthStr::width(prefix);
            let row_width = prefix_width + UnicodeWidthStr::width(row.as_str());
            format!(
                "{background}{}{background}{}{background}{}{reset}",
                theme.paint(
                    "input_prompt",
                    if index == 0 { prefix } else { &continuation }
                ),
                theme.paint("input_text", &row),
                " ".repeat(width.saturating_sub(row_width))
            )
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}
