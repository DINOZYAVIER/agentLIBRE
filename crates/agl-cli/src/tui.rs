use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal as _};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agl_client::{
    AgentLibreClient, ClientError, ExecutionAttachment, ExecutionAttachmentEvent,
    PresentationSubscription, PresentationSubscriptionEvent, RunSubscriptionEvent,
};
use agl_ids::{ExecutionId, MessageId, RunId, SessionId, TerminalSessionId};
use agl_protocol::{
    ActiveRunView, ApplicationAction, ApplicationActionRequest, ApplicationActionResult,
    ClientEffectKind, CommandAvailability, CommandCatalogRequest, CommandDescriptor,
    CommandSuggestion, CommandSuggestionsRequest, ExecutionProfile, ExecutionStatusRequest,
    ExecutionView, HostStartupPolicy, HumanHostTerminalEnsureRequest, HumanTerminalEnsureRequest,
    HumanTerminalEnsuredEvent, KillMode, ProcessBytes, ProtocolRunState, ProtocolToolMode,
    RunBudgetRequest, RunSubmitRequest, RunSubscribeRequest, SessionLaunchOptions,
    SessionPresentationItem, SessionPresentationRequest, SessionPresentationSnapshot,
    SessionPresentationSubscribeRequest, SessionSelector, StructuredEnvironmentOverlay,
    TerminalOwnerView, TerminalPromptState, TerminalSessionView, TerminalSize,
};
use agl_runtime::AgentLibreRuntimeConfig;
use anyhow::{Context as _, Result, bail};
use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use fs2::FileExt as _;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{TerminalOptions, Viewport};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::sync::mpsc;
use unicode_segmentation::UnicodeSegmentation as _;

use self::terminal_filter::TerminalOutputFilter;
use self::terminal_input::{RawTerminalInputGate, TerminalInputAction};
#[cfg(target_os = "linux")]
use self::terminal_view::RawTtyInput;

use crate::args::InteractiveOptions;

pub(crate) mod terminal_filter;
mod terminal_input;
mod terminal_view;

const UI_EVENT_CAPACITY: usize = 256;
const MAX_COMPOSER_BYTES: usize = 64 * 1024;
const MAX_COMPOSER_LINES: usize = 2_000;
const MAX_HISTORY_ENTRIES: usize = 1_000;
const MAX_HISTORY_BYTES: usize = 1024 * 1024;
const MAX_LIVE_ASSISTANT_DELTAS: usize = 8;
const MAX_LIVE_ASSISTANT_DELTA_BYTES: usize = 1024 * 1024;
const MAX_PICKER_ENTRIES: usize = 256;
const MAX_PICKER_PAGES: usize = 8;
const CHAT_FRAME_INTERVAL: Duration = Duration::from_micros(16_667);
const CHAT_INPUT_POLL_INTERVAL: Duration = Duration::from_millis(20);

struct ChatInput {
    receiver: mpsc::UnboundedReceiver<io::Result<Event>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ChatInput {
    fn new() -> io::Result<Self> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::Builder::new()
            .name("agl-chat-input".to_owned())
            .spawn(move || {
                while !thread_shutdown.load(Ordering::Acquire) {
                    match crossterm::event::poll(CHAT_INPUT_POLL_INTERVAL) {
                        Ok(true) => match crossterm::event::read() {
                            Ok(event) => {
                                if sender.send(Ok(event)).is_err() {
                                    break;
                                }
                            }
                            Err(error) => {
                                let _ = sender.send(Err(error));
                                break;
                            }
                        },
                        Ok(false) => {}
                        Err(error) => {
                            let _ = sender.send(Err(error));
                            break;
                        }
                    }
                }
            })?;
        Ok(Self {
            receiver,
            shutdown,
            thread: Some(thread),
        })
    }

    async fn next(&mut self) -> Option<io::Result<Event>> {
        self.receiver.recv().await
    }
}

impl Drop for ChatInput {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ComposerMode {
    Prompt,
    Command,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ComposerSubmission {
    Prompt(String),
    SwitchTerminal,
    Command(String),
    Picker(PickerSubmit),
}

#[derive(Debug)]
struct Composer {
    mode: ComposerMode,
    buffer: String,
    cursor: usize,
    selected_command: usize,
    history_position: Option<usize>,
    history_draft: String,
    terminal_switch_eligible: bool,
}

impl Default for Composer {
    fn default() -> Self {
        Self {
            mode: ComposerMode::Prompt,
            buffer: String::new(),
            cursor: 0,
            selected_command: 0,
            history_position: None,
            history_draft: String::new(),
            terminal_switch_eligible: false,
        }
    }
}

impl Composer {
    fn label(&self) -> &'static str {
        match self.mode {
            ComposerMode::Prompt => "Prompt >",
            ComposerMode::Command => "Command /",
        }
    }

    fn insert_char(&mut self, character: char) {
        self.history_position = None;
        self.history_draft.clear();
        if self.buffer.len().saturating_add(character.len_utf8()) > MAX_COMPOSER_BYTES
            || (character == '\n'
                && self.buffer.bytes().filter(|byte| *byte == b'\n').count() + 1
                    >= MAX_COMPOSER_LINES)
        {
            return;
        }
        let terminal_switch_eligible = self.mode == ComposerMode::Prompt
            && self.buffer.is_empty()
            && self.cursor == 0
            && character == '!';
        if self.mode == ComposerMode::Prompt && self.buffer.is_empty() {
            if character == '/' {
                self.mode = ComposerMode::Command;
                self.terminal_switch_eligible = false;
                return;
            }
        } else if self.buffer.is_empty()
            && let (ComposerMode::Command, '/') = (self.mode, character)
        {
            self.mode = ComposerMode::Prompt;
            self.buffer.push('/');
            self.cursor = 1;
            return;
        }
        self.buffer.insert(self.cursor, character);
        self.cursor += character.len_utf8();
        self.terminal_switch_eligible = terminal_switch_eligible;
        self.selected_command = 0;
    }

    #[cfg(test)]
    fn insert_text(&mut self, text: &str) {
        for character in text.chars() {
            self.insert_char(character);
        }
    }

    fn insert_paste(&mut self, text: &str) {
        self.history_position = None;
        self.history_draft.clear();
        self.terminal_switch_eligible = false;
        let available = MAX_COMPOSER_BYTES.saturating_sub(self.buffer.len());
        let mut accepted = String::new();
        let mut line_count = self.buffer.bytes().filter(|byte| *byte == b'\n').count() + 1;
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
        self.buffer.insert_str(self.cursor, &accepted);
        self.cursor += accepted.len();
        self.selected_command = 0;
    }

    fn move_left(&mut self) {
        self.cursor = self.buffer[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(index, _)| index);
    }

    fn move_right(&mut self) {
        let suffix = &self.buffer[self.cursor..];
        self.cursor += suffix.graphemes(true).next().map_or(0, str::len);
    }

    fn backspace(&mut self) {
        self.history_position = None;
        self.history_draft.clear();
        self.terminal_switch_eligible = false;
        if self.cursor == 0 {
            if self.buffer.is_empty() && self.mode != ComposerMode::Prompt {
                self.reset();
            }
            return;
        }
        let previous = self.buffer[..self.cursor]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(index, _)| index);
        self.buffer.replace_range(previous..self.cursor, "");
        self.cursor = previous;
        self.selected_command = 0;
    }

    fn delete(&mut self) {
        self.history_position = None;
        self.history_draft.clear();
        self.terminal_switch_eligible = false;
        if self.cursor == self.buffer.len() {
            return;
        }
        let length = self.buffer[self.cursor..]
            .graphemes(true)
            .next()
            .map_or(0, str::len);
        self.buffer
            .replace_range(self.cursor..self.cursor + length, "");
    }

    fn submit(&mut self) -> Option<ComposerSubmission> {
        let text = self.buffer.trim_end_matches(['\r', '\n']).to_owned();
        if text.trim().is_empty() {
            return None;
        }
        let submission = match self.mode {
            ComposerMode::Prompt if text == "!" && self.terminal_switch_eligible => {
                ComposerSubmission::SwitchTerminal
            }
            ComposerMode::Prompt => ComposerSubmission::Prompt(text),
            ComposerMode::Command => ComposerSubmission::Command(text),
        };
        self.reset();
        Some(submission)
    }

    fn reset(&mut self) {
        self.mode = ComposerMode::Prompt;
        self.buffer.clear();
        self.cursor = 0;
        self.selected_command = 0;
        self.history_position = None;
        self.history_draft.clear();
        self.terminal_switch_eligible = false;
    }

    fn history_previous(&mut self, entries: &[String]) {
        if entries.is_empty() {
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
        self.terminal_switch_eligible = false;
        self.buffer = entries[position].clone();
        self.cursor = self.buffer.len();
    }

    fn history_next(&mut self, entries: &[String]) {
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
        self.terminal_switch_eligible = false;
        self.cursor = self.buffer.len();
    }
}

#[derive(Clone, Copy)]
enum HistoryMode {
    Prompt,
}

impl HistoryMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryRecord {
    schema: String,
    timestamp_unix_ms: u128,
    mode: String,
    input: String,
}

struct InputHistory {
    root: Option<PathBuf>,
    prompt: Vec<String>,
}

impl InputHistory {
    fn load(state_dir: &Path, workspace: &str, enabled: bool) -> (Self, Vec<String>) {
        if !enabled {
            return (
                Self {
                    root: None,
                    prompt: Vec::new(),
                },
                Vec::new(),
            );
        }
        let digest = Sha256::digest(workspace.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let root = state_dir.join("cli/history").join(digest);
        let mut warnings = Vec::new();
        if let Err(error) = create_private_directory(&root) {
            warnings.push(format!("input history disabled: {error:#}"));
            return (
                Self {
                    root: None,
                    prompt: Vec::new(),
                },
                warnings,
            );
        }
        let prompt = read_history_file(
            &root.join("prompt.jsonl"),
            HistoryMode::Prompt,
            &mut warnings,
        );
        (
            Self {
                root: Some(root),
                prompt,
            },
            warnings,
        )
    }

    fn entries(&self, mode: ComposerMode) -> &[String] {
        match mode {
            ComposerMode::Prompt => &self.prompt,
            ComposerMode::Command => &[],
        }
    }

    fn record(&mut self, mode: HistoryMode, input: &str) -> Result<()> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        let entries = match mode {
            HistoryMode::Prompt => &mut self.prompt,
        };
        if entries.last().is_some_and(|last| last == input) {
            return Ok(());
        }
        entries.push(input.to_owned());
        if entries.len() > MAX_HISTORY_ENTRIES {
            entries.drain(..entries.len() - MAX_HISTORY_ENTRIES);
        }
        let path = root.join(format!("{}.jsonl", mode.as_str()));
        let lock_path = root.join(format!("{}.lock", mode.as_str()));
        let lock = open_private_file(&lock_path, false)?;
        lock.lock_exclusive()
            .context("failed to lock input history")?;
        let record = HistoryRecord {
            schema: "agentlibre.cli.input_history.v1".to_owned(),
            timestamp_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            mode: mode.as_str().to_owned(),
            input: input.to_owned(),
        };
        let line = serde_json::to_vec(&record).context("failed to encode input history")?;
        let mut file = open_private_file(&path, true)?;
        file.write_all(&line)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        let metadata = file.metadata()?;
        drop(file);
        if metadata.len() as usize > MAX_HISTORY_BYTES {
            compact_history(&path, entries, mode)?;
        }
        fs2::FileExt::unlock(&lock).context("failed to unlock input history")?;
        Ok(())
    }
}

fn create_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("history path is not a private directory")
    }
    #[cfg(unix)]
    fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    Ok(())
}

fn open_private_file(path: &Path, append: bool) -> Result<std::fs::File> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        bail!("history target is not a regular file: {}", path.display());
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).append(append);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(file)
}

fn read_history_file(path: &Path, mode: HistoryMode, warnings: &mut Vec<String>) -> Vec<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            warnings.push(format!("history read failed: {error}"));
            return Vec::new();
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) if metadata.is_file() && metadata.len() <= (MAX_HISTORY_BYTES * 2) as u64 => {
            metadata
        }
        Ok(_) => {
            warnings.push(format!("history file is oversized: {}", path.display()));
            return Vec::new();
        }
        Err(error) => {
            warnings.push(format!("history metadata failed: {error}"));
            return Vec::new();
        }
    };
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    if let Err(error) = file.read_to_end(&mut bytes) {
        warnings.push(format!("history read failed: {error}"));
        return Vec::new();
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            warnings.push(format!("history is not UTF-8: {}", path.display()));
            return Vec::new();
        }
    };
    let mut entries = Vec::new();
    for line in text.lines() {
        if line.len() > MAX_COMPOSER_BYTES {
            warnings.push("oversized input history record skipped".to_owned());
            continue;
        }
        let Ok(record) = serde_json::from_str::<HistoryRecord>(line) else {
            warnings.push("corrupt input history record skipped".to_owned());
            continue;
        };
        if record.schema == "agentlibre.cli.input_history.v1"
            && record.mode == mode.as_str()
            && record.input.len() <= MAX_COMPOSER_BYTES
            && entries.last() != Some(&record.input)
        {
            entries.push(record.input);
        }
    }
    if entries.len() > MAX_HISTORY_ENTRIES {
        entries.drain(..entries.len() - MAX_HISTORY_ENTRIES);
    }
    entries
}

