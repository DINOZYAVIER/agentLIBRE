use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal as _};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agl_client::{
    AgentLibreClient, ClientError, ExecutionAttachmentEvent, PresentationSubscriptionEvent,
    RunSubscriptionEvent,
};
use agl_ids::{ExecutionId, SessionId};
use agl_protocol::{
    ApplicationAction, ApplicationActionRequest, ApplicationActionResult, ClientEffectKind,
    CommandAvailability, CommandCatalogRequest, CommandDescriptor, ExecutionAttachRequest,
    ExecutionProfile, ProtocolRunState, ProtocolToolMode, RunBudgetRequest, RunSubmitRequest,
    RunSubscribeRequest, SessionListRequest, SessionOpenRequest, SessionPresentationItem,
    SessionPresentationRequest, SessionPresentationSnapshot, SessionPresentationSubscribeRequest,
    TerminalSize, UserShellStartRequest,
};
use agl_runtime::AgentLibreRuntimeConfig;
use anyhow::{Context as _, Result, bail};
use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use fs2::FileExt as _;
use futures_util::StreamExt as _;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{TerminalOptions, Viewport};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::sync::mpsc;
use unicode_segmentation::UnicodeSegmentation as _;

use crate::args::InteractiveOptions;

const UI_EVENT_CAPACITY: usize = 256;
const MAX_LOCAL_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_COMPOSER_BYTES: usize = 64 * 1024;
const MAX_COMPOSER_LINES: usize = 2_000;
const MAX_HISTORY_ENTRIES: usize = 1_000;
const MAX_HISTORY_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ComposerMode {
    Prompt,
    Shell(ExecutionProfile),
    Command,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ComposerSubmission {
    Prompt(String),
    Shell {
        command: String,
        profile: ExecutionProfile,
        background: bool,
    },
    Command(String),
}

#[derive(Debug)]
struct Composer {
    mode: ComposerMode,
    buffer: String,
    cursor: usize,
    selected_command: usize,
    history_position: Option<usize>,
    history_draft: String,
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
        }
    }
}

impl Composer {
    fn label(&self) -> &'static str {
        match self.mode {
            ComposerMode::Prompt => "Prompt >",
            ComposerMode::Shell(ExecutionProfile::Workspace) => "Shell · workspace !",
            ComposerMode::Shell(ExecutionProfile::Host) => "Shell · host !",
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
        if self.mode == ComposerMode::Prompt && self.buffer.is_empty() {
            match character {
                '!' => {
                    self.mode = ComposerMode::Shell(ExecutionProfile::Workspace);
                    return;
                }
                '/' => {
                    self.mode = ComposerMode::Command;
                    return;
                }
                _ => {}
            }
        } else if self.buffer.is_empty() {
            match (self.mode, character) {
                (ComposerMode::Shell(_), '!') => {
                    self.mode = ComposerMode::Prompt;
                    self.buffer.push('!');
                    self.cursor = 1;
                    return;
                }
                (ComposerMode::Command, '/') => {
                    self.mode = ComposerMode::Prompt;
                    self.buffer.push('/');
                    self.cursor = 1;
                    return;
                }
                _ => {}
            }
        }
        self.buffer.insert(self.cursor, character);
        self.cursor += character.len_utf8();
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

    fn toggle_shell_profile(&mut self) {
        if let ComposerMode::Shell(profile) = self.mode {
            self.mode = ComposerMode::Shell(match profile {
                ExecutionProfile::Workspace => ExecutionProfile::Host,
                ExecutionProfile::Host => ExecutionProfile::Workspace,
            });
        }
    }

    fn submit(&mut self, background: bool) -> Option<ComposerSubmission> {
        let text = self.buffer.trim_end_matches(['\r', '\n']).to_owned();
        if text.trim().is_empty() {
            return None;
        }
        let submission = match self.mode {
            ComposerMode::Prompt => ComposerSubmission::Prompt(text),
            ComposerMode::Shell(profile) => ComposerSubmission::Shell {
                command: text,
                profile,
                background,
            },
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
        self.cursor = self.buffer.len();
    }
}

#[derive(Clone, Copy)]
enum HistoryMode {
    Prompt,
    Shell,
}

impl HistoryMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Shell => "shell",
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
    shell: Vec<String>,
}

impl InputHistory {
    fn load(state_dir: &Path, workspace: &str, enabled: bool) -> (Self, Vec<String>) {
        if !enabled {
            return (
                Self {
                    root: None,
                    prompt: Vec::new(),
                    shell: Vec::new(),
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
                    shell: Vec::new(),
                },
                warnings,
            );
        }
        let prompt = read_history_file(
            &root.join("prompt.jsonl"),
            HistoryMode::Prompt,
            &mut warnings,
        );
        let shell = read_history_file(&root.join("shell.jsonl"), HistoryMode::Shell, &mut warnings);
        (
            Self {
                root: Some(root),
                prompt,
                shell,
            },
            warnings,
        )
    }