fn compact_history(path: &Path, entries: &[String], mode: HistoryMode) -> Result<()> {
    let temporary = path.with_extension(format!("jsonl.tmp-{}", std::process::id()));
    let mut file = open_private_file(&temporary, false)?;
    file.set_len(0)?;
    for input in entries {
        let record = HistoryRecord {
            schema: "agentlibre.cli.input_history.v1".to_owned(),
            timestamp_unix_ms: 0,
            mode: mode.as_str().to_owned(),
            input: input.clone(),
        };
        serde_json::to_writer(&mut file, &record)?;
        file.write_all(b"\n")?;
    }
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PickerKind {
    Resume,
    Model,
    Mode,
    Skills,
    Processes,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProcessPickerItem {
    execution_id: ExecutionId,
    state: agl_protocol::ExecutionState,
    profile: ExecutionProfile,
    cwd: String,
    terminal: Option<TerminalSessionView>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PickerPayload {
    Resume(SessionId),
    Model(String),
    Mode(ProtocolToolMode),
    Skill(String),
    EnsureHost(HostStartupPolicy),
    Process(Box<ProcessPickerItem>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PickerEntry {
    value: String,
    label: String,
    detail: Option<String>,
    payload: PickerPayload,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PickerSubmit {
    Resume(SessionId),
    Model(String),
    Mode(ProtocolToolMode),
    Skills(Vec<String>),
    EnsureHost {
        startup: HostStartupPolicy,
    },
    Attach {
        terminal: Box<TerminalSessionView>,
        writable: bool,
    },
    Kill {
        execution_id: ExecutionId,
        mode: KillMode,
    },
    Promote {
        terminal_id: TerminalSessionId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PickerConfirmation {
    prompt: String,
    submit: PickerSubmit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PickerState {
    kind: PickerKind,
    title: String,
    entries: Vec<PickerEntry>,
    query: String,
    selected: usize,
    selected_values: BTreeSet<String>,
    confirmation: Option<PickerConfirmation>,
}

impl PickerState {
    fn new(kind: PickerKind, title: impl Into<String>, entries: Vec<PickerEntry>) -> Self {
        Self {
            kind,
            title: title.into(),
            entries,
            query: String::new(),
            selected: 0,
            selected_values: BTreeSet::new(),
            confirmation: None,
        }
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let query = self.query.to_ascii_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                query.is_empty()
                    || entry.value.to_ascii_lowercase().contains(&query)
                    || entry.label.to_ascii_lowercase().contains(&query)
                    || entry
                        .detail
                        .as_deref()
                        .is_some_and(|detail| detail.to_ascii_lowercase().contains(&query))
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn selected_entry(&self) -> Option<&PickerEntry> {
        let indices = self.filtered_indices();
        indices
            .get(self.selected.min(indices.len().saturating_sub(1)))
            .and_then(|index| self.entries.get(*index))
    }

    fn move_selection(&mut self, delta: isize) {
        let length = self.filtered_indices().len();
        if length == 0 {
            self.selected = 0;
            return;
        }
        self.selected = if delta.is_negative() {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            self.selected.saturating_add(delta as usize).min(length - 1)
        };
    }

    fn select_value(&mut self, value: &str) {
        if let Some(index) = self.entries.iter().position(|entry| entry.value == value) {
            self.selected = index;
        }
    }

    fn push_query(&mut self, character: char) {
        if !character.is_control() && self.query.len().saturating_add(character.len_utf8()) <= 512 {
            self.query.push(character);
            self.selected = 0;
        }
    }

    fn pop_query(&mut self) {
        self.query.pop();
        self.selected = 0;
    }

    fn toggle_selected_skill(&mut self) {
        let Some(PickerEntry {
            payload: PickerPayload::Skill(skill_id),
            ..
        }) = self.selected_entry()
        else {
            return;
        };
        let skill_id = skill_id.clone();
        if !self.selected_values.remove(&skill_id) {
            self.selected_values.insert(skill_id);
        }
    }

    fn select_all_skills(&mut self) {
        self.selected_values = self
            .entries
            .iter()
            .filter_map(|entry| match &entry.payload {
                PickerPayload::Skill(skill_id) => Some(skill_id.clone()),
                _ => None,
            })
            .collect();
    }

    fn clear_skills(&mut self) {
        self.selected_values.clear();
    }
}

struct UiState {
    snapshot: SessionPresentationSnapshot,
    catalog: Vec<CommandDescriptor>,
    composer: Composer,
    last_terminal: Option<TerminalSessionId>,
    terminal_cursors: BTreeMap<ExecutionId, u64>,
    seen_terminals: BTreeSet<TerminalSessionId>,
    assistant_deltas: BTreeMap<MessageId, AssistantDeltaState>,
    picker: Option<PickerState>,
    notices: Vec<String>,
    active_run: Option<agl_ids::RunId>,
    exit_armed: bool,
    workspace_change_armed: Option<String>,
    shell_profile_id: Option<String>,
    history: InputHistory,
}

struct AssistantDeltaState {
    run_id: RunId,
    next_sequence: u64,
    text: String,
    valid: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AssistantDeltaApply {
    Applied,
    Duplicate,
    SequenceGap,
    BoundExceeded,
}

impl UiState {
    fn matching_commands(&self) -> Vec<&CommandDescriptor> {
        let query = self
            .composer
            .buffer
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        self.catalog
            .iter()
            .filter(|command| {
                !matches!(command.availability, CommandAvailability::Hidden)
                    && (query.is_empty()
                        || command.name.starts_with(&query)
                        || command
                            .aliases
                            .iter()
                            .any(|alias| alias.starts_with(&query)))
            })
            .take(8)
            .collect()
    }

    fn notice(&mut self, message: impl Into<String>) {
        self.notices.push(message.into());
        if self.notices.len() > 6 {
            self.notices.remove(0);
        }
    }
}

enum UiAsyncEvent {
    RunAccepted {
        session_id: SessionId,
        run_id: agl_ids::RunId,
        state: ProtocolRunState,
    },
    Snapshot {
        session_id: SessionId,
        snapshot: Box<SessionPresentationSnapshot>,
    },
    Notice(String),
}

pub(crate) fn run_interactive(
    options: InteractiveOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    bail!("interactive Chat/Terminal UI is currently supported only on Linux");
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("interactive UI requires terminal stdin and stdout");
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build interactive CLI runtime")?
        .block_on(run_interactive_async(options, runtime))
}

async fn run_interactive_async(
    options: InteractiveOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let socket_path = options
        .socket_path
        .clone()
        .unwrap_or_else(|| agl_daemon::default_socket_path(&runtime.paths));
    let client = AgentLibreClient::connect(&socket_path)
        .await
        .map_err(|error| interactive_connect_error(&socket_path, error))?;
    let mut session_id = resolve_session(&client, &options).await?;
    let mut presentation = client
        .subscribe_presentation(SessionPresentationSubscribeRequest {
            session_id: session_id.clone(),
        })
        .await
        .context("failed to subscribe to the session presentation")?;
    let catalog = client
        .command_catalog(CommandCatalogRequest {
            session_id: Some(session_id.clone()),
            client_effects: vec![
                ClientEffectKind::Help,
                ClientEffectKind::Disconnect,
                ClientEffectKind::InputHistory,
                ClientEffectKind::RawExecutionAttach,
            ],
        })
        .await
        .context("failed to load the command catalog")?
        .descriptors;
    let (history, history_warnings) = InputHistory::load(
        &runtime.paths.state_dir,
        &presentation.snapshot.header.workspace_root,
        options.input_history,
    );
    let mut notices = vec![
        "Type exact ! then Enter for Terminal, / for commands, Ctrl+D to disconnect".to_owned(),
    ];
    notices.extend(history_warnings);
    let seen_terminals = presentation
        .snapshot
        .terminals
        .iter()
        .map(|terminal| terminal.terminal_id.clone())
        .collect();
    let mut state = UiState {
        snapshot: presentation.snapshot.clone(),
        catalog,
        composer: Composer::default(),
        last_terminal: presentation
            .snapshot
            .terminals
            .first()
            .map(|terminal| terminal.terminal_id.clone()),
        terminal_cursors: BTreeMap::new(),
        seen_terminals,
        assistant_deltas: BTreeMap::new(),
        picker: None,
        notices,
        active_run: None,
        exit_armed: false,
        workspace_change_armed: None,
        shell_profile_id: managed_shell_profile_id(&runtime.execution.shell.program)
            .map(str::to_owned),
        history,
    };
    let (async_sender, mut async_events) = mpsc::channel(UI_EVENT_CAPACITY);
    let mut pending_terminal: Option<Box<TerminalViewRequest>> = None;
    let mut terminal_stream = None;
    let mut interrupt_signal =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .context("failed to install SIGINT handling")?;
    let mut terminate_signal =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("failed to install SIGTERM handling")?;
    let mut suspend_signal =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(libc::SIGTSTP))
            .context("failed to install SIGTSTP handling")?;
    let mut resize_signal =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
            .context("failed to install SIGWINCH handling")?;
    let mut terminal_mode = TuiTerminalMode::enter()?;
    let mut render_tick = tokio::time::interval(CHAT_FRAME_INTERVAL);
    render_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let height = crossterm::terminal::size()
        .map(|(_, rows)| rows.max(8))
        .unwrap_or(24);
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height),
        },
    )
    .context("failed to initialize terminal UI")?;
    let mut input = Some(ChatInput::new().context("failed to start Chat input reader")?);

    let result = loop {
        if let Some(terminal_request) = pending_terminal.take() {
            drop(input.take());
            match run_terminal_passthrough(
                &client,
                *terminal_request,
                &mut state,
                &mut presentation,
                &mut async_events,
                (
                    &mut terminal,
                    &mut interrupt_signal,
                    &mut terminate_signal,
                    &mut suspend_signal,
                    &mut resize_signal,
                ),
                &mut terminal_stream,
            )
            .await
            {
                Ok(TerminalPassthroughOutcome::Chat) => {}
                Ok(TerminalPassthroughOutcome::Disconnect) => break Ok(()),
                Err(error) => state.notice(format!("Terminal view ended: {error:#}")),
            }
            if let Some(stream) = terminal_stream.as_mut() {
                stream.filter.set_visible(false);
            }
            // Reconcile the inline viewport before restarting Crossterm's
            // asynchronous reader. On Unix, inline resize asks the terminal
            // for its cursor position through the same global reader.
            terminal
                .autoresize()
                .context("failed to resize restored Chat view")?;
            terminal.clear().context("failed to restore Chat view")?;
            terminal
                .draw(|frame| draw(frame, &state))
                .context("failed to redraw restored Chat view")?;
            input = Some(ChatInput::new().context("failed to restart Chat input reader")?);
        }
        tokio::select! {
            _ = render_tick.tick() => {
                terminal
                    .draw(|frame| draw(frame, &state))
                    .context("failed to render terminal UI")?;
            }
            event = input.as_mut().expect("Chat input stream is installed").next() => {
                let Some(event) = event else { break Ok(()); };
                match event.context("failed to read terminal input")? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if let Some(control) = handle_key(&mut state, key) {
                            match control {
                                UiControl::Disconnect => break Ok(()),
                                UiControl::CancelRun(run_id) => {
                                    match client.cancel_run(run_id).await {
                                        Ok(_) => state.notice("active turn cancellation requested"),
                                        Err(error) => state.notice(format!("cancel failed: {error}")),
                                    }
                                }
                                UiControl::Notice(message) => state.notice(message),
                                UiControl::Submission(submission) => {
                                    match handle_submission(
                                        &client,
                                        &session_id,
                                        &mut state,
                                        submission,
                                        &async_sender,
                                    ).await? {
                                        SubmissionOutcome::Continue => {}
                                        SubmissionOutcome::Disconnect => break Ok(()),
                                        SubmissionOutcome::EnterTerminal(request) => {
                                            pending_terminal = Some(request);
                                        }
                                        SubmissionOutcome::SwitchSession { session_id: next_session_id } => {
                                            match prepare_session_switch(
                                                &client,
                                                next_session_id,
                                                &runtime.paths.state_dir,
                                                options.input_history,
                                            )
                                            .await
                                            {
                                                Ok(next) => {
                                                    let PreparedSessionSwitch {
                                                        session_id: next_session_id,
                                                        presentation: next_presentation,
                                                        snapshot,
                                                        catalog,
                                                        history,
                                                        warnings,
                                                    } = next;
                                                    if let Some(stream) = terminal_stream.as_mut() {
                                                        stream.attachment.detach().await.ok();
                                                    }
                                                    terminal_stream = None;
                                                    session_id = next_session_id;
                                                    presentation = next_presentation;
                                                    install_session_switch(
                                                        &mut state,
                                                        snapshot,
                                                        catalog,
                                                        history,
                                                        warnings,
                                                    );
                                                    state.notice(format!("switched to session {session_id}"));
                                                }
                                                Err(error) => state.notice(format!(
                                                    "session switch failed; source session remains active: {error:#}"
                                                )),
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Event::Paste(text) => {
                        if let Some(picker) = state.picker.as_mut() {
                            for character in text.chars() {
                                picker.push_query(character);
                            }
                        } else {
                            state.composer.insert_paste(&text);
                        }
                    }
                    Event::Resize(_, _) => {
                        drop(input.take());
                        terminal.autoresize()?;
                        terminal
                            .draw(|frame| draw(frame, &state))
                            .context("failed to render resized Chat view")?;
                        input = Some(
                            ChatInput::new().context("failed to restart Chat input reader")?
                        );
                    }
                    _ => {}
                }
            }
            event = presentation.next() => {
                match event {
                    Ok(Some(PresentationSubscriptionEvent::SnapshotReplaced { snapshot, .. })) => {
                        install_presentation_snapshot(&mut state, *snapshot);
                        reload_command_catalog(&client, &mut state).await?;
                    }
                    Ok(Some(PresentationSubscriptionEvent::Event(event))) => {
                        if apply_presentation_event(&mut state, event.event.clone()) {
                            reload_command_catalog(&client, &mut state).await?;
                        }
                    }
                    Ok(Some(PresentationSubscriptionEvent::Finished(event))) => {
                        if event.reason == agl_protocol::PresentationSubscriptionFinishReason::SessionFinished {
                            break Ok(());
                        }
                        state.notice(format!(
                            "presentation ended ({:?}); loading a fresh snapshot",
                            event.reason
                        ));
                        resubscribe_presentation(&client, &mut state, &mut presentation).await?;
                    }
                    Ok(None) => bail!("session presentation stream ended without a terminal event"),
                    Err(error) => {
                        state.notice(format!("presentation needs resync: {error}"));
                        resubscribe_presentation(&client, &mut state, &mut presentation).await?;
                    }
                }
            }
            event = async_events.recv() => {
                if let Some(event) = event {
                    apply_async_event(&mut state, &session_id, event);
                }
            }
            event = next_hidden_terminal_event(&mut terminal_stream) => {
                match event {
                    Ok(Some(ExecutionAttachmentEvent::Output(event))) => {
                        if let Some(stream) = terminal_stream.as_mut() {
                            let bytes = event.chunk.bytes.decode(65_536)
                                .context("daemon sent an invalid hidden Terminal output chunk")?;
                            stream.filter.set_visible(false);
                            let was_alternate = stream.filter.alternate_screen();
                            let report = stream.filter.filter(&bytes);
                            stream.drained_cursor = event.chunk.sequence;
                            state.terminal_cursors.insert(
                                stream.terminal.execution_id.clone(),
                                event.chunk.sequence,
                            );
                            if (!was_alternate || !stream.filter.alternate_screen())
                                && !report.bytes.is_empty()
                            {
                                stream.hidden_normal_output = true;
                            }
                        }
                    }
                    Ok(Some(ExecutionAttachmentEvent::Finished(event))) => {
                        state.notice(format!("Terminal process ended: {:?}", event.state));
                        finish_terminal_stream(&mut terminal_stream, &mut state);
                    }
                    Ok(None) => finish_terminal_stream(&mut terminal_stream, &mut state),
                    Err(error) => {
                        state.notice(format!("Terminal background stream ended: {error}"));
                        finish_terminal_stream(&mut terminal_stream, &mut state);
                    }
                }
            }
            signal = interrupt_signal.recv() => {
                if signal.is_none() {
                    bail!("SIGINT signal stream ended");
                }
                break Ok(());
            }
            signal = terminate_signal.recv() => {
                if signal.is_none() {
                    bail!("SIGTERM signal stream ended");
                }
                break Ok(());
            }
            signal = suspend_signal.recv() => {
                if signal.is_none() {
                    bail!("SIGTSTP signal stream ended");
                }
                // ChatInput joins its polling thread on drop, so synchronous
                // inline-viewport cursor queries cannot race it after SIGCONT.
                drop(input.take());
                terminal_mode.suspend();
                if unsafe { libc::raise(libc::SIGSTOP) } != 0 {
                    bail!("failed to suspend the interactive process");
                }
                terminal_mode.resume()?;
                resubscribe_presentation(&client, &mut state, &mut presentation).await?;
                terminal.autoresize().context("failed to resize Chat after SIGCONT")?;
                terminal.clear().context("failed to redraw Chat after SIGCONT")?;
                terminal
                    .draw(|frame| draw(frame, &state))
                    .context("failed to render Chat after SIGCONT")?;
                input = Some(ChatInput::new().context("failed to restart Chat input reader")?);
            }
            signal = resize_signal.recv() => {
                if signal.is_none() {
                    bail!("SIGWINCH signal stream ended");
                }
                drop(input.take());
                terminal.autoresize()?;
                terminal
                    .draw(|frame| draw(frame, &state))
                    .context("failed to render resized Chat view")?;
                input = Some(ChatInput::new().context("failed to restart Chat input reader")?);
            }
        }
    };
    drop(input.take());
    drop(terminal);
    drop(terminal_mode);
    result
}

async fn resolve_session(
    client: &AgentLibreClient,
    options: &InteractiveOptions,
) -> Result<SessionId> {
    let action = match options.resume.as_deref() {
        None => ApplicationAction::SessionNew {
            launch: SessionLaunchOptions {
                workspace_root: options
                    .workspace_root
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
                function_ref: options.function_ref.clone(),
                model_id: options.model_id.clone(),
                operation_mode: options.operation_mode.map(protocol_tool_mode),
                skill_ids: options.skills.clone(),
            },
        },
        Some("latest") => ApplicationAction::SessionResume {
            selector: SessionSelector::Latest,
        },
        Some(value) => ApplicationAction::SessionResume {
            selector: SessionSelector::Id {
                session_id: SessionId::parse(value).context("invalid --resume session ID")?,
            },
        },
    };
    let response = client
        .application_action(ApplicationActionRequest {
            session_id: None,
            client_submission_id: format!("cli-launch-{}", agl_ids::RequestId::generate()),
            action,
        })
        .await
        .context("failed to open interactive session")?;
    match response.result {
        ApplicationActionResult::SessionOpened { session_id, .. } => Ok(session_id),
        result => bail!("daemon returned an invalid launch result: {result:?}"),
    }
}

fn protocol_tool_mode(mode: crate::args::ToolAccessMode) -> ProtocolToolMode {
    match mode {
        crate::args::ToolAccessMode::Write => ProtocolToolMode::Write,
        crate::args::ToolAccessMode::Execute => ProtocolToolMode::Execute,
        crate::args::ToolAccessMode::Approve => ProtocolToolMode::Approve,
        crate::args::ToolAccessMode::Admin => ProtocolToolMode::Admin,
        crate::args::ToolAccessMode::ReadOnly => ProtocolToolMode::ReadOnly,
    }
}

fn missing_daemon_message(socket_path: &Path) -> String {
    format!(
        "agentLIBRE daemon is unavailable at {}; install/start the user socket or run `agl serve --socket {}`",
        socket_path.display(),
        socket_path.display()
    )
}

fn interactive_connect_error(socket_path: &Path, error: ClientError) -> anyhow::Error {
    let context = match &error {
        ClientError::Io(_) | ClientError::ConnectionClosed => missing_daemon_message(socket_path),
        _ => format!(
            "daemon at {} is running an incompatible protocol; restart it with the current `agl serve` binary",
            socket_path.display()
        ),
    };
    anyhow::Error::new(error).context(context)
}

enum UiControl {
    Disconnect,
    CancelRun(agl_ids::RunId),
    Submission(ComposerSubmission),
    Notice(String),
}

enum SubmissionOutcome {
    Continue,
    Disconnect,
    EnterTerminal(Box<TerminalViewRequest>),
    SwitchSession { session_id: SessionId },
}

enum CommandOutcome {
    Continue,
    Disconnect,
    EnterTerminal(Box<TerminalViewRequest>),
    SwitchSession { session_id: SessionId },
}

enum TerminalPassthroughOutcome {
    Chat,
    Disconnect,
}

struct TerminalViewRequest {
    terminal: TerminalSessionView,
    writable: bool,
}

struct TerminalStreamState {
    terminal: TerminalSessionView,
    attachment: ExecutionAttachment,
    filter: TerminalOutputFilter,
    visible_cursor: u64,
    drained_cursor: u64,
    hidden_normal_output: bool,
    replay_through_cursor: Option<u64>,
    physical_alternate_screen: Arc<AtomicBool>,
    panic_restore_bytes: Arc<Mutex<Vec<u8>>>,
    writable: bool,
}

struct PreparedSessionSwitch {
    session_id: SessionId,
    presentation: PresentationSubscription,
    snapshot: SessionPresentationSnapshot,
    catalog: Vec<CommandDescriptor>,
    history: InputHistory,
    warnings: Vec<String>,
}

async fn prepare_session_switch(
    client: &AgentLibreClient,
    session_id: SessionId,
    state_dir: &Path,
    input_history: bool,
) -> Result<PreparedSessionSwitch> {
    let presentation = client
        .subscribe_presentation(SessionPresentationSubscribeRequest {
            session_id: session_id.clone(),
        })
        .await
        .context("failed to load the selected session presentation")?;
    let catalog = client
        .command_catalog(CommandCatalogRequest {
            session_id: Some(session_id.clone()),
            client_effects: vec![
                ClientEffectKind::Help,
                ClientEffectKind::Disconnect,
                ClientEffectKind::InputHistory,
                ClientEffectKind::RawExecutionAttach,
            ],
        })
        .await
        .context("failed to load the selected session command catalog")?
        .descriptors;
    let snapshot = presentation.snapshot.clone();
    let (history, warnings) =
        InputHistory::load(state_dir, &snapshot.header.workspace_root, input_history);
    Ok(PreparedSessionSwitch {
        session_id,
        presentation,
        snapshot,
        catalog,
        history,
        warnings,
    })
}

fn install_session_switch(
    state: &mut UiState,
    snapshot: SessionPresentationSnapshot,
    catalog: Vec<CommandDescriptor>,
    history: InputHistory,
    warnings: Vec<String>,
) {
    state.snapshot = snapshot;
    state.catalog = catalog;
    state.history = history;
    state.last_terminal = state
        .snapshot
        .terminals
        .first()
        .map(|terminal| terminal.terminal_id.clone());
    state.terminal_cursors.clear();
    state.seen_terminals = state
        .snapshot
        .terminals
        .iter()
        .map(|terminal| terminal.terminal_id.clone())
        .collect();
    state.assistant_deltas.clear();
    state.picker = None;
    state.active_run = state
        .snapshot
        .active_run
        .as_ref()
        .map(|active| active.run_id.clone());
    state.exit_armed = false;
    state.workspace_change_armed = None;
    for warning in warnings {
        state.notice(warning);
    }
}

fn install_presentation_snapshot(state: &mut UiState, snapshot: SessionPresentationSnapshot) {
    state.seen_terminals.extend(
        snapshot
            .terminals
            .iter()
            .map(|terminal| terminal.terminal_id.clone()),
    );
    state.active_run = snapshot
        .active_run
        .as_ref()
        .map(|active| active.run_id.clone());
    state.snapshot = snapshot;
    state.assistant_deltas.clear();
}

type TerminalPhysicalIo<'a> = (
    &'a mut Terminal<CrosstermBackend<io::Stdout>>,
    &'a mut tokio::signal::unix::Signal,
    &'a mut tokio::signal::unix::Signal,
    &'a mut tokio::signal::unix::Signal,
    &'a mut tokio::signal::unix::Signal,
);

fn handle_key(state: &mut UiState, key: KeyEvent) -> Option<UiControl> {
    if state.picker.is_some() {
        return handle_picker_key(state, key);
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('d') => return Some(UiControl::Disconnect),
            KeyCode::Char('c') => {
                if state.composer.buffer.is_empty() {
                    if let Some(run_id) = state.active_run.take() {
                        return Some(UiControl::CancelRun(run_id));
                    } else {
                        state.notice("Ctrl+D disconnects this UI; /exit finishes the session");
                    }
                } else {
                    state.composer.reset();
                }
                return None;
            }
            _ => {}
        }
    }
    match key.code {
        KeyCode::Char(character)
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
        {
            state.composer.insert_char(character)
        }
        KeyCode::Left => state.composer.move_left(),
        KeyCode::Right => state.composer.move_right(),
        KeyCode::Home => state.composer.cursor = 0,
        KeyCode::End => state.composer.cursor = state.composer.buffer.len(),
        KeyCode::Backspace => state.composer.backspace(),
        KeyCode::Delete => state.composer.delete(),
        KeyCode::Esc => state.composer.reset(),
        KeyCode::Up if state.composer.mode == ComposerMode::Command => {
            state.composer.selected_command = state.composer.selected_command.saturating_sub(1)
        }
        KeyCode::Down if state.composer.mode == ComposerMode::Command => {
            state.composer.selected_command = state.composer.selected_command.saturating_add(1)
        }
        KeyCode::Up => {
            let entries = state.history.entries(state.composer.mode).to_vec();
            state.composer.history_previous(&entries);
        }
        KeyCode::Down => {
            let entries = state.history.entries(state.composer.mode).to_vec();
            state.composer.history_next(&entries);
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            state.composer.insert_char('\n')
        }
        KeyCode::Enter => {
            if state.composer.mode == ComposerMode::Command
                && !state.matching_commands().is_empty()
                && !state.composer.buffer.contains(char::is_whitespace)
            {
                let selected = state
                    .composer
                    .selected_command
                    .min(state.matching_commands().len() - 1);
                state.composer.buffer = state.matching_commands()[selected].name.clone();
                state.composer.cursor = state.composer.buffer.len();
            }
            return state.composer.submit().map(UiControl::Submission);
        }
        _ => {}
    }
    None
}

fn handle_picker_key(state: &mut UiState, key: KeyEvent) -> Option<UiControl> {
    let confirmation = state
        .picker
        .as_ref()
        .and_then(|picker| picker.confirmation.clone());
    if let Some(confirmation) = confirmation {
        return match key.code {
            KeyCode::Enter => {
                state.picker = None;
                Some(UiControl::Submission(ComposerSubmission::Picker(
                    confirmation.submit,
                )))
            }
            KeyCode::Esc => {
                if let Some(picker) = state.picker.as_mut() {
                    picker.confirmation = None;
                }
                None
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(picker) = state.picker.as_mut() {
                    picker.confirmation = None;
                }
                None
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(UiControl::Disconnect)
            }
            _ => None,
        };
    }

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('d') => return Some(UiControl::Disconnect),
            KeyCode::Char('c') => {
                state.picker = None;
                return None;
            }
            KeyCode::Char('a')
                if matches!(
                    state.picker.as_ref().map(|picker| &picker.kind),
                    Some(PickerKind::Skills)
                ) =>
            {
                if let Some(picker) = state.picker.as_mut() {
                    picker.select_all_skills();
                }
                return None;
            }
            KeyCode::Char('u')
                if matches!(
                    state.picker.as_ref().map(|picker| &picker.kind),
                    Some(PickerKind::Skills)
                ) =>
            {
                if let Some(picker) = state.picker.as_mut() {
                    picker.clear_skills();
                }
                return None;
            }
            KeyCode::Char('r')
                if matches!(
                    state.picker.as_ref().map(|picker| &picker.kind),
                    Some(PickerKind::Processes)
                ) =>
            {
                return selected_process_terminal(state).map_or_else(
                    || {
                        Some(UiControl::Notice(
                            "selected execution is not a terminal".to_owned(),
                        ))
                    },
                    |terminal| {
                        state.picker = None;
                        Some(UiControl::Submission(ComposerSubmission::Picker(
                            PickerSubmit::Attach {
                                terminal: Box::new(terminal),
                                writable: false,
                            },
                        )))
                    },
                );
            }
            KeyCode::Char('w')
                if matches!(
                    state.picker.as_ref().map(|picker| &picker.kind),
                    Some(PickerKind::Processes)
                ) =>
            {
                let Some(terminal) = selected_process_terminal(state) else {
                    return Some(UiControl::Notice(
                        "selected execution is not a terminal".to_owned(),
                    ));
                };
                let authority = if terminal.profile == ExecutionProfile::Host {
                    "HOST "
                } else {
                    ""
                };
                if let Some(picker) = state.picker.as_mut() {
                    picker.confirmation = Some(PickerConfirmation {
                        prompt: format!(
                            "Take the writable lease for {authority}terminal {}?",
                            terminal.terminal_id
                        ),
                        submit: PickerSubmit::Attach {
                            terminal: Box::new(terminal),
                            writable: true,
                        },
                    });
                }
                return None;
            }
            KeyCode::Char('k') | KeyCode::Char('K')
                if matches!(
                    state.picker.as_ref().map(|picker| &picker.kind),
                    Some(PickerKind::Processes)
                ) =>
            {
                let process = selected_process(state)?;
                if !process.state.is_live() {
                    return Some(UiControl::Notice(
                        "selected execution has already finished".to_owned(),
                    ));
                }
                let mode = if key.modifiers.contains(KeyModifiers::SHIFT) {
                    KillMode::Immediate
                } else {
                    KillMode::Graceful
                };
                if let Some(picker) = state.picker.as_mut() {
                    picker.confirmation = Some(PickerConfirmation {
                        prompt: format!(
                            "Terminate execution {} with {mode:?} mode?",
                            process.execution_id
                        ),
                        submit: PickerSubmit::Kill {
                            execution_id: process.execution_id,
                            mode,
                        },
                    });
                }
                return None;
            }
            KeyCode::Char('p')
                if matches!(
                    state.picker.as_ref().map(|picker| &picker.kind),
                    Some(PickerKind::Processes)
                ) =>
            {
                let Some(terminal) = selected_process_terminal(state) else {
                    return Some(UiControl::Notice(
                        "selected execution is not a terminal".to_owned(),
                    ));
                };
                if !matches!(terminal.owner, TerminalOwnerView::Subagent { .. }) {
                    return Some(UiControl::Notice(
                        "only a subagent terminal can be promoted".to_owned(),
                    ));
                }
                if let Some(picker) = state.picker.as_mut() {
                    picker.confirmation = Some(PickerConfirmation {
                        prompt: format!(
                            "Promote subagent terminal {} to the durable session?",
                            terminal.terminal_id
                        ),
                        submit: PickerSubmit::Promote {
                            terminal_id: terminal.terminal_id,
                        },
                    });
                }
                return None;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Esc => state.picker = None,
        KeyCode::Up => state.picker.as_mut()?.move_selection(-1),
        KeyCode::Down => state.picker.as_mut()?.move_selection(1),
        KeyCode::PageUp => state.picker.as_mut()?.move_selection(-8),
        KeyCode::PageDown => state.picker.as_mut()?.move_selection(8),
        KeyCode::Home => state.picker.as_mut()?.selected = 0,
        KeyCode::End => {
            let length = state.picker.as_ref()?.filtered_indices().len();
            state.picker.as_mut()?.selected = length.saturating_sub(1);
        }
        KeyCode::Backspace => state.picker.as_mut()?.pop_query(),
        KeyCode::Char(' ') if matches!(&state.picker.as_ref()?.kind, PickerKind::Skills) => {
            state.picker.as_mut()?.toggle_selected_skill()
        }
        KeyCode::Enter => return submit_current_picker(state),
        KeyCode::Char(character)
            if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
        {
            state.picker.as_mut()?.push_query(character)
        }
        _ => {}
    }
    None
}

fn selected_process(state: &UiState) -> Option<ProcessPickerItem> {
    match &state.picker.as_ref()?.selected_entry()?.payload {
        PickerPayload::Process(process) => Some(process.as_ref().clone()),
        _ => None,
    }
}

fn selected_process_terminal(state: &UiState) -> Option<TerminalSessionView> {
    selected_process(state)?.terminal
}

fn submit_current_picker(state: &mut UiState) -> Option<UiControl> {
    let submit = match picker_default_submit(state.picker.as_ref()?) {
        Ok(submit) => submit,
        Err(message) => return Some(UiControl::Notice(message.to_owned())),
    };
    match submit {
        PickerSubmit::EnsureHost { startup } => {
            let prompt = match startup {
                HostStartupPolicy::ManagedOnly => {
                    "Create or select a distinct HOST terminal with managed startup? This grants explicit Host process authority for that terminal lifetime."
                }
                HostStartupPolicy::SourceUserRc => {
                    "Create or select a distinct HOST terminal and source your normal shell rc? This grants explicit Host process authority and runs your user rc configuration."
                }
            };
            state.picker.as_mut()?.confirmation = Some(PickerConfirmation {
                prompt: prompt.to_owned(),
                submit: PickerSubmit::EnsureHost { startup },
            });
            None
        }
        submit => {
            state.picker = None;
            Some(UiControl::Submission(ComposerSubmission::Picker(submit)))
        }
    }
}

fn picker_default_submit(picker: &PickerState) -> std::result::Result<PickerSubmit, &'static str> {
    Ok(match &picker.kind {
        PickerKind::Skills => {
            PickerSubmit::Skills(picker.selected_values.iter().cloned().collect::<Vec<_>>())
        }
        PickerKind::Processes => {
            let Some(entry) = picker.selected_entry() else {
                return Err("no matching execution is selected");
            };
            if let PickerPayload::EnsureHost(startup) = &entry.payload {
                return Ok(PickerSubmit::EnsureHost { startup: *startup });
            }
            let PickerPayload::Process(process) = &entry.payload else {
                return Err("process picker entry has an invalid action type");
            };
            let Some(terminal) = &process.terminal else {
                return Err(
                    "selected execution has no interactive terminal; use Ctrl+K to terminate it",
                );
            };
            PickerSubmit::Attach {
                writable: matches!(&terminal.owner, TerminalOwnerView::Human { .. }),
                terminal: Box::new(terminal.clone()),
            }
        }
        PickerKind::Resume | PickerKind::Model | PickerKind::Mode => {
            let Some(entry) = picker.selected_entry() else {
                return Err("no matching picker entry is selected");
            };
            match &entry.payload {
                PickerPayload::Resume(session_id) => PickerSubmit::Resume(session_id.clone()),
                PickerPayload::Model(model_id) => PickerSubmit::Model(model_id.clone()),
                PickerPayload::Mode(mode) => PickerSubmit::Mode(*mode),
                PickerPayload::Skill(_)
                | PickerPayload::EnsureHost(_)
                | PickerPayload::Process(_) => {
                    return Err("picker entry has an invalid action type");
                }
            }
        }
    })
}

async fn handle_submission(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
    submission: ComposerSubmission,
    sender: &mpsc::Sender<UiAsyncEvent>,
) -> Result<SubmissionOutcome> {
    match &submission {
        ComposerSubmission::Prompt(input) => {
            if let Err(error) = state.history.record(HistoryMode::Prompt, input) {
                state.notice(format!("prompt history write failed: {error:#}"));
            }
        }
        ComposerSubmission::SwitchTerminal => {}
        ComposerSubmission::Command(_) => {}
        ComposerSubmission::Picker(_) => {}
    }
    match submission {
        ComposerSubmission::Prompt(content) => {
            spawn_prompt(client.clone(), session_id.clone(), content, sender.clone());
        }
        ComposerSubmission::SwitchTerminal => {
            if let Some(terminal) = state.last_terminal.as_ref().and_then(|terminal_id| {
                state
                    .snapshot
                    .terminals
                    .iter()
                    .find(|terminal| {
                        &terminal.terminal_id == terminal_id && terminal.process_state.is_live()
                    })
                    .cloned()
            }) {
                let writable = matches!(terminal.owner, TerminalOwnerView::Human { .. });
                return Ok(SubmissionOutcome::EnterTerminal(Box::new(
                    TerminalViewRequest { terminal, writable },
                )));
            }
            let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 24));
            let shell_profile_id = state.shell_profile_id.clone().ok_or_else(|| {
                anyhow::anyhow!("configured shell is not an admitted managed Bash/Zsh profile")
            })?;
            let ensured = client
                .ensure_human_terminal(HumanTerminalEnsureRequest {
                    session_id: session_id.clone(),
                    client_submission_id: format!(
                        "cli-terminal-{}",
                        agl_ids::RequestId::generate()
                    ),
                    execution_context_revision: state.snapshot.header.execution_context_revision,
                    profile: ExecutionProfile::Workspace,
                    shell_profile_id,
                    terminal_size: TerminalSize {
                        columns: columns.max(1),
                        rows: rows.max(1),
                    },
                    agl_env: current_terminal_environment(),
                    host_startup: HostStartupPolicy::ManagedOnly,
                })
                .await
                .context("failed to ensure the Human workspace terminal")?;
            state.last_terminal = Some(ensured.terminal.terminal_id.clone());
            return Ok(SubmissionOutcome::EnterTerminal(Box::new(
                TerminalViewRequest {
                    terminal: ensured.terminal,
                    writable: true,
                },
            )));
        }
        ComposerSubmission::Command(command) => {
            return match handle_command(client, session_id, state, &command).await? {
                CommandOutcome::Continue => Ok(SubmissionOutcome::Continue),
                CommandOutcome::Disconnect => Ok(SubmissionOutcome::Disconnect),
                CommandOutcome::EnterTerminal(request) => {
                    Ok(SubmissionOutcome::EnterTerminal(request))
                }
                CommandOutcome::SwitchSession { session_id } => {
                    Ok(SubmissionOutcome::SwitchSession { session_id })
                }
            };
        }
        ComposerSubmission::Picker(submit) => {
            return handle_picker_submit(client, session_id, state, submit).await;
        }
    }
    Ok(SubmissionOutcome::Continue)
}

async fn handle_picker_submit(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
    submit: PickerSubmit,
) -> Result<SubmissionOutcome> {
    let (action, attach_terminal) = match submit {
        PickerSubmit::Resume(session_id) => (
            ApplicationAction::SessionResume {
                selector: SessionSelector::Id { session_id },
            },
            None,
        ),
        PickerSubmit::Model(model_id) => (ApplicationAction::ModelSelect { model_id }, None),
        PickerSubmit::Mode(mode) => (ApplicationAction::OperationModeSelect { mode }, None),
        PickerSubmit::Skills(skill_ids) => (ApplicationAction::SkillsSelect { skill_ids }, None),
        PickerSubmit::EnsureHost { startup } => {
            return Ok(handle_host_terminal_submit(client, session_id, state, startup).await);
        }
        PickerSubmit::Attach { terminal, writable } => (
            ApplicationAction::ExecutionAttach {
                execution_id: terminal.execution_id.clone(),
                read_only: !writable,
            },
            Some((terminal, writable)),
        ),
        PickerSubmit::Kill { execution_id, mode } => (
            ApplicationAction::ExecutionKill { execution_id, mode },
            None,
        ),
        PickerSubmit::Promote { terminal_id } => {
            (ApplicationAction::TerminalPromote { terminal_id }, None)
        }
    };
    let response = client
        .application_action(ApplicationActionRequest {
            session_id: Some(session_id.clone()),
            client_submission_id: format!("cli-picker-{}", agl_ids::RequestId::generate()),
            action,
        })
        .await;
    match response {
        Ok(event) => match event.result {
            ApplicationActionResult::SessionOpened { session_id, .. } => {
                Ok(SubmissionOutcome::SwitchSession { session_id })
            }
            ApplicationActionResult::ModelChanged { header } => {
                state.snapshot.header = header;
                reload_command_catalog(client, state).await?;
                state.notice("model selection changed");
                Ok(SubmissionOutcome::Continue)
            }
            ApplicationActionResult::ModeChanged { header } => {
                state.snapshot.header = header;
                reload_command_catalog(client, state).await?;
                state.notice("operation mode changed");
                Ok(SubmissionOutcome::Continue)
            }
            ApplicationActionResult::SkillsChanged { header } => {
                state.snapshot.header = header;
                reload_command_catalog(client, state).await?;
                state.notice("skill selection changed");
                Ok(SubmissionOutcome::Continue)
            }
            ApplicationActionResult::AttachAccepted {
                execution_id,
                read_only,
            } => {
                let (terminal, writable) = attach_terminal
                    .context("daemon accepted a picker attachment with no terminal")?;
                if terminal.execution_id != execution_id || read_only == writable {
                    bail!("daemon returned a mismatched picker attachment result");
                }
                state.last_terminal = Some(terminal.terminal_id.clone());
                Ok(SubmissionOutcome::EnterTerminal(Box::new(
                    TerminalViewRequest {
                        terminal: *terminal,
                        writable,
                    },
                )))
            }
            ApplicationActionResult::KillAccepted { execution_id, mode } => {
                state.notice(format!(
                    "execution {execution_id} termination requested ({mode:?})"
                ));
                Ok(SubmissionOutcome::Continue)
            }
            ApplicationActionResult::TerminalPromoted { terminal } => {
                state.last_terminal = Some(terminal.terminal_id.clone());
                let _ = apply_presentation_event(
                    state,
                    agl_protocol::SessionPresentationEventPayload::TerminalChanged { terminal },
                );
                state.notice("subagent terminal promoted to the durable session");
                Ok(SubmissionOutcome::Continue)
            }
            result => bail!("daemon returned an invalid picker action result: {result:?}"),
        },
        Err(error) => {
            state.notice(format!("picker action failed: {error}"));
            Ok(SubmissionOutcome::Continue)
        }
    }
}

trait HostTerminalEnsurer {
    async fn ensure_host_terminal(
        &self,
        request: HumanHostTerminalEnsureRequest,
    ) -> std::result::Result<HumanTerminalEnsuredEvent, ClientError>;
}

impl HostTerminalEnsurer for AgentLibreClient {
    async fn ensure_host_terminal(
        &self,
        request: HumanHostTerminalEnsureRequest,
    ) -> std::result::Result<HumanTerminalEnsuredEvent, ClientError> {
        self.ensure_human_host_terminal(request).await
    }
}

async fn handle_host_terminal_submit(
    client: &impl HostTerminalEnsurer,
    session_id: &SessionId,
    state: &mut UiState,
    startup: HostStartupPolicy,
) -> SubmissionOutcome {
    let request = match host_terminal_request(session_id, state, startup, current_terminal_size()) {
        Ok(request) => request,
        Err(error) => {
            state.notice(format!("HOST terminal ensure failed: {error:#}"));
            return SubmissionOutcome::Continue;
        }
    };
    let ensured = match client.ensure_host_terminal(request).await {
        Ok(ensured) => ensured,
        Err(error) => {
            state.notice(format!("HOST terminal ensure failed: {error}"));
            return SubmissionOutcome::Continue;
        }
    };
    let terminal = ensured.terminal;
    if terminal.profile != ExecutionProfile::Host
        || !matches!(
            &terminal.owner,
            TerminalOwnerView::Human {
                session_id: owner_session_id
            } if owner_session_id == session_id
        )
    {
        state.notice("HOST terminal ensure failed: daemon returned a non-Human Host terminal");
        return SubmissionOutcome::Continue;
    }
    if state.snapshot.terminals.iter().any(|workspace| {
        workspace.profile == ExecutionProfile::Workspace
            && (workspace.terminal_id == terminal.terminal_id
                || workspace.execution_id == terminal.execution_id)
    }) {
        state.notice(
            "HOST terminal ensure failed: daemon attempted to reuse a Workspace terminal identity",
        );
        return SubmissionOutcome::Continue;
    }
    if !terminal.process_state.is_live() {
        state.notice("HOST terminal ensure failed: durable Host terminal is not live");
        return SubmissionOutcome::Continue;
    }

    state.last_terminal = Some(terminal.terminal_id.clone());
    let _ = apply_presentation_event(
        state,
        agl_protocol::SessionPresentationEventPayload::TerminalAdded {
            terminal: terminal.clone(),
        },
    );
    SubmissionOutcome::EnterTerminal(Box::new(TerminalViewRequest {
        terminal,
        writable: true,
    }))
}

fn host_terminal_request(
    session_id: &SessionId,
    state: &UiState,
    startup: HostStartupPolicy,
    terminal_size: TerminalSize,
) -> Result<HumanHostTerminalEnsureRequest> {
    let shell_profile_id = state.shell_profile_id.clone().ok_or_else(|| {
        anyhow::anyhow!("configured shell is not an admitted managed Bash/Zsh profile")
    })?;
    Ok(HumanHostTerminalEnsureRequest {
        terminal: HumanTerminalEnsureRequest {
            session_id: session_id.clone(),
            client_submission_id: format!("cli-host-terminal-{}", agl_ids::RequestId::generate()),
            execution_context_revision: state.snapshot.header.execution_context_revision,
            profile: ExecutionProfile::Host,
            shell_profile_id,
            terminal_size,
            agl_env: current_terminal_environment(),
            host_startup: startup,
        },
        confirm_host_authority: true,
    })
}

fn current_terminal_size() -> TerminalSize {
    let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    TerminalSize {
        columns: columns.max(1),
        rows: rows.max(1),
    }
}

fn current_terminal_environment() -> StructuredEnvironmentOverlay {
    let terminal_name = std::env::var("TERM").ok();
    terminal_environment_for(terminal_name.as_deref())
}

fn terminal_environment_for(terminal_name: Option<&str>) -> StructuredEnvironmentOverlay {
    const DEFAULT_TERMINAL_NAME: &str = "xterm-256color";
    const MAX_TERMINAL_NAME_BYTES: usize = 128;

    let terminal_name = terminal_name
        .filter(|name| {
            !name.is_empty()
                && name.len() <= MAX_TERMINAL_NAME_BYTES
                && name.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'+' | b'.')
                })
        })
        .unwrap_or(DEFAULT_TERMINAL_NAME);
    StructuredEnvironmentOverlay {
        values: BTreeMap::from([("TERM".to_owned(), terminal_name.to_owned())]),
        ..StructuredEnvironmentOverlay::default()
    }
}

#[cfg(target_os = "linux")]
async fn run_terminal_passthrough(
    client: &AgentLibreClient,
    request: TerminalViewRequest,
    state: &mut UiState,
    presentation: &mut PresentationSubscription,
    async_events: &mut mpsc::Receiver<UiAsyncEvent>,
    terminal_io: TerminalPhysicalIo<'_>,
    terminal_stream: &mut Option<TerminalStreamState>,
) -> Result<TerminalPassthroughOutcome> {
    let result = run_terminal_passthrough_inner(
        client,
        request,
        state,
        presentation,
        async_events,
        terminal_io,
        terminal_stream,
    )
    .await;
    let restore_result = restore_chat_terminal_modes(terminal_stream);
    match (result, restore_result) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), _) => Err(error),
    }
}

#[cfg(target_os = "linux")]
async fn run_terminal_passthrough_inner(
    client: &AgentLibreClient,
    request: TerminalViewRequest,
    state: &mut UiState,
    presentation: &mut PresentationSubscription,
    async_events: &mut mpsc::Receiver<UiAsyncEvent>,
    terminal_io: TerminalPhysicalIo<'_>,
    terminal_stream: &mut Option<TerminalStreamState>,
) -> Result<TerminalPassthroughOutcome> {
    let (terminal, interrupt_signal, terminate_signal, suspend_signal, resize_signal) = terminal_io;
    let TerminalViewRequest {
        terminal: terminal_view,
        writable,
    } = request;
    let terminal_id = terminal_view.terminal_id.clone();
    let execution_id = terminal_view.execution_id.clone();
    prepare_terminal_stream(
        client,
        terminal_view.clone(),
        writable,
        state,
        terminal_stream,
    )
    .await?;
    let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 24));

    terminal
        .clear()
        .context("failed to clear Chat before Terminal view")?;
    let mut stdout = io::stdout();
    execute!(stdout, Show).context("failed to restore the Terminal cursor")?;
    let stream = terminal_stream
        .as_mut()
        .context("Terminal stream was not installed")?;
    refresh_terminal_panic_restore(stream);
    let _restore_chat_view = ChatViewRestore {
        physical_alternate_screen: Arc::clone(&stream.physical_alternate_screen),
        restore_bytes: Arc::clone(&stream.panic_restore_bytes),
    };
    writeln!(
        stdout,
        "\r! agentLIBRE Terminal · {} · {} · {} · Esc then ! returns to Chat\r",
        terminal_owner_label(&terminal_view.owner),
        terminal_authority_label(terminal_view.profile),
        if stream.writable {
            "writable"
        } else {
            "read-only"
        },
    )?;
    sync_physical_terminal_modes(&mut stdout, stream)?;
    stream.filter.set_visible(true);
    stream
        .attachment
        .resize(columns.max(1), rows.max(1))
        .await
        .context("failed to send the Terminal view size")?;
    stdout.flush()?;

    let raw_input = RawTtyInput::open().context("failed to open /dev/tty for Terminal input")?;
    let mut input_buffer = [0_u8; 4096];
    let mut input_gate = RawTerminalInputGate::default();
    let initial_actions = update_terminal_input_gate(&mut input_gate, terminal_view.prompt_state);
    if forward_terminal_actions(&stream.attachment, stream.writable, initial_actions).await? {
        stream.filter.set_visible(false);
        return Ok(TerminalPassthroughOutcome::Chat);
    }
    let blocked_before = stream.filter.blocked_total();
    let malformed_before = stream.filter.malformed_total();
    let clock = Instant::now();
    let mut stream_ended = false;
    let mut disconnect = false;

    'terminal_view: loop {
        tokio::select! {
            read = raw_input.read(&mut input_buffer) => {
                let count = read.context("failed to read Terminal input")?;
                if count == 0 {
                    break 'terminal_view;
                }
                let actions = input_gate.handle_bytes(&input_buffer[..count], clock.elapsed());
                if forward_terminal_actions(&stream.attachment, stream.writable, actions).await? {
                    break 'terminal_view;
                }
            }
            event = stream.attachment.next() => {
                match event.context("Terminal attachment failed")? {
                    Some(ExecutionAttachmentEvent::Output(event)) => {
                        state.terminal_cursors.insert(execution_id.clone(), event.chunk.sequence);
                        stream.visible_cursor = event.chunk.sequence;
                        stream.drained_cursor = event.chunk.sequence;
                        let bytes = event.chunk.bytes.decode(65_536)
                            .context("daemon sent an invalid Terminal output chunk")?;
                        let stale_replay = stream
                            .replay_through_cursor
                            .is_some_and(|cursor| event.chunk.sequence <= cursor);
                        let report = if stale_replay {
                            stream.filter.filter_stale_replay(&bytes)
                        } else {
                            stream.filter.filter(&bytes)
                        };
                        refresh_terminal_panic_restore(stream);
                        let replay_completed = stream
                            .replay_through_cursor
                            .is_some_and(|cursor| event.chunk.sequence >= cursor);
                        if replay_completed {
                            stream.replay_through_cursor = None;
                        }
                        if !report.bytes.is_empty() {
                            stdout.write_all(&report.bytes)?;
                            stdout.flush()?;
                        }
                        if replay_completed {
                            sync_physical_terminal_modes(&mut stdout, stream)?;
                        } else {
                            stream
                                .physical_alternate_screen
                                .store(stream.filter.alternate_screen(), Ordering::Release);
                        }
                    }
                    Some(ExecutionAttachmentEvent::Finished(event)) => {
                        state.terminal_cursors.insert(execution_id.clone(), event.last_delivered_sequence);
                        state.notice(format!("Terminal process ended: {:?}", event.state));
                        stream_ended = true;
                        break 'terminal_view;
                    }
                    None => {
                        state.notice("Terminal attachment ended");
                        stream_ended = true;
                        break 'terminal_view;
                    }
                }
            }
            event = presentation.next() => {
                match event {
                    Ok(Some(PresentationSubscriptionEvent::SnapshotReplaced { snapshot, .. })) => {
                        install_presentation_snapshot(state, *snapshot);
                        reload_command_catalog(client, state).await?;
                        let prompt_state = terminal_prompt_from_snapshot(&state.snapshot, &terminal_id);
                        let actions = update_terminal_input_gate(&mut input_gate, prompt_state);
                        if forward_terminal_actions(&stream.attachment, stream.writable, actions).await? {
                            break 'terminal_view;
                        }
                    }
                    Ok(Some(PresentationSubscriptionEvent::Event(event))) => {
                        let prompt_state = terminal_prompt_from_event(&event.event, &terminal_id);
                        if apply_presentation_event(state, event.event.clone()) {
                            reload_command_catalog(client, state).await?;
                        }
                        if let Some(prompt_state) = prompt_state {
                            let actions = update_terminal_input_gate(&mut input_gate, prompt_state);
                            if forward_terminal_actions(&stream.attachment, stream.writable, actions).await? {
                                break 'terminal_view;
                            }
                        }
                    }
                    Ok(Some(PresentationSubscriptionEvent::Finished(event))) => {
                        if event.reason == agl_protocol::PresentationSubscriptionFinishReason::SessionFinished {
                            disconnect = true;
                            break 'terminal_view;
                        }
                        state.notice(format!(
                            "presentation ended ({:?}); loading a fresh snapshot",
                            event.reason
                        ));
                        resubscribe_presentation(client, state, presentation).await?;
                        let prompt_state = state.snapshot.terminals.iter()
                            .find(|terminal| terminal.terminal_id == terminal_id)
                            .map(|terminal| terminal.prompt_state)
                            .unwrap_or(TerminalPromptState::Unavailable);
                        let actions = update_terminal_input_gate(&mut input_gate, prompt_state);
                        if forward_terminal_actions(&stream.attachment, stream.writable, actions).await? {
                            break 'terminal_view;
                        }
                    }
                    Ok(None) => bail!("session presentation stream ended without a terminal event"),
                    Err(error) => {
                        state.notice(format!("presentation needs resync: {error}"));
                        resubscribe_presentation(client, state, presentation).await?;
                        let prompt_state = state.snapshot.terminals.iter()
                            .find(|terminal| terminal.terminal_id == terminal_id)
                            .map(|terminal| terminal.prompt_state)
                            .unwrap_or(TerminalPromptState::Unavailable);
                        let actions = update_terminal_input_gate(&mut input_gate, prompt_state);
                        if forward_terminal_actions(&stream.attachment, stream.writable, actions).await? {
                            break 'terminal_view;
                        }
                    }
                }
            }
            event = async_events.recv() => {
                if let Some(event) = event {
                    let session_id = state.snapshot.header.session_id.clone();
                    let before = state.snapshot.terminals.iter()
                        .find(|terminal| terminal.terminal_id == terminal_id)
                        .map(|terminal| terminal.prompt_state);
                    apply_async_event(state, &session_id, event);
                    let after = state.snapshot.terminals.iter()
                        .find(|terminal| terminal.terminal_id == terminal_id)
                        .map(|terminal| terminal.prompt_state);
                    if after != before && let Some(prompt_state) = after {
                        let actions = update_terminal_input_gate(&mut input_gate, prompt_state);
                        if forward_terminal_actions(&stream.attachment, stream.writable, actions).await? {
                            break 'terminal_view;
                        }
                    }
                }
            }
            resize = resize_signal.recv() => {
                if resize.is_none() {
                    bail!("Terminal resize signal stream ended");
                }
                let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                stream.attachment.resize(columns.max(1), rows.max(1)).await
                    .context("failed to resize the Terminal view")?;
            }
            signal = interrupt_signal.recv() => {
                if signal.is_none() {
                    bail!("SIGINT signal stream ended");
                }
                disconnect = true;
                break 'terminal_view;
            }
            signal = terminate_signal.recv() => {
                if signal.is_none() {
                    bail!("SIGTERM signal stream ended");
                }
                disconnect = true;
                break 'terminal_view;
            }
            signal = suspend_signal.recv() => {
                if signal.is_none() {
                    bail!("SIGTSTP signal stream ended");
                }
                restore_chat_terminal_modes_for_stream(&mut stdout, stream)?;
                restore_physical_terminal();
                if unsafe { libc::raise(libc::SIGSTOP) } != 0 {
                    bail!("failed to suspend the interactive process");
                }
                enable_raw_mode().context("failed to restore raw mode after SIGCONT")?;
                execute!(stdout, EnableBracketedPaste, Show)
                    .context("failed to restore Terminal mode after SIGCONT")?;
                resubscribe_presentation(client, state, presentation).await?;
                let prompt_state = state.snapshot.terminals.iter()
                    .find(|terminal| terminal.terminal_id == terminal_id)
                    .map(|terminal| terminal.prompt_state)
                    .unwrap_or(TerminalPromptState::Unavailable);
                let actions = update_terminal_input_gate(&mut input_gate, prompt_state);
                if forward_terminal_actions(&stream.attachment, stream.writable, actions).await? {
                    break 'terminal_view;
                }
                sync_physical_terminal_modes(&mut stdout, stream)?;
                let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                stream.attachment.resize(columns.max(1), rows.max(1)).await
                    .context("failed to redraw the Terminal view after SIGCONT")?;
            }
        }
    }

    stream.filter.set_visible(false);
    let blocked = stream.filter.blocked_total().saturating_sub(blocked_before);
    let malformed = stream
        .filter
        .malformed_total()
        .saturating_sub(malformed_before);
    if blocked > 0 || malformed > 0 {
        state.notice(format!(
            "Terminal filtered {} high-risk and {} malformed control sequence(s)",
            blocked, malformed,
        ));
    }
    if stream_ended {
        finish_terminal_stream(terminal_stream, state);
    }
    Ok(if disconnect {
        TerminalPassthroughOutcome::Disconnect
    } else {
        TerminalPassthroughOutcome::Chat
    })
}

async fn prepare_terminal_stream(
    client: &AgentLibreClient,
    terminal_view: TerminalSessionView,
    writable: bool,
    state: &mut UiState,
    terminal_stream: &mut Option<TerminalStreamState>,
) -> Result<()> {
    let execution_id = terminal_view.execution_id.clone();
    if let Some(mut existing) = terminal_stream.take() {
        if existing.terminal.terminal_id == terminal_view.terminal_id {
            existing.replay_through_cursor = None;
            let replay_after =
                if existing.writable != writable || existing.filter.alternate_screen() {
                    Some(existing.drained_cursor)
                } else if existing.hidden_normal_output {
                    Some(existing.visible_cursor)
                } else {
                    None
                };
            if let Some(after_sequence) = replay_after {
                let replay_through_cursor =
                    (after_sequence == existing.visible_cursor).then_some(existing.drained_cursor);
                existing.attachment.detach().await.ok();
                existing.attachment = client
                    .attach_execution(execution_id.clone(), after_sequence, writable)
                    .await
                    .context("failed to resume the Human terminal")?;
                existing.visible_cursor = after_sequence;
                existing.drained_cursor = after_sequence;
                existing.hidden_normal_output = false;
                existing.replay_through_cursor = replay_through_cursor;
                if !existing.filter.alternate_screen() {
                    existing.filter = TerminalOutputFilter::new(true);
                }
            }
            existing.terminal = terminal_view;
            existing.writable = existing.attachment.started.writable;
            existing.filter.set_visible(true);
            *terminal_stream = Some(existing);
            return Ok(());
        }
        existing.attachment.detach().await.ok();
        let _ = existing.filter.finish();
    }

    let first_attach = state
        .seen_terminals
        .insert(terminal_view.terminal_id.clone());
    let after_sequence = if first_attach {
        0
    } else {
        match client
            .execution_status(ExecutionStatusRequest {
                execution_id: execution_id.clone(),
                include_private_command: false,
            })
            .await
        {
            Ok(status) => status.status.last_sequence,
            Err(_) => state
                .terminal_cursors
                .get(&execution_id)
                .copied()
                .unwrap_or_default(),
        }
    };
    let attachment = client
        .attach_execution(execution_id.clone(), after_sequence, writable)
        .await
        .context("failed to attach the Human terminal")?;
    state.terminal_cursors.insert(execution_id, after_sequence);
    let writable = attachment.started.writable;
    *terminal_stream = Some(TerminalStreamState {
        terminal: terminal_view,
        attachment,
        filter: TerminalOutputFilter::new(true),
        visible_cursor: after_sequence,
        drained_cursor: after_sequence,
        hidden_normal_output: false,
        replay_through_cursor: None,
        physical_alternate_screen: Arc::new(AtomicBool::new(false)),
        panic_restore_bytes: Arc::new(Mutex::new(Vec::new())),
        writable,
    });
    Ok(())
}