    fn entries(&self, mode: ComposerMode) -> &[String] {
        match mode {
            ComposerMode::Prompt => &self.prompt,
            ComposerMode::Shell(_) => &self.shell,
            ComposerMode::Command => &[],
        }
    }

    fn record(&mut self, mode: HistoryMode, input: &str) -> Result<()> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        let entries = match mode {
            HistoryMode::Prompt => &mut self.prompt,
            HistoryMode::Shell => &mut self.shell,
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

struct LocalExecution {
    command: String,
    profile: ExecutionProfile,
    output: String,
    state: String,
}

struct UiState {
    snapshot: SessionPresentationSnapshot,
    catalog: Vec<CommandDescriptor>,
    composer: Composer,
    executions: BTreeMap<ExecutionId, LocalExecution>,
    notices: Vec<String>,
    active_run: Option<agl_ids::RunId>,
    exit_armed: bool,
    history: InputHistory,
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
                query.is_empty()
                    || command.name.starts_with(&query)
                    || command
                        .aliases
                        .iter()
                        .any(|alias| alias.starts_with(&query))
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
    RunAccepted(agl_ids::RunId),
    Snapshot(Box<SessionPresentationSnapshot>),
    ShellStarted {
        execution_id: ExecutionId,
        command: String,
        profile: ExecutionProfile,
    },
    ShellOutput {
        execution_id: ExecutionId,
        bytes: Vec<u8>,
    },
    ShellFinished {
        execution_id: ExecutionId,
        state: String,
    },
    Notice(String),
}

pub(crate) fn run_interactive(
    options: InteractiveOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
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
    let session_id = resolve_session(&client, &options).await?;
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
    let mut notices = vec!["Type ! for shell, / for commands, Ctrl+D to disconnect".to_owned()];
    notices.extend(history_warnings);
    let mut state = UiState {
        snapshot: presentation.snapshot.clone(),
        catalog,
        composer: Composer::default(),
        executions: BTreeMap::new(),
        notices,
        active_run: None,
        exit_armed: false,
        history,
    };
    let (async_sender, mut async_events) = mpsc::channel(UI_EVENT_CAPACITY);
    let mut input = EventStream::new();
    let terminal_mode = TuiTerminalMode::enter()?;
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

    let result = loop {
        terminal
            .draw(|frame| draw(frame, &state))
            .context("failed to render terminal UI")?;
        tokio::select! {
            event = input.next() => {
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
                                UiControl::Submission(submission) => {
                                    if handle_submission(
                                        &client,
                                        &session_id,
                                        &mut state,
                                        submission,
                                        &async_sender,
                                    ).await? {
                                        break Ok(());
                                    }
                                }
                            }
                        }
                    }
                    Event::Paste(text) => state.composer.insert_paste(&text),
                    Event::Resize(_, _) => terminal.autoresize()?,
                    _ => {}
                }
            }
            event = presentation.next() => {
                match event {
                    Ok(Some(PresentationSubscriptionEvent::Event(event))) => {
                        apply_presentation_event(&mut state, event.event.clone());
                    }
                    Ok(Some(PresentationSubscriptionEvent::Finished(event))) => {
                        state.notice(format!("presentation ended: {:?}", event.reason));
                    }
                    Ok(None) => {}
                    Err(error) => state.notice(format!("presentation needs resync: {error}")),
                }
            }
            event = async_events.recv() => {
                if let Some(event) = event {
                    apply_async_event(&mut state, event);
                }
            }
        }
    };
    drop(terminal);
    drop(terminal_mode);
    result
}

async fn resolve_session(
    client: &AgentLibreClient,
    options: &InteractiveOptions,
) -> Result<SessionId> {
    if let Some(model_id) = &options.model_id {
        bail!(
            "interactive model selection is not available for `{model_id}`; configure the daemon profile first"
        );
    }
    let resume_id = match options.resume.as_deref() {
        None => None,
        Some("latest") => client
            .list_sessions(SessionListRequest::default())
            .await?
            .sessions
            .into_iter()
            .filter(|session| {
                matches!(
                    session.status,
                    agl_protocol::SessionStatus::Open | agl_protocol::SessionStatus::Busy
                )
            })
            .max_by(|left, right| {
                left.updated_at_unix_ms
                    .cmp(&right.updated_at_unix_ms)
                    .then_with(|| left.session_id.cmp(&right.session_id))
            })
            .map(|session| session.session_id),
        Some(value) => Some(SessionId::parse(value).context("invalid --resume session ID")?),
    };
    if options.resume.is_some() && resume_id.is_none() {
        bail!("no resumable session was found");
    }
    client
        .open_session(SessionOpenRequest {
            session_id: resume_id,
            new_session: options.resume.is_none(),
            workspace_root: options
                .workspace_root
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            function_ref: options.function_ref.clone(),
            skills: options.skills.clone(),
            tool_mode: match options.operation_mode {
                Some(crate::args::ToolAccessMode::Write) => ProtocolToolMode::Write,
                Some(crate::args::ToolAccessMode::Execute) => ProtocolToolMode::Execute,
                Some(crate::args::ToolAccessMode::Approve) => ProtocolToolMode::Approve,
                Some(crate::args::ToolAccessMode::Admin) => ProtocolToolMode::Admin,
                Some(crate::args::ToolAccessMode::ReadOnly) | None => ProtocolToolMode::ReadOnly,
            },
        })
        .await
        .context("failed to open interactive session")
        .map(|opened| opened.session_id)
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
}

fn handle_key(state: &mut UiState, key: KeyEvent) -> Option<UiControl> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('d') => return Some(UiControl::Disconnect),
            KeyCode::Char('b') => {
                return state.composer.submit(true).map(UiControl::Submission);
            }
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
    if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('p') {
        state.composer.toggle_shell_profile();
        return None;
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
            return state.composer.submit(false).map(UiControl::Submission);
        }
        _ => {}
    }
    None
}

async fn handle_submission(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
    submission: ComposerSubmission,
    sender: &mpsc::Sender<UiAsyncEvent>,
) -> Result<bool> {
    match &submission {
        ComposerSubmission::Prompt(input) => {
            if let Err(error) = state.history.record(HistoryMode::Prompt, input) {
                state.notice(format!("prompt history write failed: {error:#}"));
            }
        }
        ComposerSubmission::Shell { command, .. } => {
            if let Err(error) = state.history.record(HistoryMode::Shell, command) {
                state.notice(format!("shell history write failed: {error:#}"));
            }
        }
        ComposerSubmission::Command(_) => {}
    }
    match submission {
        ComposerSubmission::Prompt(content) => {
            spawn_prompt(client.clone(), session_id.clone(), content, sender.clone());
        }
        ComposerSubmission::Shell {
            command,
            profile,
            background,
        } => {
            let (_, rows) = crossterm::terminal::size().unwrap_or((80, 24));
            let (columns, _) = crossterm::terminal::size().unwrap_or((80, 24));
            spawn_shell(
                client.clone(),
                UserShellStartRequest {
                    session_id: session_id.clone(),
                    client_submission_id: format!("cli-shell-{}", agl_ids::RequestId::generate()),
                    command: command.clone(),
                    execution_context_revision: state.snapshot.header.execution_context_revision,
                    profile,
                    terminal_size: TerminalSize {
                        columns: columns.max(1),
                        rows: rows.max(1),
                    },
                    background,
                },
                command,
                profile,
                sender.clone(),
            );
        }
        ComposerSubmission::Command(command) => {
            return handle_command(client, session_id, state, &command).await;
        }
    }
    Ok(false)
}