fn sync_physical_terminal_modes(
    stdout: &mut io::Stdout,
    stream: &mut TerminalStreamState,
) -> Result<()> {
    refresh_terminal_panic_restore(stream);
    stdout.write_all(&stream.filter.terminal_mode_restore_bytes())?;
    stream
        .physical_alternate_screen
        .store(stream.filter.alternate_screen(), Ordering::Release);
    stdout.flush()?;
    Ok(())
}

fn restore_chat_terminal_modes(terminal_stream: &mut Option<TerminalStreamState>) -> Result<()> {
    let Some(stream) = terminal_stream.as_mut() else {
        return Ok(());
    };
    restore_chat_terminal_modes_for_stream(&mut io::stdout(), stream)
}

fn restore_chat_terminal_modes_for_stream(
    stdout: &mut io::Stdout,
    stream: &mut TerminalStreamState,
) -> Result<()> {
    stdout.write_all(&stream.filter.chat_mode_restore_bytes())?;
    stdout.flush()?;
    stream
        .physical_alternate_screen
        .store(false, Ordering::Release);
    Ok(())
}

fn refresh_terminal_panic_restore(stream: &TerminalStreamState) {
    let bytes = stream.filter.chat_mode_restore_bytes();
    *stream
        .panic_restore_bytes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = bytes;
}

async fn next_hidden_terminal_event(
    terminal_stream: &mut Option<TerminalStreamState>,
) -> Result<Option<ExecutionAttachmentEvent>, ClientError> {
    match terminal_stream.as_mut() {
        Some(stream) => stream.attachment.next().await,
        None => std::future::pending().await,
    }
}

fn finish_terminal_stream(terminal_stream: &mut Option<TerminalStreamState>, state: &mut UiState) {
    let Some(mut stream) = terminal_stream.take() else {
        return;
    };
    let report = stream.filter.finish();
    state
        .terminal_cursors
        .insert(stream.terminal.execution_id, stream.drained_cursor);
    if report.malformed_sequences > 0 {
        state.notice("Terminal stream ended inside a malformed control sequence");
    }
}

#[cfg(not(target_os = "linux"))]
async fn run_terminal_passthrough(
    _client: &AgentLibreClient,
    _request: TerminalViewRequest,
    _state: &mut UiState,
    _presentation: &mut PresentationSubscription,
    _async_events: &mut mpsc::Receiver<UiAsyncEvent>,
    _terminal_io: TerminalPhysicalIo<'_>,
    _terminal_stream: &mut Option<TerminalStreamState>,
) -> Result<TerminalPassthroughOutcome> {
    bail!("Terminal view is currently supported only on Linux")
}

async fn forward_terminal_actions(
    attachment: &ExecutionAttachment,
    writable: bool,
    actions: Vec<TerminalInputAction>,
) -> Result<bool> {
    for action in actions {
        match action {
            TerminalInputAction::Forward(bytes) if writable && !bytes.is_empty() => {
                attachment
                    .input(ProcessBytes::from_bytes(&bytes), false)
                    .await
                    .context("failed to forward Terminal input")?;
            }
            TerminalInputAction::Forward(_) => {}
            TerminalInputAction::SwitchToChat => return Ok(true),
        }
    }
    Ok(false)
}

fn update_terminal_input_gate(
    input_gate: &mut RawTerminalInputGate,
    prompt_state: TerminalPromptState,
) -> Vec<TerminalInputAction> {
    match prompt_state {
        TerminalPromptState::Ready => {
            input_gate.prompt_ready();
            Vec::new()
        }
        TerminalPromptState::Degraded | TerminalPromptState::Unavailable => {
            input_gate.integration_degraded()
        }
        TerminalPromptState::Starting
        | TerminalPromptState::CommandRunning
        | TerminalPromptState::ForegroundProcess => input_gate.prompt_busy(),
    }
}

fn terminal_prompt_from_event(
    event: &agl_protocol::SessionPresentationEventPayload,
    terminal_id: &TerminalSessionId,
) -> Option<TerminalPromptState> {
    match event {
        agl_protocol::SessionPresentationEventPayload::TerminalAdded { terminal }
        | agl_protocol::SessionPresentationEventPayload::TerminalChanged { terminal }
            if &terminal.terminal_id == terminal_id =>
        {
            Some(terminal.prompt_state)
        }
        agl_protocol::SessionPresentationEventPayload::TerminalRemoved {
            terminal_id: removed,
        } if removed == terminal_id => Some(TerminalPromptState::Unavailable),
        agl_protocol::SessionPresentationEventPayload::TerminalCommandStarted {
            terminal_id: changed,
            ..
        } if changed == terminal_id => Some(TerminalPromptState::CommandRunning),
        agl_protocol::SessionPresentationEventPayload::TerminalCommandFinished {
            terminal_id: changed,
            ..
        } if changed == terminal_id => Some(TerminalPromptState::Ready),
        _ => None,
    }
}

fn terminal_prompt_from_snapshot(
    snapshot: &SessionPresentationSnapshot,
    terminal_id: &TerminalSessionId,
) -> TerminalPromptState {
    snapshot
        .terminals
        .iter()
        .find(|terminal| &terminal.terminal_id == terminal_id)
        .map(|terminal| terminal.prompt_state)
        .unwrap_or(TerminalPromptState::Unavailable)
}

async fn resubscribe_presentation(
    client: &AgentLibreClient,
    state: &mut UiState,
    presentation: &mut PresentationSubscription,
) -> Result<()> {
    let replacement = client
        .subscribe_presentation(SessionPresentationSubscribeRequest {
            session_id: state.snapshot.header.session_id.clone(),
        })
        .await
        .context("failed to resubscribe to the session presentation")?;
    state.snapshot = replacement.snapshot.clone();
    state.assistant_deltas.clear();
    *presentation = replacement;
    reload_command_catalog(client, state).await?;
    Ok(())
}

async fn reload_command_catalog(client: &AgentLibreClient, state: &mut UiState) -> Result<()> {
    state.catalog = client
        .command_catalog(CommandCatalogRequest {
            session_id: Some(state.snapshot.header.session_id.clone()),
            client_effects: vec![
                ClientEffectKind::Help,
                ClientEffectKind::Disconnect,
                ClientEffectKind::InputHistory,
                ClientEffectKind::RawExecutionAttach,
            ],
        })
        .await
        .context("failed to refresh the command catalog")?
        .descriptors;
    Ok(())
}

struct ChatViewRestore {
    physical_alternate_screen: Arc<AtomicBool>,
    restore_bytes: Arc<Mutex<Vec<u8>>>,
}

impl Drop for ChatViewRestore {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let restore_bytes = self
            .restore_bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = stdout.write_all(&restore_bytes);
        if self.physical_alternate_screen.swap(false, Ordering::AcqRel) {
            let _ = stdout.write_all(b"\x1b[?47l\x1b[?1047l\x1b[?1049l");
        }
        if std::thread::panicking() {
            let _ = execute!(stdout, DisableBracketedPaste, Show);
            let _ = stdout.flush();
            let _ = disable_raw_mode();
        } else {
            let _ = execute!(stdout, EnableBracketedPaste, Show);
            let _ = stdout.flush();
        }
    }
}

fn spawn_prompt(
    client: AgentLibreClient,
    session_id: SessionId,
    content: String,
    sender: mpsc::Sender<UiAsyncEvent>,
) {
    tokio::spawn(async move {
        let accepted = match client
            .submit_prompt(RunSubmitRequest {
                session_id: session_id.clone(),
                content: match agl_content::Content::text(content) {
                    Ok(content) => content,
                    Err(error) => {
                        let _ = sender.send(UiAsyncEvent::Notice(error.to_string())).await;
                        return;
                    }
                },
                client_submission_id: format!("cli-prompt-{}", agl_ids::RequestId::generate()),
                budget: RunBudgetRequest::default(),
            })
            .await
        {
            Ok(accepted) => accepted,
            Err(error) => {
                let _ = sender
                    .send(UiAsyncEvent::Notice(format!("prompt rejected: {error}")))
                    .await;
                return;
            }
        };
        let _ = sender
            .send(UiAsyncEvent::RunAccepted {
                session_id: session_id.clone(),
                run_id: accepted.run_id.clone(),
                state: accepted.state,
            })
            .await;
        let mut run = match client
            .subscribe_run(RunSubscribeRequest {
                run_id: accepted.run_id,
                after_sequence: 0,
            })
            .await
        {
            Ok(run) => run,
            Err(error) => {
                let _ = sender
                    .send(UiAsyncEvent::Notice(format!("run stream failed: {error}")))
                    .await;
                return;
            }
        };
        while let Ok(Some(event)) = run.next().await {
            if let RunSubscriptionEvent::Finished(finished) = event {
                if finished.state != ProtocolRunState::Succeeded {
                    let _ = sender
                        .send(UiAsyncEvent::Notice(format!(
                            "turn finished: {:?}",
                            finished.state
                        )))
                        .await;
                }
                break;
            }
        }
        match client
            .session_presentation(SessionPresentationRequest {
                session_id: session_id.clone(),
                page_cursor: None,
            })
            .await
        {
            Ok(snapshot) => {
                let _ = sender
                    .send(UiAsyncEvent::Snapshot {
                        session_id,
                        snapshot: Box::new(snapshot),
                    })
                    .await;
            }
            Err(error) => {
                let _ = sender
                    .send(UiAsyncEvent::Notice(format!("refresh failed: {error}")))
                    .await;
            }
        }
    });
}

async fn picker_suggestions(
    client: &AgentLibreClient,
    session_id: &SessionId,
    command_id: &str,
    argument_id: &str,
) -> Result<Vec<CommandSuggestion>> {
    let mut entries = Vec::new();
    let mut cursor = None;
    let mut seen_cursors = BTreeSet::new();
    for _ in 0..MAX_PICKER_PAGES {
        let page = client
            .command_suggestions(CommandSuggestionsRequest {
                session_id: Some(session_id.clone()),
                command_id: command_id.to_owned(),
                argument_id: argument_id.to_owned(),
                query: String::new(),
                cursor: cursor.clone(),
            })
            .await
            .with_context(|| format!("failed to load {argument_id} picker entries"))?;
        entries.extend(
            page.entries
                .into_iter()
                .take(MAX_PICKER_ENTRIES.saturating_sub(entries.len())),
        );
        if entries.len() >= MAX_PICKER_ENTRIES {
            break;
        }
        let Some(next_cursor) = page.next_cursor else {
            break;
        };
        if !seen_cursors.insert(next_cursor.clone()) {
            bail!("daemon repeated a picker pagination cursor");
        }
        cursor = Some(next_cursor);
    }
    Ok(entries)
}

async fn open_resume_picker(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
) -> Result<()> {
    let suggestions = picker_suggestions(client, session_id, "session.resume", "selector").await?;
    let mut entries = Vec::with_capacity(suggestions.len());
    for suggestion in suggestions {
        let candidate = SessionId::parse(&suggestion.value)
            .context("daemon returned an invalid session picker ID")?;
        let detail = if &candidate == session_id {
            Some(match suggestion.detail {
                Some(detail) => format!("current · {detail}"),
                None => "current session".to_owned(),
            })
        } else {
            suggestion.detail
        };
        entries.push(PickerEntry {
            value: suggestion.value,
            label: suggestion.label,
            detail,
            payload: PickerPayload::Resume(candidate),
        });
    }
    if entries.is_empty() {
        state.notice("no resumable sessions are available");
    } else {
        state.picker = Some(PickerState::new(
            PickerKind::Resume,
            "Resume session",
            entries,
        ));
    }
    Ok(())
}

async fn open_model_picker(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
) -> Result<()> {
    let suggestions = picker_suggestions(client, session_id, "model.select", "model_id").await?;
    let current = state.snapshot.header.model_id.as_deref();
    let entries = suggestions
        .into_iter()
        .map(|suggestion| PickerEntry {
            detail: if current == Some(suggestion.value.as_str()) {
                Some(match suggestion.detail {
                    Some(detail) => format!("current · {detail}"),
                    None => "current model".to_owned(),
                })
            } else {
                suggestion.detail
            },
            payload: PickerPayload::Model(suggestion.value.clone()),
            value: suggestion.value,
            label: suggestion.label,
        })
        .collect::<Vec<_>>();
    if entries.is_empty() {
        state.notice("no installed compatible models are available");
    } else {
        let mut picker = PickerState::new(PickerKind::Model, "Select model", entries);
        if let Some(current) = current {
            picker.select_value(current);
        }
        state.picker = Some(picker);
    }
    Ok(())
}

fn open_mode_picker(state: &mut UiState) {
    let current = state.snapshot.header.operation_mode;
    let entries = operation_mode_picker_entries(current);
    let mut picker = PickerState::new(PickerKind::Mode, "Select operation mode", entries);
    if let Some(current) = picker
        .entries
        .iter()
        .find(|entry| entry.detail.as_deref() == Some("current mode"))
        .map(|entry| entry.value.clone())
    {
        picker.select_value(&current);
    }
    state.picker = Some(picker);
}

fn operation_mode_picker_entries(current: ProtocolToolMode) -> Vec<PickerEntry> {
    [
        ("read-only", ProtocolToolMode::ReadOnly),
        ("write", ProtocolToolMode::Write),
        ("execute", ProtocolToolMode::Execute),
        ("approve", ProtocolToolMode::Approve),
        ("admin", ProtocolToolMode::Admin),
    ]
    .into_iter()
    .map(|(value, mode)| PickerEntry {
        value: value.to_owned(),
        label: value.to_owned(),
        detail: (mode == current).then(|| "current mode".to_owned()),
        payload: PickerPayload::Mode(mode),
    })
    .collect()
}

async fn open_skills_picker(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
) -> Result<()> {
    let suggestions = picker_suggestions(client, session_id, "skills.select", "skill_id").await?;
    let mut seen = BTreeSet::new();
    let mut entries = suggestions
        .into_iter()
        .map(|suggestion| {
            seen.insert(suggestion.value.clone());
            PickerEntry {
                payload: PickerPayload::Skill(suggestion.value.clone()),
                value: suggestion.value,
                label: suggestion.label,
                detail: suggestion.detail,
            }
        })
        .collect::<Vec<_>>();
    for selected in &state.snapshot.header.selected_skills {
        if seen.insert(selected.clone()) {
            entries.push(PickerEntry {
                value: selected.clone(),
                label: selected.clone(),
                detail: Some("currently selected; not in the admitted suggestion set".to_owned()),
                payload: PickerPayload::Skill(selected.clone()),
            });
        }
    }
    let mut picker = PickerState::new(PickerKind::Skills, "Select skills", entries);
    picker.selected_values = state
        .snapshot
        .header
        .selected_skills
        .iter()
        .cloned()
        .collect();
    if let Some(selected) = state.snapshot.header.selected_skills.first() {
        picker.select_value(selected);
    }
    state.picker = Some(picker);
    Ok(())
}

async fn open_process_picker(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
    include_finished: bool,
) -> Result<()> {
    let terminals = client
        .application_action(ApplicationActionRequest {
            session_id: Some(session_id.clone()),
            client_submission_id: format!("cli-terminals-{}", agl_ids::RequestId::generate()),
            action: ApplicationAction::TerminalList { include_finished },
        })
        .await
        .context("failed to load terminal picker entries")?;
    let ApplicationActionResult::Terminals { terminals } = terminals.result else {
        bail!("daemon returned an invalid terminal-list result");
    };
    let executions = client
        .application_action(ApplicationActionRequest {
            session_id: Some(session_id.clone()),
            client_submission_id: format!("cli-executions-{}", agl_ids::RequestId::generate()),
            action: ApplicationAction::ExecutionList { include_finished },
        })
        .await
        .context("failed to load execution picker entries")?;
    let ApplicationActionResult::Executions { executions } = executions.result else {
        bail!("daemon returned an invalid execution-list result");
    };

    let terminals_by_execution = terminals
        .into_iter()
        .map(|terminal| (terminal.execution_id.clone(), terminal))
        .collect::<BTreeMap<_, _>>();
    let mut processes = executions
        .into_iter()
        .map(|execution| {
            let terminal = terminals_by_execution.get(&execution.execution_id).cloned();
            process_picker_item(execution, terminal)
        })
        .collect::<BTreeMap<_, _>>();
    for (execution_id, terminal) in terminals_by_execution {
        processes
            .entry(execution_id.clone())
            .or_insert_with(|| ProcessPickerItem {
                execution_id: terminal.execution_id.clone(),
                state: terminal.process_state,
                profile: terminal.profile,
                cwd: terminal.cwd.clone(),
                terminal: Some(terminal),
            });
    }
    let mut entries = host_terminal_picker_entries();
    let remaining_entries = MAX_PICKER_ENTRIES.saturating_sub(entries.len());
    entries.extend(
        processes
            .into_values()
            .take(remaining_entries)
            .map(process_picker_entry),
    );
    let mut picker = PickerState::new(
        PickerKind::Processes,
        if include_finished {
            "Processes · live and finished"
        } else {
            "Processes · live"
        },
        entries,
    );
    if let Some(execution_id) = state.last_terminal.as_ref().and_then(|terminal_id| {
        picker.entries.iter().find_map(|entry| {
            let PickerPayload::Process(process) = &entry.payload else {
                return None;
            };
            process
                .terminal
                .as_ref()
                .filter(|terminal| &terminal.terminal_id == terminal_id)
                .map(|_| entry.value.clone())
        })
    }) {
        picker.select_value(&execution_id);
    }
    state.picker = Some(picker);
    Ok(())
}

fn host_terminal_picker_entries() -> Vec<PickerEntry> {
    vec![
        PickerEntry {
            value: "action:host-terminal:managed".to_owned(),
            label: "Open HOST terminal".to_owned(),
            detail: Some(
                "managed startup · recommended · explicit Host authority confirmation".to_owned(),
            ),
            payload: PickerPayload::EnsureHost(HostStartupPolicy::ManagedOnly),
        },
        PickerEntry {
            value: "action:host-terminal:user-rc".to_owned(),
            label: "Open HOST terminal + user rc".to_owned(),
            detail: Some(
                "source normal shell rc · separate Host authority confirmation".to_owned(),
            ),
            payload: PickerPayload::EnsureHost(HostStartupPolicy::SourceUserRc),
        },
    ]
}

fn process_picker_item(
    execution: ExecutionView,
    terminal: Option<TerminalSessionView>,
) -> (ExecutionId, ProcessPickerItem) {
    let execution_id = execution.execution_id;
    (
        execution_id.clone(),
        ProcessPickerItem {
            execution_id,
            state: execution.state,
            profile: execution.profile,
            cwd: execution.cwd,
            terminal,
        },
    )
}

fn process_picker_entry(process: ProcessPickerItem) -> PickerEntry {
    let (label, detail) = if let Some(terminal) = &process.terminal {
        let authority = terminal_authority_label(terminal.profile);
        (
            format!("terminal {}", terminal.terminal_id),
            format!(
                "{} · {authority} · {:?} · writer:{:?} · cwd:{}",
                terminal_owner_label(&terminal.owner),
                terminal.process_state,
                terminal.writer,
                terminal.cwd,
            ),
        )
    } else {
        (
            format!("process {}", process.execution_id),
            format!(
                "{:?} · {:?} · cwd:{}",
                process.profile, process.state, process.cwd
            ),
        )
    };
    PickerEntry {
        value: process.execution_id.to_string(),
        label,
        detail: Some(detail),
        payload: PickerPayload::Process(Box::new(process)),
    }
}