fn spawn_prompt(
    client: AgentLibreClient,
    session_id: SessionId,
    content: String,
    sender: mpsc::Sender<UiAsyncEvent>,
) {
    tokio::spawn(async move {
        let accepted = match client
            .submit_run(RunSubmitRequest {
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
            .send(UiAsyncEvent::RunAccepted(accepted.run_id.clone()))
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
            .session_presentation(SessionPresentationRequest { session_id })
            .await
        {
            Ok(event) => {
                let _ = sender
                    .send(UiAsyncEvent::Snapshot(Box::new(event.snapshot)))
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

fn spawn_shell(
    client: AgentLibreClient,
    request: UserShellStartRequest,
    command: String,
    profile: ExecutionProfile,
    sender: mpsc::Sender<UiAsyncEvent>,
) {
    tokio::spawn(async move {
        let accepted = match client.start_user_shell(request).await {
            Ok(accepted) => accepted,
            Err(error) => {
                let _ = sender
                    .send(UiAsyncEvent::Notice(format!("shell rejected: {error}")))
                    .await;
                return;
            }
        };
        let execution_id = accepted.execution_id.clone();
        let _ = sender
            .send(UiAsyncEvent::ShellStarted {
                execution_id: execution_id.clone(),
                command,
                profile,
            })
            .await;
        let mut attachment = match client
            .execution_attach(ExecutionAttachRequest {
                execution_id: execution_id.clone(),
                after_sequence: 0,
                writable: false,
            })
            .await
        {
            Ok(attachment) => attachment,
            Err(error) => {
                let _ = sender
                    .send(UiAsyncEvent::Notice(format!(
                        "shell capture failed: {error}"
                    )))
                    .await;
                return;
            }
        };
        loop {
            match attachment.next().await {
                Ok(Some(ExecutionAttachmentEvent::Output(event))) => {
                    if let Ok(bytes) = event.chunk.bytes.decode(MAX_LOCAL_OUTPUT_BYTES) {
                        let _ = sender
                            .send(UiAsyncEvent::ShellOutput {
                                execution_id: execution_id.clone(),
                                bytes,
                            })
                            .await;
                    }
                }
                Ok(Some(ExecutionAttachmentEvent::Finished(event))) => {
                    let _ = sender
                        .send(UiAsyncEvent::ShellFinished {
                            execution_id,
                            state: format!("{:?}", event.state).to_ascii_lowercase(),
                        })
                        .await;
                    return;
                }
                Ok(None) => return,
                Err(error) => {
                    let _ = sender
                        .send(UiAsyncEvent::Notice(format!(
                            "shell stream failed: {error}"
                        )))
                        .await;
                    return;
                }
            }
        }
    });
}

async fn handle_command(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
    command: &str,
) -> Result<bool> {
    let words = lex_command(command)?;
    let mut parts = words.into_iter();
    let name = parts.next().unwrap_or_default();
    match name.as_str() {
        "disconnect" => return Ok(true),
        "help" => {
            state.notice("Use ↑/↓ in Command mode; Enter invokes the selected command");
            return Ok(false);
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
            return Ok(false);
        }
        _ => {}
    }
    let action = match name.as_str() {
        "status" => ApplicationAction::SessionStatus,
        "pwd" => ApplicationAction::WorkingDirectoryGet,
        "workspace" => match parts.next() {
            Some(path) => ApplicationAction::WorkspaceSet {
                path: std::iter::once(path)
                    .chain(parts)
                    .collect::<Vec<_>>()
                    .join(" "),
            },
            None => ApplicationAction::WorkspaceGet,
        },
        "cd" => {
            let host = matches!(parts.clone().next().as_deref(), Some("--host"));
            if host {
                parts.next();
            }
            let path = parts.collect::<Vec<_>>().join(" ");
            if path.is_empty() {
                bail!("/cd requires PATH");
            }
            ApplicationAction::WorkingDirectorySet {
                path,
                profile: if host {
                    ExecutionProfile::Host
                } else {
                    ExecutionProfile::Workspace
                },
            }
        }
        "processes" => ApplicationAction::ExecutionList {
            include_finished: matches!(parts.next().as_deref(), Some("--all")),
        },
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
            state.notice("Use `agl process attach EXECUTION_ID` for raw TTY attachment.");
            return Ok(false);
        }
        "new" | "resume" | "model" | "mode" | "skills" => {
            state.notice(format!(
                "/{name} is visible but unavailable in the current session"
            ));
            return Ok(false);
        }
        _ => {
            state.notice(format!("unknown command /{name}"));
            return Ok(false);
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
            ApplicationActionResult::SessionExited { .. } => return Ok(true),
            ApplicationActionResult::Status { header }
            | ApplicationActionResult::WorkspaceChanged { header }
            | ApplicationActionResult::WorkingDirectoryChanged { header } => {
                state.snapshot.header = header;
            }
            ApplicationActionResult::Executions { executions } => {
                state.notice(format!("{} execution(s)", executions.len()));
            }
            ApplicationActionResult::Cleared { .. } => {
                state.snapshot.items.clear();
                state.notice("conversation context cleared");
            }
            _ => state.notice(format!("/{name} completed")),
        },
        Err(error) => state.notice(format!("/{name} failed: {error}")),
    }
    Ok(false)
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

fn apply_async_event(state: &mut UiState, event: UiAsyncEvent) {
    match event {
        UiAsyncEvent::RunAccepted(run_id) => state.active_run = Some(run_id),
        UiAsyncEvent::Snapshot(snapshot) => {
            state.snapshot = *snapshot;
            state.active_run = None;
        }
        UiAsyncEvent::ShellStarted {
            execution_id,
            command,
            profile,
        } => {
            state.executions.insert(
                execution_id,
                LocalExecution {
                    command,
                    profile,
                    output: String::new(),
                    state: "running".to_owned(),
                },
            );
        }
        UiAsyncEvent::ShellOutput {
            execution_id,
            bytes,
        } => {
            if let Some(execution) = state.executions.get_mut(&execution_id) {
                execution.output.push_str(&sanitize_terminal_output(&bytes));
                if execution.output.len() > MAX_LOCAL_OUTPUT_BYTES {
                    let start = execution.output.len() - MAX_LOCAL_OUTPUT_BYTES;
                    let start = execution.output.floor_char_boundary(start);
                    execution.output.drain(..start);
                }
            }
        }
        UiAsyncEvent::ShellFinished {
            execution_id,
            state: terminal,
        } => {
            if let Some(execution) = state.executions.get_mut(&execution_id) {
                execution.state = terminal;
            }
        }
        UiAsyncEvent::Notice(message) => state.notice(message),
    }
}

fn apply_presentation_event(
    state: &mut UiState,
    event: agl_protocol::SessionPresentationEventPayload,
) {
    match event {
        agl_protocol::SessionPresentationEventPayload::SnapshotReplaced { snapshot } => {
            state.snapshot = *snapshot;
        }
        agl_protocol::SessionPresentationEventPayload::HeaderChanged { header } => {
            state.snapshot.header = header
        }
        agl_protocol::SessionPresentationEventPayload::ItemUpsert { item } => {
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
        agl_protocol::SessionPresentationEventPayload::ItemRemoved { item_key } => state
            .snapshot
            .items
            .retain(|item| presentation_item_key(item) != item_key),
        agl_protocol::SessionPresentationEventPayload::Notice { message, .. } => {
            state.notice(message)
        }
        _ => {}
    }
}

fn presentation_item_key(item: &SessionPresentationItem) -> String {
    match item {
        SessionPresentationItem::UserMessage { message_id, .. }
        | SessionPresentationItem::AssistantMessage { message_id, .. } => message_id.to_string(),
        SessionPresentationItem::AgentAction {
            run_id, step_id, ..
        } => format!("{run_id}:{step_id}"),
        SessionPresentationItem::UserExecution { execution_id, .. } => execution_id.to_string(),
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
            "Enter submit  Shift+Enter newline  Alt+P profile  Esc prompt  Ctrl+D disconnect",
        )
        .style(Style::default().fg(Color::DarkGray)),
        layout[4],
    );
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
            _ => {}
        }
        lines.push(Line::raw(""));
    }
    for (execution_id, execution) in &state.executions {
        lines.push(Line::from(vec![
            Span::styled(
                format!("! {:?}", execution.profile).to_ascii_lowercase(),
                Style::default().fg(Color::Magenta),
            ),
            Span::raw(format!(
                "  {}  [{}, {}]",
                execution.command, execution_id, execution.state
            )),
        ]));
        lines.extend(
            execution
                .output
                .lines()
                .map(|line| Line::raw(line.to_owned())),
        );
        lines.push(Line::raw(""));
    }
    for notice in &state.notices {
        lines.push(Line::styled(
            format!("· {notice}"),
            Style::default().fg(Color::Yellow),
        ));
    }
    let scroll = lines.len().saturating_sub(area.height as usize) as u16;
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
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
        ComposerMode::Shell(ExecutionProfile::Workspace) => Style::default().fg(Color::Magenta),
        ComposerMode::Shell(ExecutionProfile::Host) => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
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

fn sanitize_terminal_output(bytes: &[u8]) -> String {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return format!("⟦binary {} bytes⟧", bytes.len());
    };
    let mut output = String::new();
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\u{1b}' {
            match chars.next() {
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    let mut escaped = false;
                    for next in chars.by_ref() {
                        if next == '\u{7}' || (escaped && next == '\\') {
                            break;
                        }
                        escaped = next == '\u{1b}';
                    }
                }
                Some(_) | None => {}
            }
        } else if character == '\n' || character == '\t' || !character.is_control() {
            output.push(character);
        } else if character == '\r' {
            output.push('\n');
        }
    }
    output
}

struct TuiTerminalMode;

impl TuiTerminalMode {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable terminal raw mode")?;
        if let Err(error) = execute!(io::stdout(), EnableBracketedPaste) {
            let _ = disable_raw_mode();
            return Err(error).context("failed to enable bracketed paste");
        }
        Ok(Self)
    }
}

impl Drop for TuiTerminalMode {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableBracketedPaste, Show);
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sigils_switch_mode_and_double_sigils_escape_to_prompt() {
        let mut composer = Composer::default();
        composer.insert_char('!');
        assert_eq!(
            composer.mode,
            ComposerMode::Shell(ExecutionProfile::Workspace)
        );
        composer.insert_char('!');
        composer.insert_text("literal");
        assert_eq!(composer.mode, ComposerMode::Prompt);
        assert_eq!(composer.buffer, "!literal");

        composer.reset();
        composer.insert_char('/');
        assert_eq!(composer.mode, ComposerMode::Command);
        composer.insert_char('/');
        assert_eq!(composer.mode, ComposerMode::Prompt);
        assert_eq!(composer.buffer, "/");
    }

    #[test]
    fn shell_profile_and_background_are_state_not_command_text() {
        let mut composer = Composer::default();
        composer.insert_char('!');
        composer.insert_text("printf test &");
        composer.toggle_shell_profile();
        assert_eq!(
            composer.submit(true),
            Some(ComposerSubmission::Shell {
                command: "printf test &".to_owned(),
                profile: ExecutionProfile::Host,
                background: true,
            })
        );
        assert_eq!(composer.mode, ComposerMode::Prompt);
    }

    #[test]
    fn pasted_leading_sigils_remain_prompt_text() {
        let mut composer = Composer::default();
        composer.insert_paste("!printf pasted\n/also-literal");
        assert_eq!(composer.mode, ComposerMode::Prompt);
        assert_eq!(composer.buffer, "!printf pasted\n/also-literal");
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
    fn terminal_sanitizer_removes_control_sequences_and_marks_binary() {
        assert_eq!(
            sanitize_terminal_output(b"ok\x1b]52;c;secret\x07\x1b[31m red\x1b[0m"),
            "ok red"
        );
        assert_eq!(sanitize_terminal_output(&[0xff, 0]), "⟦binary 2 bytes⟧");
    }

    #[test]
    fn command_lexer_handles_quotes_and_escapes_without_shell_expansion() {
        assert_eq!(
            lex_command("cd --host 'dir with spaces'/child\\ name").unwrap(),
            vec!["cd", "--host", "dir with spaces/child name"]
        );
        assert_eq!(
            lex_command("workspace \"$HOME/*.rs\"").unwrap()[1],
            "$HOME/*.rs"
        );
        assert!(lex_command("cd 'unfinished").is_err());
        assert!(lex_command("cd trailing\\").is_err());
    }

    #[test]
    fn daemon_connection_errors_distinguish_missing_and_incompatible_servers() {
        let socket = Path::new("/tmp/agentlibre-test.sock");
        let missing = interactive_connect_error(socket, ClientError::Io("refused".to_owned()));
        assert!(missing.to_string().contains("daemon is unavailable"));

        let incompatible =
            interactive_connect_error(socket, ClientError::Json("old schema".to_owned()));
        assert!(incompatible.to_string().contains("incompatible protocol"));
        assert!(format!("{incompatible:#}").contains("old schema"));
    }

    #[test]
    fn private_history_separates_prompt_and_shell_and_hides_workspace_path() {
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
        history.record(HistoryMode::Shell, "printf secret").unwrap();
        let history_root = history.root.clone().unwrap();
        assert!(!history_root.to_string_lossy().contains("workspace"));
        assert_eq!(history.prompt, vec!["hello"]);
        assert_eq!(history.shell, vec!["printf secret"]);
        let (reloaded, warnings) = InputHistory::load(&state_dir, workspace, true);
        assert!(warnings.is_empty());
        assert_eq!(reloaded.prompt, vec!["hello"]);
        assert_eq!(reloaded.shell, vec!["printf secret"]);
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