async fn handle_command(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
    command: &str,
) -> Result<CommandOutcome> {
    let words = lex_command(command)?;
    let mut parts = words.into_iter();
    let invoked_name = parts.next().unwrap_or_default();
    let descriptor = state
        .catalog
        .iter()
        .find(|descriptor| {
            descriptor.name == invoked_name
                || descriptor
                    .aliases
                    .iter()
                    .any(|alias| alias == &invoked_name)
        })
        .map(|descriptor| (descriptor.name.clone(), descriptor.availability.clone()));
    let name = match descriptor {
        Some((_, CommandAvailability::Disabled { message, .. })) => {
            state.notice(format!("/{invoked_name} is unavailable: {message}"));
            return Ok(CommandOutcome::Continue);
        }
        Some((_, CommandAvailability::Hidden)) => {
            state.notice(format!("unknown command /{invoked_name}"));
            return Ok(CommandOutcome::Continue);
        }
        Some((name, CommandAvailability::Enabled)) => name,
        None => invoked_name,
    };
    let mut workspace_candidate = None;
    match name.as_str() {
        "disconnect" => return Ok(CommandOutcome::Disconnect),
        "help" => {
            state.notice("Use ↑/↓ in Command mode; Enter invokes the selected command");
            return Ok(CommandOutcome::Continue);
        }
        "exit"
            if !state.exit_armed
                && (state.snapshot.header.active_run_count > 0
                    || state.snapshot.header.queued_prompt_count > 0
                    || state.snapshot.header.active_execution_count > 0) =>
        {
            state.exit_armed = true;
            state
                .notice("Active work exists. Run /exit again to cancel it and finish the session.");
            return Ok(CommandOutcome::Continue);
        }
        _ => {}
    }
    let mut attach_candidate = None;
    let action = match name.as_str() {
        "status" => ApplicationAction::SessionStatus,
        "workspace" => match parts.next() {
            Some(path) => {
                let path = std::iter::once(path)
                    .chain(parts)
                    .collect::<Vec<_>>()
                    .join(" ");
                workspace_candidate = Some(path.clone());
                ApplicationAction::WorkspaceSet {
                    confirm_terminate_terminals: state.workspace_change_armed.as_deref()
                        == Some(path.as_str()),
                    path,
                }
            }
            None => ApplicationAction::WorkspaceGet,
        },
        "processes" => {
            let include_finished = match parts.next().as_deref() {
                None => false,
                Some("--all") => true,
                Some(_) => {
                    state.notice("usage: /processes [--all]");
                    return Ok(CommandOutcome::Continue);
                }
            };
            if parts.next().is_some() {
                state.notice("usage: /processes [--all]");
                return Ok(CommandOutcome::Continue);
            }
            if let Err(error) =
                open_process_picker(client, session_id, state, include_finished).await
            {
                state.notice(format!("process picker failed: {error:#}"));
            }
            return Ok(CommandOutcome::Continue);
        }
        "kill" => {
            let id = parts.next().context("/kill requires EXECUTION_ID")?;
            ApplicationAction::ExecutionKill {
                execution_id: ExecutionId::parse(&id).context("invalid execution ID")?,
                mode: if matches!(parts.next().as_deref(), Some("--immediate")) {
                    agl_protocol::KillMode::Immediate
                } else {
                    agl_protocol::KillMode::Graceful
                },
            }
        }
        "reload" => ApplicationAction::RuntimeContextReload,
        "clear" => ApplicationAction::SessionClear,
        "exit" => ApplicationAction::SessionExit {
            confirm_active: state.exit_armed,
        },
        "attach" => {
            let id = parts.next().context("/attach requires EXECUTION_ID")?;
            let execution_id = ExecutionId::parse(&id).context("invalid execution ID")?;
            let Some(candidate) = state
                .snapshot
                .terminals
                .iter()
                .find(|terminal| terminal.execution_id == execution_id)
                .cloned()
            else {
                state.notice("That execution is not an interactive terminal");
                return Ok(CommandOutcome::Continue);
            };
            let read_only = !matches!(candidate.owner, TerminalOwnerView::Human { .. })
                || matches!(parts.next().as_deref(), Some("--read-only"));
            attach_candidate = Some((candidate, !read_only));
            ApplicationAction::ExecutionAttach {
                execution_id,
                read_only,
            }
        }
        "new" => ApplicationAction::SessionNew {
            launch: SessionLaunchOptions {
                workspace_root: Some(state.snapshot.header.workspace_root.clone()),
                function_ref: None,
                model_id: state.snapshot.header.model_id.clone(),
                operation_mode: Some(state.snapshot.header.operation_mode),
                skill_ids: state.snapshot.header.selected_skills.clone(),
            },
        },
        "resume" => {
            let Some(selector) = parts.next() else {
                if let Err(error) = open_resume_picker(client, session_id, state).await {
                    state.notice(format!("session picker failed: {error:#}"));
                }
                return Ok(CommandOutcome::Continue);
            };
            if parts.next().is_some() {
                state.notice("usage: /resume [latest|SESSION_ID]");
                return Ok(CommandOutcome::Continue);
            }
            let selector = match selector.as_str() {
                "latest" => SessionSelector::Latest,
                value => SessionSelector::Id {
                    session_id: SessionId::parse(value).context("invalid session ID")?,
                },
            };
            ApplicationAction::SessionResume { selector }
        }
        "model" => {
            let Some(model_id) = parts.next() else {
                if let Err(error) = open_model_picker(client, session_id, state).await {
                    state.notice(format!("model picker failed: {error:#}"));
                }
                return Ok(CommandOutcome::Continue);
            };
            if parts.next().is_some() {
                state.notice("usage: /model [MODEL_ID]");
                return Ok(CommandOutcome::Continue);
            }
            ApplicationAction::ModelSelect { model_id }
        }
        "mode" => {
            let Some(mode) = parts.next() else {
                open_mode_picker(state);
                return Ok(CommandOutcome::Continue);
            };
            if parts.next().is_some() {
                state.notice("usage: /mode [read-only|write|execute|approve|admin]");
                return Ok(CommandOutcome::Continue);
            }
            ApplicationAction::OperationModeSelect {
                mode: parse_protocol_tool_mode(&mode)?,
            }
        }
        "skills" => {
            let skill_ids = parts.collect::<Vec<_>>();
            if skill_ids.is_empty() {
                if let Err(error) = open_skills_picker(client, session_id, state).await {
                    state.notice(format!("skills picker failed: {error:#}"));
                }
                return Ok(CommandOutcome::Continue);
            }
            ApplicationAction::SkillsSelect { skill_ids }
        }
        _ => {
            state.notice(format!("unknown command /{name}"));
            return Ok(CommandOutcome::Continue);
        }
    };
    let response = client
        .application_action(ApplicationActionRequest {
            session_id: Some(session_id.clone()),
            client_submission_id: format!("cli-action-{}", agl_ids::RequestId::generate()),
            action,
        })
        .await;
    match response {
        Ok(event) => match event.result {
            ApplicationActionResult::SessionOpened { session_id, .. } => {
                return Ok(CommandOutcome::SwitchSession { session_id });
            }
            ApplicationActionResult::SessionExited { .. } => {
                return Ok(CommandOutcome::Disconnect);
            }
            ApplicationActionResult::Status { header } => {
                let notice = match name.as_str() {
                    "workspace" => Some(header.workspace_root.clone()),
                    _ => None,
                };
                state.snapshot.header = header;
                if let Some(notice) = notice {
                    state.notice(notice);
                }
            }
            ApplicationActionResult::WorkspaceChanged { header } => {
                state.snapshot.header = header;
                state.workspace_change_armed = None;
            }
            ApplicationActionResult::ModelChanged { header }
            | ApplicationActionResult::ModeChanged { header }
            | ApplicationActionResult::SkillsChanged { header } => {
                state.snapshot.header = header;
            }
            ApplicationActionResult::Executions { executions } => {
                let summary = executions
                    .iter()
                    .take(4)
                    .map(|execution| {
                        let terminal = state
                            .snapshot
                            .terminals
                            .iter()
                            .find(|terminal| terminal.execution_id == execution.execution_id)
                            .map(|terminal| format!("terminal:{}", terminal.terminal_id))
                            .unwrap_or_else(|| "process".to_owned());
                        format!(
                            "{} {terminal} {:?}",
                            execution.execution_id, execution.state
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" · ");
                state.notice(if summary.is_empty() {
                    "no executions".to_owned()
                } else {
                    summary
                });
            }
            ApplicationActionResult::AttachAccepted { .. } => {
                let (terminal, writable) = attach_candidate
                    .context("daemon accepted an attachment for an unknown terminal")?;
                state.last_terminal = Some(terminal.terminal_id.clone());
                return Ok(CommandOutcome::EnterTerminal(Box::new(
                    TerminalViewRequest { terminal, writable },
                )));
            }
            ApplicationActionResult::Cleared { .. } => {
                state.snapshot.items.clear();
                state.assistant_deltas.clear();
                state.notice("conversation context cleared");
            }
            _ => state.notice(format!("/{name} completed")),
        },
        Err(ClientError::Protocol {
            code: agl_protocol::ProtocolErrorCode::ConfirmationRequired,
            ..
        }) if name == "workspace" => {
            state.workspace_change_armed = workspace_candidate;
            state.notice(
                "Workspace change will terminate terminals tied to the current root. Run the same /workspace command again to confirm.",
            );
        }
        Err(error) => state.notice(format!("/{name} failed: {error}")),
    }
    Ok(CommandOutcome::Continue)
}

fn lex_command(input: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for character in input.chars() {
        if escaped {
            word.push(character);
            escaped = false;
            started = true;
            continue;
        }
        if character == '\\' {
            escaped = true;
            started = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if character == delimiter {
                quote = None;
            } else {
                word.push(character);
            }
            started = true;
            continue;
        }
        match character {
            '\'' | '"' => {
                quote = Some(character);
                started = true;
            }
            character if character.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            _ => {
                word.push(character);
                started = true;
            }
        }
    }
    if escaped {
        bail!("command ends with an incomplete escape");
    }
    if quote.is_some() {
        bail!("command contains an unterminated quote");
    }
    if started {
        words.push(word);
    }
    Ok(words)
}

fn parse_protocol_tool_mode(value: &str) -> Result<ProtocolToolMode> {
    match value {
        "read-only" => Ok(ProtocolToolMode::ReadOnly),
        "write" => Ok(ProtocolToolMode::Write),
        "execute" => Ok(ProtocolToolMode::Execute),
        "approve" => Ok(ProtocolToolMode::Approve),
        "admin" => Ok(ProtocolToolMode::Admin),
        _ => bail!(
            "invalid operation mode `{value}`; expected read-only, write, execute, approve, or admin"
        ),
    }
}

fn apply_async_event(state: &mut UiState, session_id: &SessionId, event: UiAsyncEvent) {
    match event {
        UiAsyncEvent::RunAccepted {
            session_id: event_session_id,
            run_id,
            state: ProtocolRunState::Running,
        } if &event_session_id == session_id => state.active_run = Some(run_id),
        UiAsyncEvent::Snapshot {
            session_id: event_session_id,
            snapshot,
        } if &event_session_id == session_id => {
            state.active_run = snapshot
                .active_run
                .as_ref()
                .map(|active| active.run_id.clone());
            state.snapshot = *snapshot;
            state.assistant_deltas.clear();
        }
        UiAsyncEvent::RunAccepted { .. } | UiAsyncEvent::Snapshot { .. } => {}
        UiAsyncEvent::Notice(message) => state.notice(message),
    }
}

fn apply_presentation_event(
    state: &mut UiState,
    event: agl_protocol::SessionPresentationEventPayload,
) -> bool {
    let command_catalog_changed = matches!(
        &event,
        agl_protocol::SessionPresentationEventPayload::CommandAvailabilityChanged
    );
    match event {
        agl_protocol::SessionPresentationEventPayload::HeaderChanged { header } => {
            state.snapshot.header = header
        }
        agl_protocol::SessionPresentationEventPayload::ItemUpsert { item } => {
            if let SessionPresentationItem::AssistantMessage { message_id, .. } = &item {
                state.assistant_deltas.remove(message_id);
            }
            let key = presentation_item_key(&item);
            if let Some(existing) = state
                .snapshot
                .items
                .iter_mut()
                .find(|existing| presentation_item_key(existing) == key)
            {
                *existing = item;
            } else {
                state.snapshot.items.push(item);
            }
        }
        agl_protocol::SessionPresentationEventPayload::ItemRemoved { item_key } => {
            state
                .snapshot
                .items
                .retain(|item| presentation_item_key(item) != item_key);
            state
                .assistant_deltas
                .retain(|message_id, _| message_id.to_string() != item_key);
        }
        agl_protocol::SessionPresentationEventPayload::AssistantTextDelta {
            run_id,
            provisional_message_id,
            sequence,
            text,
            ..
        } => {
            match append_assistant_delta(
                &mut state.assistant_deltas,
                run_id,
                provisional_message_id,
                sequence,
                &text,
            ) {
                AssistantDeltaApply::SequenceGap => state.notice(
                    "assistant presentation delta gap; waiting for the durable final message",
                ),
                AssistantDeltaApply::BoundExceeded => state.notice(
                    "assistant presentation delta exceeded its private display bound; waiting for the durable final message",
                ),
                AssistantDeltaApply::Applied | AssistantDeltaApply::Duplicate => {}
            }
        }
        agl_protocol::SessionPresentationEventPayload::PromptQueued { prompt } => {
            if let Some(existing) = state
                .snapshot
                .queued_prompts
                .iter_mut()
                .find(|existing| existing.run_id == prompt.run_id)
            {
                *existing = prompt;
            } else {
                state.snapshot.queued_prompts.push(prompt);
            }
            sync_ui_prompt_counts(&mut state.snapshot);
        }
        agl_protocol::SessionPresentationEventPayload::PromptActivated { run_id } => {
            state
                .snapshot
                .queued_prompts
                .retain(|prompt| prompt.run_id != run_id);
            if state
                .snapshot
                .active_run
                .as_ref()
                .is_none_or(|active| active.run_id != run_id)
            {
                state.snapshot.active_run = Some(ActiveRunView {
                    run_id: run_id.clone(),
                    turn_id: None,
                    state: "running".to_owned(),
                });
            }
            state.active_run = Some(run_id);
            sync_ui_prompt_counts(&mut state.snapshot);
        }
        agl_protocol::SessionPresentationEventPayload::PromptFinished { run_id, .. } => {
            state
                .assistant_deltas
                .retain(|_, delta| delta.run_id != run_id);
            if state.active_run.as_ref() == Some(&run_id) {
                state.active_run = None;
            }
            if state
                .snapshot
                .active_run
                .as_ref()
                .is_some_and(|active| active.run_id == run_id)
            {
                state.snapshot.active_run = None;
            }
            state
                .snapshot
                .queued_prompts
                .retain(|prompt| prompt.run_id != run_id);
            sync_ui_prompt_counts(&mut state.snapshot);
        }
        agl_protocol::SessionPresentationEventPayload::TerminalAdded { terminal }
        | agl_protocol::SessionPresentationEventPayload::TerminalChanged { terminal } => {
            if let Some(existing) = state
                .snapshot
                .terminals
                .iter_mut()
                .find(|existing| existing.terminal_id == terminal.terminal_id)
            {
                *existing = terminal;
            } else {
                state.snapshot.terminals.push(terminal);
            }
        }
        agl_protocol::SessionPresentationEventPayload::TerminalRemoved { terminal_id } => {
            state
                .snapshot
                .terminals
                .retain(|terminal| terminal.terminal_id != terminal_id);
        }
        agl_protocol::SessionPresentationEventPayload::ExecutionStateChanged { execution } => {
            if let Some(existing) = state
                .snapshot
                .executions
                .iter_mut()
                .find(|existing| existing.execution_id == execution.execution_id)
            {
                *existing = execution;
            } else {
                state.snapshot.executions.push(execution);
            }
        }
        agl_protocol::SessionPresentationEventPayload::Notice { message, .. } => {
            state.notice(message)
        }
        _ => {}
    }
    command_catalog_changed
}

fn sync_ui_prompt_counts(snapshot: &mut SessionPresentationSnapshot) {
    snapshot.header.active_run_count = u32::from(snapshot.active_run.is_some());
    snapshot.header.queued_prompt_count =
        u32::try_from(snapshot.queued_prompts.len()).unwrap_or(u32::MAX);
    snapshot.command_context.active_or_queued_turns = snapshot
        .header
        .active_run_count
        .saturating_add(snapshot.header.queued_prompt_count);
}

fn append_assistant_delta(
    deltas: &mut BTreeMap<MessageId, AssistantDeltaState>,
    run_id: RunId,
    message_id: MessageId,
    sequence: u64,
    text: &str,
) -> AssistantDeltaApply {
    if !deltas.contains_key(&message_id) && deltas.len() >= MAX_LIVE_ASSISTANT_DELTAS {
        return AssistantDeltaApply::BoundExceeded;
    }
    let delta = deltas
        .entry(message_id)
        .or_insert_with(|| AssistantDeltaState {
            run_id: run_id.clone(),
            next_sequence: 1,
            text: String::new(),
            valid: true,
        });
    if !delta.valid {
        return AssistantDeltaApply::Duplicate;
    }
    if delta.run_id != run_id || sequence > delta.next_sequence {
        delta.valid = false;
        delta.text.clear();
        return AssistantDeltaApply::SequenceGap;
    }
    if sequence < delta.next_sequence {
        return AssistantDeltaApply::Duplicate;
    }
    if delta.text.len().saturating_add(text.len()) > MAX_LIVE_ASSISTANT_DELTA_BYTES {
        delta.valid = false;
        delta.text.clear();
        return AssistantDeltaApply::BoundExceeded;
    }
    delta.text.push_str(text);
    delta.next_sequence = delta.next_sequence.saturating_add(1);
    AssistantDeltaApply::Applied
}

fn presentation_item_key(item: &SessionPresentationItem) -> String {
    match item {
        SessionPresentationItem::UserMessage { message_id, .. }
        | SessionPresentationItem::AssistantMessage { message_id, .. } => message_id.to_string(),
        SessionPresentationItem::AgentAction {
            run_id, step_id, ..
        } => format!("{run_id}:{step_id}"),
        SessionPresentationItem::ContextBoundary { event_id, .. }
        | SessionPresentationItem::Notice { event_id, .. } => event_id.to_string(),
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, state: &UiState) {
    let palette_height = if state.composer.mode == ComposerMode::Command {
        state.matching_commands().len().min(6) as u16 + 2
    } else {
        0
    };
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(4),
            Constraint::Length(palette_height),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());
    draw_header(frame, layout[0], state);
    draw_transcript(frame, layout[1], state);
    if palette_height > 0 {
        draw_palette(frame, layout[2], state);
    }
    draw_composer(frame, layout[3], state);
    frame.render_widget(
        Paragraph::new(
            "Enter submit  exact ! opens Terminal  Shift+Enter newline  Ctrl+D disconnect",
        )
        .style(Style::default().fg(Color::DarkGray)),
        layout[4],
    );
    if let Some(picker) = &state.picker {
        draw_picker(frame, picker);
    }
}

fn draw_picker(frame: &mut ratatui::Frame<'_>, picker: &PickerState) {
    let frame_area = frame.area();
    let width = frame_area.width.saturating_sub(2).clamp(1, 110);
    let height = frame_area.height.saturating_sub(2).clamp(1, 24);
    let area = Rect::new(
        frame_area.x + frame_area.width.saturating_sub(width) / 2,
        frame_area.y + frame_area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let title = if matches!(&picker.kind, PickerKind::Skills) {
        format!(
            " {} · {} selected ",
            picker.title,
            picker.selected_values.len()
        )
    } else {
        format!(" {} ", picker.title)
    };
    let query_prefix = "filter: ";
    let mut lines = vec![Line::from(vec![
        Span::styled(query_prefix, Style::default().fg(Color::DarkGray)),
        Span::raw(picker.query.clone()),
    ])];
    let inner_height = area.height.saturating_sub(2) as usize;

    if let Some(confirmation) = &picker.confirmation {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            confirmation.prompt.clone(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::styled(
            "Enter confirm  Esc cancel",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        let filtered = picker.filtered_indices();
        let visible_entries = inner_height.saturating_sub(2).max(1);
        let selected = picker.selected.min(filtered.len().saturating_sub(1));
        let first = selected
            .saturating_add(1)
            .saturating_sub(visible_entries)
            .min(filtered.len().saturating_sub(visible_entries));
        if filtered.is_empty() {
            lines.push(Line::styled(
                "no matching entries",
                Style::default().fg(Color::DarkGray),
            ));
        } else {
            for (rank, entry_index) in filtered
                .iter()
                .enumerate()
                .skip(first)
                .take(visible_entries)
            {
                let entry = &picker.entries[*entry_index];
                let selection = match &entry.payload {
                    PickerPayload::Skill(skill_id) => {
                        if picker.selected_values.contains(skill_id) {
                            "[x] "
                        } else {
                            "[ ] "
                        }
                    }
                    _ => "",
                };
                let detail = entry
                    .detail
                    .as_deref()
                    .map(|detail| format!(" · {detail}"))
                    .unwrap_or_default();
                let style = if rank == selected {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default()
                };
                lines.push(Line::styled(
                    format!("{selection}{}{detail}", entry.label),
                    style,
                ));
            }
        }
        lines.push(Line::styled(
            picker_help(&picker.kind),
            Style::default().fg(Color::DarkGray),
        ));
    }

    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
    let max_cursor_x = area.right().saturating_sub(1).max(area.x);
    let cursor_x = area
        .x
        .saturating_add(1)
        .saturating_add(query_prefix.len() as u16)
        .saturating_add(picker.query.graphemes(true).count() as u16)
        .min(max_cursor_x);
    frame.set_cursor_position((cursor_x, area.y.saturating_add(1)));
}

fn picker_help(kind: &PickerKind) -> &'static str {
    match kind {
        PickerKind::Skills => "Space toggle  Ctrl+A all  Ctrl+U none  Enter apply  Esc close",
        PickerKind::Processes => {
            "Enter attach/action (HOST confirms)  Ctrl+R read-only  Ctrl+W writer  Ctrl+K stop  Ctrl+Shift+K kill  Ctrl+P promote  Esc"
        }
        PickerKind::Resume | PickerKind::Model | PickerKind::Mode => {
            "Type to filter  Enter select  Esc close"
        }
    }
}

fn draw_header(frame: &mut ratatui::Frame<'_>, area: Rect, state: &UiState) {
    let header = &state.snapshot.header;
    let model = header.model_id.as_deref().unwrap_or("local");
    let status = if state.active_run.is_some() {
        "thinking"
    } else {
        "ready"
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("agentLIBRE", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(format!(
                    "  {status}  model:{model}  mode:{:?}",
                    header.operation_mode
                )),
            ]),
            Line::from(format!(
                "{}  cwd:{}  session:{}",
                workspace_label(&header.workspace_root),
                header.cwd,
                header.session_id
            )),
        ]),
        area,
    );
}

fn draw_transcript(frame: &mut ratatui::Frame<'_>, area: Rect, state: &UiState) {
    let mut lines = Vec::new();
    for item in &state.snapshot.items {
        match item {
            SessionPresentationItem::UserMessage { content, .. } => {
                lines.push(Line::styled("you", Style::default().fg(Color::Cyan)));
                lines.extend(text_lines(content_text(content)));
            }
            SessionPresentationItem::AssistantMessage { content, .. } => {
                lines.push(Line::styled(
                    "agentLIBRE",
                    Style::default().fg(Color::Green),
                ));
                lines.extend(text_lines(content_text(content)));
            }
            SessionPresentationItem::ContextBoundary { .. } => lines.push(Line::styled(
                "──────── context cleared ────────",
                Style::default().fg(Color::DarkGray),
            )),
            SessionPresentationItem::Notice { message, .. } => lines.push(Line::styled(
                message.clone(),
                Style::default().fg(Color::Yellow),
            )),
            SessionPresentationItem::AgentAction { summary, state, .. } => {
                lines.push(Line::styled(
                    format!("agent action · {summary} · {state:?}"),
                    Style::default().fg(Color::Magenta),
                ))
            }
        }
        lines.push(Line::raw(""));
    }
    for terminal in &state.snapshot.terminals {
        let authority = terminal_authority_label(terminal.profile);
        lines.push(Line::styled(
            format!(
                "! {} · {authority} · cwd:{} · {:?} · {:?}",
                terminal_owner_label(&terminal.owner),
                terminal.cwd,
                terminal.prompt_state,
                terminal.process_state,
            ),
            if terminal.profile == ExecutionProfile::Host {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Magenta)
            },
        ));
    }
    for delta in state.assistant_deltas.values().filter(|delta| delta.valid) {
        lines.push(Line::styled(
            "agentLIBRE · streaming",
            Style::default().fg(Color::Green),
        ));
        lines.extend(text_lines(delta.text.clone()));
        lines.push(Line::raw(""));
    }
    for notice in &state.notices {
        lines.push(Line::styled(
            format!("· {notice}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let scroll = paragraph
        .line_count(area.width)
        .saturating_sub(area.height as usize)
        .min(u16::MAX as usize) as u16;
    frame.render_widget(paragraph.scroll((scroll, 0)), area);
}

fn draw_palette(frame: &mut ratatui::Frame<'_>, area: Rect, state: &UiState) {
    let commands = state.matching_commands();
    let selected = state
        .composer
        .selected_command
        .min(commands.len().saturating_sub(1));
    let lines = commands
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let availability = match &command.availability {
                CommandAvailability::Enabled => "",
                CommandAvailability::Disabled { message, .. } => message,
                CommandAvailability::Hidden => "hidden",
            };
            let style = if index == selected {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else if matches!(command.availability, CommandAvailability::Disabled { .. }) {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            Line::styled(
                format!(
                    "/{:<12} {:<38} {}",
                    command.name, command.summary, availability
                ),
                style,
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Commands ")),
        area,
    );
}

fn draw_composer(frame: &mut ratatui::Frame<'_>, area: Rect, state: &UiState) {
    let mode_style = match state.composer.mode {
        ComposerMode::Prompt => Style::default().fg(Color::Cyan),
        ComposerMode::Command => Style::default().fg(Color::Blue),
    };
    let paragraph = Paragraph::new(state.composer.buffer.as_str()).block(
        Block::default().borders(Borders::ALL).title(Span::styled(
            format!(" {} ", state.composer.label()),
            mode_style,
        )),
    );
    frame.render_widget(paragraph, area);
    let prefix = &state.composer.buffer[..state.composer.cursor];
    let cursor_x = area.x + 1 + prefix.graphemes(true).count() as u16;
    frame.set_cursor_position((cursor_x.min(area.right().saturating_sub(1)), area.y + 1));
}

fn text_lines(text: String) -> Vec<Line<'static>> {
    text.lines()
        .map(|line| Line::raw(line.to_owned()))
        .collect()
}

fn content_text(content: &agl_content::Content) -> String {
    content
        .clone()
        .text_only()
        .unwrap_or_else(|| "[multimodal content]".to_owned())
}

fn workspace_label(workspace: &str) -> &str {
    Path::new(workspace)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(workspace)
}

fn managed_shell_profile_id(program: &Path) -> Option<&'static str> {
    match program.file_name()?.to_str()? {
        "bash" => Some("bash-managed"),
        "zsh" => Some("zsh-managed"),
        _ => None,
    }
}

fn terminal_owner_label(owner: &TerminalOwnerView) -> String {
    match owner {
        TerminalOwnerView::Human { .. } => "Human".to_owned(),
        TerminalOwnerView::MainAgent { .. } => "main agent".to_owned(),
        TerminalOwnerView::Subagent { owner_run_id, .. } => format!("subagent {owner_run_id}"),
        TerminalOwnerView::SessionPromoted {
            previous_owner_run_id,
            ..
        } => format!("promoted {previous_owner_run_id}"),
    }
}

fn terminal_authority_label(profile: ExecutionProfile) -> &'static str {
    match profile {
        ExecutionProfile::Workspace => "workspace",
        ExecutionProfile::Host => "HOST",
    }
}

fn restore_physical_terminal() {
    let mut stdout = io::stdout();
    let _ = stdout.write_all(b"\x1b[0m");
    let _ = execute!(stdout, DisableBracketedPaste, Show);
    let _ = stdout.flush();
    let _ = disable_raw_mode();
}

struct TuiTerminalMode {
    active: Arc<AtomicBool>,
}

impl TuiTerminalMode {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable terminal raw mode")?;
        if let Err(error) = execute!(io::stdout(), EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(error).context("failed to enable bracketed paste");
        }
        let active = Arc::new(AtomicBool::new(true));
        let hook_active = Arc::clone(&active);
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic| {
            if hook_active.swap(false, Ordering::AcqRel) {
                restore_physical_terminal();
            }
            previous(panic);
        }));
        Ok(Self { active })
    }

    fn suspend(&mut self) {
        if self.active.swap(false, Ordering::AcqRel) {
            restore_physical_terminal();
        }
    }

    fn resume(&mut self) -> Result<()> {
        enable_raw_mode().context("failed to restore terminal raw mode after SIGCONT")?;
        if let Err(error) = execute!(io::stdout(), EnableBracketedPaste, Show) {
            restore_physical_terminal();
            return Err(error).context("failed to restore terminal modes after SIGCONT");
        }
        self.active.store(true, Ordering::Release);
        Ok(())
    }
}

impl Drop for TuiTerminalMode {
    fn drop(&mut self) {
        if self.active.swap(false, Ordering::AcqRel) {
            restore_physical_terminal();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    const PANIC_GUARD_CHILD_ENV: &str = "AGL_INTERNAL_TUI_PANIC_GUARD_CHILD";

    #[cfg(target_os = "linux")]
    #[test]
    fn panic_hook_restores_parent_terminal_in_native_pty() {
        if std::env::var_os(PANIC_GUARD_CHILD_ENV).is_some() {
            let _terminal_mode = TuiTerminalMode::enter().unwrap();
            io::stdout().write_all(b"AGL_TUI_PANIC_READY\n").unwrap();
            io::stdout().flush().unwrap();
            let mut trigger = [0_u8; 1];
            io::stdin().read_exact(&mut trigger).unwrap();
            panic!("intentional TUI terminal-guard panic fixture");
        }

        let mut fixture = PanicGuardParentTerminal::spawn();
        fixture.wait_for(b"AGL_TUI_PANIC_READY");
        fixture.assert_raw();
        fixture.write(b"x");
        let status = fixture.finish();
        assert!(!status.success(), "induced panic unexpectedly succeeded");
        fixture.assert_restored();
        let enable = fixture
            .output
            .windows(b"\x1b[?2004h".len())
            .position(|candidate| candidate == b"\x1b[?2004h")
            .expect("panic fixture never enabled bracketed paste");
        let disable = fixture
            .output
            .windows(b"\x1b[?2004l".len())
            .rposition(|candidate| candidate == b"\x1b[?2004l")
            .expect("panic hook never disabled bracketed paste");
        assert!(disable > enable);
        assert!(
            fixture.output[disable..]
                .windows(b"\x1b[?25h".len())
                .any(|candidate| candidate == b"\x1b[?25h")
        );
    }

    #[cfg(target_os = "linux")]
    struct PanicGuardParentTerminal {
        master: std::fs::File,
        child: std::process::Child,
        output: Vec<u8>,
        original: libc::termios,
    }

    #[cfg(target_os = "linux")]
    impl PanicGuardParentTerminal {
        fn spawn() -> Self {
            use std::os::fd::FromRawFd as _;
            use std::process::Stdio;

            let mut master = -1;
            let mut slave = -1;
            let size = libc::winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            assert_eq!(
                unsafe {
                    libc::openpty(
                        &mut master,
                        &mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null(),
                        &size,
                    )
                },
                0
            );
            let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
            assert_eq!(unsafe { libc::tcgetattr(slave, &mut original) }, 0);
            let duplicate = |descriptor| {
                let found = unsafe { libc::dup(descriptor) };
                assert!(found >= 0);
                unsafe { std::fs::File::from_raw_fd(found) }
            };
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tui::tests::panic_hook_restores_parent_terminal_in_native_pty",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(PANIC_GUARD_CHILD_ENV, "1")
                .env("RUST_BACKTRACE", "0")
                .stdin(Stdio::from(duplicate(slave)))
                .stdout(Stdio::from(duplicate(slave)))
                .stderr(Stdio::from(duplicate(slave)))
                .spawn()
                .unwrap();
            unsafe { libc::close(slave) };
            let flags = unsafe { libc::fcntl(master, libc::F_GETFL) };
            assert!(flags >= 0);
            assert_eq!(
                unsafe { libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) },
                0
            );
            Self {
                master: unsafe { std::fs::File::from_raw_fd(master) },
                child,
                output: Vec::new(),
                original,
            }
        }

        fn write(&mut self, bytes: &[u8]) {
            self.master.write_all(bytes).unwrap();
            self.master.flush().unwrap();
        }

        fn wait_for(&mut self, needle: &[u8]) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !self
                .output
                .windows(needle.len())
                .any(|candidate| candidate == needle)
            {
                self.read_available();
                if let Some(status) = self.child.try_wait().unwrap() {
                    panic!(
                        "panic fixture exited before ready: {status}; output={}",
                        String::from_utf8_lossy(&self.output)
                    );
                }
                assert!(Instant::now() < deadline, "panic fixture timed out");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn finish(&mut self) -> std::process::ExitStatus {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                self.read_available();
                if let Some(status) = self.child.try_wait().unwrap() {
                    self.read_available();
                    return status;
                }
                assert!(Instant::now() < deadline, "panic fixture did not finish");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn assert_raw(&self) {
            let current = self.current_termios();
            assert_eq!(current.c_lflag & (libc::ICANON | libc::ECHO), 0);
        }

        fn assert_restored(&self) {
            let current = self.current_termios();
            assert_eq!(current.c_iflag, self.original.c_iflag);
            assert_eq!(current.c_oflag, self.original.c_oflag);
            assert_eq!(current.c_cflag, self.original.c_cflag);
            assert_eq!(current.c_lflag, self.original.c_lflag);
            assert_eq!(current.c_cc, self.original.c_cc);
        }

        fn current_termios(&self) -> libc::termios {
            use std::os::fd::AsRawFd as _;

            let mut current = unsafe { std::mem::zeroed::<libc::termios>() };
            assert_eq!(
                unsafe { libc::tcgetattr(self.master.as_raw_fd(), &mut current) },
                0
            );
            current
        }

        fn read_available(&mut self) {
            let mut bytes = [0_u8; 4096];
            loop {
                match self.master.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(count) => self.output.extend_from_slice(&bytes[..count]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                    Err(error) => panic!("failed to read panic fixture PTY: {error}"),
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for PanicGuardParentTerminal {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    #[test]
    fn exact_typed_bang_switches_and_other_bang_inputs_remain_prompts() {
        let mut composer = Composer::default();
        composer.insert_char('!');
        assert_eq!(composer.submit(), Some(ComposerSubmission::SwitchTerminal));

        composer.insert_text("!ls");
        assert_eq!(
            composer.submit(),
            Some(ComposerSubmission::Prompt("!ls".to_owned()))
        );

        composer.insert_text("!!");
        assert_eq!(
            composer.submit(),
            Some(ComposerSubmission::Prompt("!!".to_owned()))
        );

        composer.insert_char('!');
        composer.insert_char('\n');
        assert_eq!(
            composer.submit(),
            Some(ComposerSubmission::Prompt("!".to_owned()))
        );

        composer.insert_char('/');
        assert_eq!(composer.mode, ComposerMode::Command);
        composer.insert_char('/');
        assert_eq!(composer.mode, ComposerMode::Prompt);
        assert_eq!(composer.buffer, "/");
    }

    #[test]
    fn pasted_leading_sigils_remain_prompt_text() {
        let mut composer = Composer::default();
        composer.insert_paste("!printf pasted\n/also-literal");
        assert_eq!(composer.mode, ComposerMode::Prompt);
        assert_eq!(composer.buffer, "!printf pasted\n/also-literal");
        composer.reset();
        composer.insert_paste("!");
        assert_eq!(
            composer.submit(),
            Some(ComposerSubmission::Prompt("!".to_owned()))
        );
    }

    #[test]
    fn unicode_editing_moves_and_deletes_whole_graphemes() {
        let mut composer = Composer::default();
        composer.insert_text("a👩‍💻б");
        composer.move_left();
        composer.backspace();
        assert_eq!(composer.buffer, "aб");
        assert_eq!(composer.cursor, 1);
    }

    #[test]
    fn command_lexer_handles_quotes_and_escapes_without_shell_expansion() {
        assert_eq!(
            lex_command("workspace 'dir with spaces'/child\\ name").unwrap(),
            vec!["workspace", "dir with spaces/child name"]
        );
        assert_eq!(
            lex_command("workspace \"$HOME/*.rs\"").unwrap()[1],
            "$HOME/*.rs"
        );
        assert!(lex_command("workspace 'unfinished").is_err());
        assert!(lex_command("workspace trailing\\").is_err());
    }

    #[test]
    fn managed_shell_profile_is_exact_and_has_no_sh_fallback() {
        assert_eq!(
            managed_shell_profile_id(Path::new("/nix/store/hash/bin/bash")),
            Some("bash-managed")
        );
        assert_eq!(
            managed_shell_profile_id(Path::new("/usr/bin/zsh")),
            Some("zsh-managed")
        );
        assert_eq!(managed_shell_profile_id(Path::new("/bin/sh")), None);
    }

    #[test]
    fn terminal_environment_uses_a_bounded_physical_terminal_name() {
        let overlay = terminal_environment_for(Some("tmux-256color"));
        assert_eq!(overlay.values["TERM"], "tmux-256color");
        assert!(overlay.inherited_names.is_empty());
        assert!(overlay.secret_refs.is_empty());

        for invalid in ["", "../../host", "xterm\nINJECTED=value"] {
            assert_eq!(
                terminal_environment_for(Some(invalid)).values["TERM"],
                "xterm-256color"
            );
        }
    }

    #[test]
    fn operation_mode_parser_uses_the_canonical_catalog_spelling() {
        assert_eq!(
            parse_protocol_tool_mode("read-only").unwrap(),
            ProtocolToolMode::ReadOnly
        );
        assert_eq!(
            parse_protocol_tool_mode("execute").unwrap(),
            ProtocolToolMode::Execute
        );
        assert!(parse_protocol_tool_mode("read_only").is_err());
    }

    #[test]
    fn picker_filter_and_selection_reducer_are_bounded() {
        let mut picker = PickerState::new(
            PickerKind::Model,
            "models",
            vec![
                PickerEntry {
                    value: "local-small".to_owned(),
                    label: "Small".to_owned(),
                    detail: Some("fast local model".to_owned()),
                    payload: PickerPayload::Model("local-small".to_owned()),
                },
                PickerEntry {
                    value: "local-large".to_owned(),
                    label: "Large".to_owned(),
                    detail: Some("deep reasoning".to_owned()),
                    payload: PickerPayload::Model("local-large".to_owned()),
                },
            ],
        );

        for character in "reason".chars() {
            picker.push_query(character);
        }
        assert_eq!(picker.filtered_indices(), vec![1]);
        assert_eq!(picker.selected_entry().unwrap().value, "local-large");

        picker.query.clear();
        picker.move_selection(50);
        assert_eq!(picker.selected_entry().unwrap().value, "local-large");
        picker.move_selection(-50);
        assert_eq!(picker.selected_entry().unwrap().value, "local-small");

        picker.query.clear();
        for _ in 0..512 {
            picker.push_query('a');
        }
        picker.push_query('b');
        picker.push_query('\n');
        assert_eq!(picker.query.len(), 512);
        assert!(!picker.query.ends_with('b'));
        assert!(!picker.query.contains('\n'));
    }

    #[test]
    fn skills_picker_has_explicit_multi_select_and_empty_apply() {
        let entries = ["build", "review"]
            .into_iter()
            .map(|skill_id| PickerEntry {
                value: skill_id.to_owned(),
                label: skill_id.to_owned(),
                detail: None,
                payload: PickerPayload::Skill(skill_id.to_owned()),
            })
            .collect();
        let mut picker = PickerState::new(PickerKind::Skills, "skills", entries);

        picker.toggle_selected_skill();
        assert_eq!(picker.selected_values, BTreeSet::from(["build".to_owned()]));
        picker.select_all_skills();
        assert_eq!(
            picker.selected_values,
            BTreeSet::from(["build".to_owned(), "review".to_owned()])
        );
        picker.clear_skills();
        assert_eq!(
            picker_default_submit(&picker).unwrap(),
            PickerSubmit::Skills(Vec::new())
        );
    }

    #[test]
    fn mode_picker_uses_canonical_values_and_typed_payloads() {
        let entries = operation_mode_picker_entries(ProtocolToolMode::ReadOnly);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.value.as_str())
                .collect::<Vec<_>>(),
            vec!["read-only", "write", "execute", "approve", "admin"]
        );
        assert_eq!(entries[0].detail.as_deref(), Some("current mode"));
        let picker = PickerState::new(PickerKind::Mode, "mode", entries);
        assert_eq!(
            picker_default_submit(&picker).unwrap(),
            PickerSubmit::Mode(ProtocolToolMode::ReadOnly)
        );
    }

    #[test]
    fn process_picker_defaults_to_owner_write_and_foreign_read_only() {
        let human = test_terminal(
            TerminalOwnerView::Human {
                session_id: SessionId::generate(),
            },
            ExecutionProfile::Workspace,
        );
        let human_picker = PickerState::new(
            PickerKind::Processes,
            "processes",
            vec![process_picker_entry(test_process(Some(human.clone())))],
        );
        assert_eq!(
            picker_default_submit(&human_picker).unwrap(),
            PickerSubmit::Attach {
                terminal: Box::new(human),
                writable: true,
            }
        );

        let host = test_terminal(
            TerminalOwnerView::MainAgent {
                session_id: SessionId::generate(),
            },
            ExecutionProfile::Host,
        );
        let host_entry = process_picker_entry(test_process(Some(host.clone())));
        assert!(host_entry.detail.as_deref().unwrap().contains("HOST"));
        let foreign_picker = PickerState::new(PickerKind::Processes, "processes", vec![host_entry]);
        assert_eq!(
            picker_default_submit(&foreign_picker).unwrap(),
            PickerSubmit::Attach {
                terminal: Box::new(host),
                writable: false,
            }
        );
    }

    #[test]
    fn host_picker_actions_require_confirmation_and_cancel_never_submits() {
        let session_id = SessionId::generate();
        let mut state = test_ui_state(session_id, Vec::new());
        state.picker = Some(PickerState::new(
            PickerKind::Processes,
            "processes",
            host_terminal_picker_entries(),
        ));
        let client = RecordingHostClient::error("must not be called");

        assert!(
            handle_picker_key(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            )
            .is_none()
        );
        let confirmation = state
            .picker
            .as_ref()
            .and_then(|picker| picker.confirmation.as_ref())
            .expect("managed HOST action must enter confirmation state");
        assert!(confirmation.prompt.contains("managed startup"));
        assert!(matches!(
            confirmation.submit,
            PickerSubmit::EnsureHost {
                startup: HostStartupPolicy::ManagedOnly
            }
        ));
        assert!(client.requests().is_empty());

        assert!(
            handle_picker_key(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .is_none()
        );
        assert!(state.picker.as_ref().unwrap().confirmation.is_none());
        assert!(client.requests().is_empty());

        state.picker.as_mut().unwrap().selected = 1;
        assert!(
            handle_picker_key(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            )
            .is_none()
        );
        let confirmation = state
            .picker
            .as_ref()
            .and_then(|picker| picker.confirmation.as_ref())
            .expect("user-rc HOST action must enter its own confirmation state");
        assert!(confirmation.prompt.contains("source your normal shell rc"));
        let Some(UiControl::Submission(ComposerSubmission::Picker(PickerSubmit::EnsureHost {
            startup,
        }))) = handle_picker_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        else {
            panic!("confirmed user-rc HOST action did not produce a typed submission");
        };
        assert_eq!(startup, HostStartupPolicy::SourceUserRc);
        assert!(client.requests().is_empty());
    }

    #[tokio::test]
    async fn confirmed_managed_host_uses_explicit_family_and_attaches_distinct_terminal() {
        let session_id = SessionId::generate();
        let workspace = test_terminal(
            TerminalOwnerView::Human {
                session_id: session_id.clone(),
            },
            ExecutionProfile::Workspace,
        );
        let host = test_terminal(
            TerminalOwnerView::Human {
                session_id: session_id.clone(),
            },
            ExecutionProfile::Host,
        );
        let client = RecordingHostClient::success(
            host.clone(),
            agl_protocol::TerminalEnsureDisposition::Created,
        );
        let mut state = test_ui_state(session_id.clone(), vec![workspace.clone()]);

        let outcome = handle_host_terminal_submit(
            &client,
            &session_id,
            &mut state,
            HostStartupPolicy::ManagedOnly,
        )
        .await;
        let SubmissionOutcome::EnterTerminal(request) = outcome else {
            panic!("confirmed HOST ensure did not enter Terminal view");
        };
        assert!(request.writable);
        assert_eq!(request.terminal, host);
        assert_eq!(terminal_authority_label(request.terminal.profile), "HOST");
        assert_eq!(
            state.last_terminal,
            Some(request.terminal.terminal_id.clone())
        );
        assert_eq!(state.snapshot.terminals[0], workspace);
        assert_eq!(state.snapshot.terminals.len(), 2);

        let requests = client.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.terminal.session_id, session_id);
        assert_eq!(request.terminal.profile, ExecutionProfile::Host);
        assert_eq!(
            request.terminal.host_startup,
            HostStartupPolicy::ManagedOnly
        );
        assert!(request.confirm_host_authority);
        assert_eq!(request.terminal.execution_context_revision, 41);
        assert_eq!(request.terminal.shell_profile_id, "bash-managed");
        assert_eq!(request.terminal.agl_env, current_terminal_environment());
        assert!(
            request
                .terminal
                .client_submission_id
                .starts_with("cli-host-terminal-")
        );
        let wire = serde_json::to_value(request).unwrap();
        assert!(wire.get("path").is_none());
        assert!(!wire.to_string().contains(".bashrc"));
        assert!(!wire.to_string().contains(".zshrc"));
    }

    #[tokio::test]
    async fn source_user_rc_request_is_explicit_and_existing_host_is_idempotent() {
        let session_id = SessionId::generate();
        let workspace = test_terminal(
            TerminalOwnerView::Human {
                session_id: session_id.clone(),
            },
            ExecutionProfile::Workspace,
        );
        let host = test_terminal(
            TerminalOwnerView::Human {
                session_id: session_id.clone(),
            },
            ExecutionProfile::Host,
        );
        let client = RecordingHostClient::success(
            host.clone(),
            agl_protocol::TerminalEnsureDisposition::Reused,
        );
        let mut state = test_ui_state(session_id.clone(), vec![workspace.clone(), host.clone()]);

        let outcome = handle_host_terminal_submit(
            &client,
            &session_id,
            &mut state,
            HostStartupPolicy::SourceUserRc,
        )
        .await;
        let SubmissionOutcome::EnterTerminal(request) = outcome else {
            panic!("reused HOST terminal was not attached");
        };
        assert!(request.writable);
        assert_eq!(request.terminal.terminal_id, host.terminal_id);
        assert_eq!(state.snapshot.terminals[0], workspace);
        assert_eq!(
            state
                .snapshot
                .terminals
                .iter()
                .filter(|terminal| terminal.terminal_id == host.terminal_id)
                .count(),
            1
        );
        let requests = client.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].terminal.host_startup,
            HostStartupPolicy::SourceUserRc
        );
        assert!(requests[0].confirm_host_authority);
    }

    #[tokio::test]
    async fn host_errors_are_visible_and_workspace_identity_cannot_be_upgraded() {
        let session_id = SessionId::generate();
        let workspace = test_terminal(
            TerminalOwnerView::Human {
                session_id: session_id.clone(),
            },
            ExecutionProfile::Workspace,
        );
        let client = RecordingHostClient::error("operator denied Host authority");
        let mut state = test_ui_state(session_id.clone(), vec![workspace.clone()]);
        assert!(matches!(
            handle_host_terminal_submit(
                &client,
                &session_id,
                &mut state,
                HostStartupPolicy::ManagedOnly
            )
            .await,
            SubmissionOutcome::Continue
        ));
        assert!(
            state
                .notices
                .iter()
                .any(|notice| notice.contains("operator denied Host authority"))
        );
        assert_eq!(state.snapshot.terminals, vec![workspace.clone()]);

        let mut invalid_host = test_terminal(
            TerminalOwnerView::Human {
                session_id: session_id.clone(),
            },
            ExecutionProfile::Host,
        );
        invalid_host.terminal_id = workspace.terminal_id.clone();
        invalid_host.execution_id = workspace.execution_id.clone();
        let client = RecordingHostClient::success(
            invalid_host,
            agl_protocol::TerminalEnsureDisposition::Created,
        );
        let mut state = test_ui_state(session_id.clone(), vec![workspace.clone()]);
        assert!(matches!(
            handle_host_terminal_submit(
                &client,
                &session_id,
                &mut state,
                HostStartupPolicy::ManagedOnly
            )
            .await,
            SubmissionOutcome::Continue
        ));
        assert!(
            state
                .notices
                .iter()
                .any(|notice| notice.contains("reuse a Workspace terminal identity"))
        );
        assert_eq!(state.snapshot.terminals, vec![workspace.clone()]);
        assert_eq!(state.last_terminal, Some(workspace.terminal_id));
    }

    struct RecordingHostClient {
        response: std::result::Result<HumanTerminalEnsuredEvent, ClientError>,
        requests: Mutex<Vec<HumanHostTerminalEnsureRequest>>,
    }

    impl RecordingHostClient {
        fn success(
            terminal: TerminalSessionView,
            disposition: agl_protocol::TerminalEnsureDisposition,
        ) -> Self {
            Self {
                response: Ok(HumanTerminalEnsuredEvent {
                    terminal,
                    disposition,
                }),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn error(message: &str) -> Self {
            Self {
                response: Err(ClientError::InvalidRequest(message.to_owned())),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<HumanHostTerminalEnsureRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl HostTerminalEnsurer for RecordingHostClient {
        async fn ensure_host_terminal(
            &self,
            request: HumanHostTerminalEnsureRequest,
        ) -> std::result::Result<HumanTerminalEnsuredEvent, ClientError> {
            self.requests.lock().unwrap().push(request);
            self.response.clone()
        }
    }

    fn test_ui_state(session_id: SessionId, terminals: Vec<TerminalSessionView>) -> UiState {
        let last_terminal = terminals
            .first()
            .map(|terminal| terminal.terminal_id.clone());
        UiState {
            snapshot: SessionPresentationSnapshot {
                session_id: session_id.clone(),
                cursor: agl_protocol::PresentationCursor {
                    daemon_instance_id: agl_ids::DaemonInstanceId::generate(),
                    revision: 1,
                },
                older_page_cursor: None,
                header: agl_protocol::SessionHeader {
                    session_id: session_id.clone(),
                    status: agl_protocol::SessionPresentationStatus::Active,
                    durable: true,
                    resumed: false,
                    title: None,
                    function_name: "coding".to_owned(),
                    model_id: Some("local".to_owned()),
                    operation_mode: ProtocolToolMode::Execute,
                    selected_skills: Vec::new(),
                    runtime_context_revision: 1,
                    workspace_root: "/workspace".to_owned(),
                    cwd: "/workspace".to_owned(),
                    execution_context_revision: 41,
                    context_used_tokens: None,
                    context_limit_tokens: None,
                    active_run_count: 0,
                    queued_prompt_count: 0,
                    active_execution_count: u32::try_from(terminals.len()).unwrap(),
                },
                items: Vec::new(),
                active_run: None,
                queued_prompts: Vec::new(),
                terminals,
                executions: Vec::new(),
                command_context: agl_protocol::CommandContext {
                    session_id: Some(session_id),
                    session_active: true,
                    active_or_queued_turns: 0,
                    active_executions: 0,
                    host_shell_available: true,
                    operation_mode: ProtocolToolMode::Execute,
                },
            },
            catalog: Vec::new(),
            composer: Composer::default(),
            last_terminal,
            terminal_cursors: BTreeMap::new(),
            seen_terminals: BTreeSet::new(),
            assistant_deltas: BTreeMap::new(),
            picker: None,
            notices: Vec::new(),
            active_run: None,
            exit_armed: false,
            workspace_change_armed: None,
            shell_profile_id: Some("bash-managed".to_owned()),
            history: InputHistory {
                root: None,
                prompt: Vec::new(),
            },
        }
    }

    fn test_process(terminal: Option<TerminalSessionView>) -> ProcessPickerItem {
        let execution_id = terminal
            .as_ref()
            .map(|terminal| terminal.execution_id.clone())
            .unwrap_or_else(ExecutionId::generate);
        ProcessPickerItem {
            execution_id,
            state: agl_protocol::ExecutionState::Running,
            profile: terminal
                .as_ref()
                .map_or(ExecutionProfile::Workspace, |terminal| terminal.profile),
            cwd: "/workspace".to_owned(),
            terminal,
        }
    }

    fn test_terminal(owner: TerminalOwnerView, profile: ExecutionProfile) -> TerminalSessionView {
        TerminalSessionView {
            terminal_id: TerminalSessionId::generate(),
            execution_id: ExecutionId::generate(),
            owner,
            profile,
            shell: agl_protocol::ShellProfileView {
                profile_id: "bash-managed".to_owned(),
                program: "/bin/bash".to_owned(),
                executable_digest: "sha256:executable".to_owned(),
                config_digest: "sha256:config".to_owned(),
            },
            workspace_root: "/workspace".to_owned(),
            cwd: "/workspace".to_owned(),
            initial_environment_digest: "sha256:environment".to_owned(),
            environment_names: vec!["PATH".to_owned()],
            command_sequence: 0,
            prompt_state: TerminalPromptState::Ready,
            process_state: agl_protocol::ExecutionState::Running,
            exit: None,
            writer: agl_protocol::TerminalWriterView::Owner,
            promoted: false,
        }
    }

    #[test]
    fn assistant_deltas_are_ordered_bounded_and_retired_on_gaps() {
        let run_id = RunId::generate();
        let message_id = MessageId::generate();
        let mut deltas = BTreeMap::new();
        assert_eq!(
            append_assistant_delta(&mut deltas, run_id.clone(), message_id.clone(), 1, "hel",),
            AssistantDeltaApply::Applied
        );
        assert_eq!(
            append_assistant_delta(
                &mut deltas,
                run_id.clone(),
                message_id.clone(),
                1,
                "duplicate",
            ),
            AssistantDeltaApply::Duplicate
        );
        assert_eq!(deltas[&message_id].text, "hel");
        assert_eq!(
            append_assistant_delta(&mut deltas, run_id, message_id.clone(), 3, "lo"),
            AssistantDeltaApply::SequenceGap
        );
        assert!(!deltas[&message_id].valid);
        assert!(deltas[&message_id].text.is_empty());
    }

    #[test]
    fn assembled_snapshot_replacement_is_installed_as_one_projection() {
        let session_id = SessionId::generate();
        let terminal = test_terminal(
            TerminalOwnerView::Human {
                session_id: session_id.clone(),
            },
            ExecutionProfile::Workspace,
        );
        let terminal_id = terminal.terminal_id.clone();
        let mut state = test_ui_state(session_id, vec![terminal]);
        let run_id = RunId::generate();
        let message_id = MessageId::generate();
        assert_eq!(
            append_assistant_delta(
                &mut state.assistant_deltas,
                run_id,
                message_id,
                1,
                "partial",
            ),
            AssistantDeltaApply::Applied
        );
        let mut replacement = state.snapshot.clone();
        replacement.cursor.revision += 1;
        replacement.header.cwd = "/workspace/replaced".to_owned();
        replacement.terminals.clear();

        install_presentation_snapshot(&mut state, replacement.clone());

        assert_eq!(state.snapshot, replacement);
        assert!(state.assistant_deltas.is_empty());
        assert_eq!(
            terminal_prompt_from_snapshot(&state.snapshot, &terminal_id),
            TerminalPromptState::Unavailable
        );
    }

    #[test]
    fn authoritative_snapshots_mark_existing_terminals_for_tail_reattach() {
        let session_id = SessionId::generate();
        let terminal = test_terminal(
            TerminalOwnerView::Human {
                session_id: session_id.clone(),
            },
            ExecutionProfile::Workspace,
        );
        let terminal_id = terminal.terminal_id.clone();
        let mut state = test_ui_state(session_id, Vec::new());
        let mut snapshot = state.snapshot.clone();
        snapshot.terminals.push(terminal);

        install_presentation_snapshot(&mut state, snapshot.clone());
        assert!(state.seen_terminals.contains(&terminal_id));

        state.seen_terminals.clear();
        install_session_switch(
            &mut state,
            snapshot,
            Vec::new(),
            InputHistory {
                root: None,
                prompt: Vec::new(),
            },
            Vec::new(),
        );
        assert!(state.seen_terminals.contains(&terminal_id));
    }

    #[test]
    fn prompt_lifecycle_events_keep_peer_ui_state_and_counts_coherent() {
        let session_id = SessionId::generate();
        let run_id = RunId::generate();
        let mut state = test_ui_state(session_id, Vec::new());

        apply_presentation_event(
            &mut state,
            agl_protocol::SessionPresentationEventPayload::PromptQueued {
                prompt: agl_protocol::QueuedPromptView {
                    run_id: run_id.clone(),
                    ordinal: 1,
                },
            },
        );
        assert_eq!(state.snapshot.header.queued_prompt_count, 1);
        assert_eq!(state.snapshot.command_context.active_or_queued_turns, 1);

        apply_presentation_event(
            &mut state,
            agl_protocol::SessionPresentationEventPayload::PromptActivated {
                run_id: run_id.clone(),
            },
        );
        assert_eq!(state.active_run.as_ref(), Some(&run_id));
        assert_eq!(state.snapshot.header.active_run_count, 1);
        assert_eq!(state.snapshot.header.queued_prompt_count, 0);

        apply_presentation_event(
            &mut state,
            agl_protocol::SessionPresentationEventPayload::PromptFinished {
                run_id,
                state: "answered".to_owned(),
            },
        );
        assert!(state.active_run.is_none());
        assert_eq!(state.snapshot.header.active_run_count, 0);
        assert_eq!(state.snapshot.command_context.active_or_queued_turns, 0);
    }

    #[test]
    fn daemon_connection_errors_distinguish_missing_and_incompatible_servers() {
        let socket = Path::new("/tmp/agentlibre-test.sock");
        let missing = interactive_connect_error(socket, ClientError::Io("refused".to_owned()));
        assert!(missing.to_string().contains("daemon is unavailable"));

        let incompatible = interactive_connect_error(
            socket,
            ClientError::SchemaMismatch {
                expected: "agentlibre.event.v5alpha",
            },
        );
        assert!(incompatible.to_string().contains("incompatible protocol"));
        assert!(format!("{incompatible:#}").contains("v5alpha"));
    }

    #[test]
    fn private_history_is_bounded_per_workspace_and_hides_workspace_path() {
        let root = std::env::temp_dir().join(format!(
            "agl-cli-history-test-{}",
            agl_ids::RequestId::generate()
        ));
        let state_dir = root.join("state");
        let workspace = "/private/workspace/name";
        let (mut history, warnings) = InputHistory::load(&state_dir, workspace, true);
        assert!(warnings.is_empty());
        history.record(HistoryMode::Prompt, "hello").unwrap();
        history.record(HistoryMode::Prompt, "hello").unwrap();
        let history_root = history.root.clone().unwrap();
        assert!(!history_root.to_string_lossy().contains("workspace"));
        assert_eq!(history.prompt, vec!["hello"]);
        let (reloaded, warnings) = InputHistory::load(&state_dir, workspace, true);
        assert!(warnings.is_empty());
        assert_eq!(reloaded.prompt, vec!["hello"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(history_root.join("prompt.jsonl"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
}
