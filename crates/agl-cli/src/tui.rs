use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal as _};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use self::composer::{Composer, ComposerMode, MAX_COMPOSER_BYTES};
use self::reducer::{UiEffect, UiEvent, update};
use self::render_model::{ComposerRenderModel, PickerRenderModel, view};
use self::terminal_filter::TerminalOutputFilter;
use self::terminal_input::{RawTerminalInputGate, TerminalInputAction};
#[cfg(target_os = "linux")]
use self::terminal_view::RawTtyInput;
use agl_client::{
    AgentLibreClient, ClientError, ExecutionAttachment, ExecutionAttachmentEvent,
    PresentationSubscription, PresentationSubscriptionEvent, RunSubscriptionEvent,
};
use agl_ids::{MessageId, RequestId, RunId, SessionId};
use agl_protocol::{
    ActiveRunView, ApplicationAction, ApplicationActionRequest, ApplicationToolResult,
    ClientEffectKind, CommandAvailability, CommandCatalogRequest, CommandDescriptor,
    CommandSuggestion, CommandSuggestionsRequest, ExecutionId, ExecutionProfile,
    ExecutionStatusRequest, ExecutionView, HostStartupPolicy, HumanHostTerminalEnsureRequest,
    HumanTerminalCommandSubmitRequest, HumanTerminalEnsureRequest, HumanTerminalEnsuredEvent,
    KillMode, ProcessBytes, ProtocolRunState, ProtocolToolMode, RunBudgetRequest, RunSubmitRequest,
    RunSubscribeRequest, RunSubscriptionFinishedEvent, SessionLaunchOptions,
    SessionPresentationItem, SessionPresentationRequest, SessionPresentationSnapshot,
    SessionPresentationSubscribeRequest, SessionSelector, StructuredEnvironmentOverlay,
    TerminalOwnerView, TerminalPromptState, TerminalSessionView, TerminalSize, WriterLeaseId,
};
use agl_runtime::AgentLibreRuntimeConfig;
use agl_terminal::TerminalId;
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
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{TerminalOptions, Viewport};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::sync::mpsc;

use crate::args::InteractiveOptions;
use crate::{
    InferenceAuthorityDecision, InferenceAuthoritySurface, classify_daemon_connection,
    inference_authority_decision,
};

mod composer;
mod reducer;
mod render_model;
pub(crate) mod terminal_filter;
mod terminal_input;
mod terminal_view;

const UI_EVENT_CAPACITY: usize = 256;
const MAX_HISTORY_ENTRIES: usize = 1_000;
const MAX_HISTORY_BYTES: usize = 1024 * 1024;
const MAX_LIVE_ASSISTANT_DELTAS: usize = 8;
const MAX_LIVE_ASSISTANT_DELTA_BYTES: usize = 1024 * 1024;
const MAX_PICKER_ENTRIES: usize = 256;
const MAX_PICKER_PAGES: usize = 8;
const MAX_RUN_FINISHED_NOTICE_BYTES: usize = 4 * 1024;
const CHAT_FRAME_INTERVAL: Duration = Duration::from_micros(16_667);
const CHAT_INPUT_POLL_INTERVAL: Duration = Duration::from_millis(20);
const SHELL_STARTUP_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const SHELL_STARTUP_OBSERVE_INTERVAL: Duration = Duration::from_millis(25);

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

#[derive(Clone, Debug, Eq, PartialEq)]
enum ComposerSubmission {
    Prompt(String),
    Shell(String),
    SwitchTerminal,
    Command(String),
    Picker(PickerSubmit),
}

impl Composer {
    fn submit(&mut self) -> Option<ComposerSubmission> {
        if self.buffer.trim().is_empty() {
            if self.mode == ComposerMode::Shell {
                self.reset();
                return Some(ComposerSubmission::SwitchTerminal);
            }
            return None;
        }
        if self.mode == ComposerMode::Shell {
            return Some(ComposerSubmission::Shell(self.buffer.clone()));
        }
        let text = self.buffer.trim_end_matches(['\r', '\n']).to_owned();
        let submission = match self.mode {
            ComposerMode::Prompt => ComposerSubmission::Prompt(text),
            ComposerMode::Command => ComposerSubmission::Command(text),
            ComposerMode::Shell => unreachable!("Shell submission returns without clearing"),
        };
        self.reset();
        Some(submission)
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
    fn load(state_dir: &Path, workspace_history_scope: &str, enabled: bool) -> (Self, Vec<String>) {
        if !enabled {
            return (
                Self {
                    root: None,
                    prompt: Vec::new(),
                },
                Vec::new(),
            );
        }
        let digest = Sha256::digest(workspace_history_scope.as_bytes())
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
        let prompt = read_history_file(&root.join("prompt.jsonl"), &mut warnings);
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
            ComposerMode::Shell | ComposerMode::Command => &[],
        }
    }

    fn record_prompt(&mut self, input: &str) -> Result<()> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        let entries = &mut self.prompt;
        if entries.last().is_some_and(|last| last == input) {
            return Ok(());
        }
        entries.push(input.to_owned());
        if entries.len() > MAX_HISTORY_ENTRIES {
            entries.drain(..entries.len() - MAX_HISTORY_ENTRIES);
        }
        let path = root.join("prompt.jsonl");
        let lock_path = root.join("prompt.lock");
        let lock = open_private_file(&lock_path, false)?;
        lock.lock_exclusive()
            .context("failed to lock input history")?;
        let record = HistoryRecord {
            schema: "agentlibre.cli.input_history.v1".to_owned(),
            timestamp_unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            mode: "prompt".to_owned(),
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
            compact_history(&path, entries)?;
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

fn read_history_file(path: &Path, warnings: &mut Vec<String>) -> Vec<String> {
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
            && record.mode == "prompt"
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

fn compact_history(path: &Path, entries: &[String]) -> Result<()> {
    let temporary = path.with_extension(format!("jsonl.tmp-{}", std::process::id()));
    let mut file = open_private_file(&temporary, false)?;
    file.set_len(0)?;
    for input in entries {
        let record = HistoryRecord {
            schema: "agentlibre.cli.input_history.v1".to_owned(),
            timestamp_unix_ms: 0,
            mode: "prompt".to_owned(),
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
        terminal_id: TerminalId,
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

struct InteractiveState {
    snapshot: SessionPresentationSnapshot,
    catalog: Vec<CommandDescriptor>,
    composer: Composer,
    last_terminal: Option<TerminalId>,
    terminal_cursors: BTreeMap<ExecutionId, u64>,
    seen_terminals: BTreeSet<TerminalId>,
    assistant_deltas: BTreeMap<MessageId, AssistantDeltaState>,
    continuation_submission_ids: BTreeMap<MessageId, String>,
    picker: Option<PickerState>,
    notices: Vec<String>,
    active_run: Option<agl_ids::RunId>,
    exit_armed: bool,
    workspace_change_armed: Option<String>,
    shell_profile_id: Option<String>,
    history: InputHistory,
    activity_expanded: bool,
    pending_shell_submission: Option<PendingShellSubmission>,
    no_color: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingShellSubmission {
    command: String,
    client_submission_id: String,
    terminal_ensure_submission_id: String,
    in_flight: bool,
    outcome_uncertain: bool,
}

#[derive(Clone)]
struct ShellSubmissionTask {
    session_id: SessionId,
    command: String,
    client_submission_id: String,
    terminal_ensure_submission_id: String,
    execution_context_revision: u64,
    shell_profile_id: Option<String>,
    terminal_size: TerminalSize,
    agl_env: StructuredEnvironmentOverlay,
    selected_terminal: Option<TerminalSessionView>,
    reusable_writer_lease_id: Option<WriterLeaseId>,
    attach_after_sequence: u64,
}

struct ShellSubmissionAttachment {
    terminal: TerminalSessionView,
    attachment: ExecutionAttachment,
    after_sequence: u64,
}

struct ShellSubmissionFailure {
    message: String,
    outcome_uncertain: bool,
}

struct ShellSubmissionCompletion {
    session_id: SessionId,
    command: String,
    client_submission_id: String,
    terminal: Option<TerminalSessionView>,
    attachment: Option<ShellSubmissionAttachment>,
    outcome: std::result::Result<
        agl_protocol::HumanTerminalCommandAcceptedEvent,
        ShellSubmissionFailure,
    >,
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

impl InteractiveState {
    fn latest_available_incomplete(&self) -> Option<MessageId> {
        self.snapshot.items.iter().rev().find_map(|item| {
            let SessionPresentationItem::IncompleteAssistant { item } = item else {
                return None;
            };
            matches!(
                item.continue_action,
                agl_protocol::ContinueActionView::Available
            )
            .then(|| item.message_id.clone())
        })
    }

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

type UiState = InteractiveState;

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
    ShellSubmission(Box<ShellSubmissionCompletion>),
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
    let connection = crate::runtime::connect_daemon(&socket_path).await;
    let authority = inference_authority_decision(
        InferenceAuthoritySurface::Interactive,
        classify_daemon_connection(&connection),
    );
    let client = match (authority, connection) {
        (InferenceAuthorityDecision::Daemon, Ok(client)) => client,
        (InferenceAuthorityDecision::Reject, Err(error)) => {
            return Err(interactive_connect_error(&socket_path, error));
        }
        _ => unreachable!("daemon connection classification and authority decision diverged"),
    };
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
        &presentation.snapshot.header.workspace_history_scope,
        options.input_history,
    );
    let mut notices =
        vec!["Type ! for Shell commands, / for product commands, Ctrl+D to disconnect".to_owned()];
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
        continuation_submission_ids: BTreeMap::new(),
        picker: None,
        notices,
        active_run: None,
        exit_armed: false,
        workspace_change_armed: None,
        shell_profile_id: managed_shell_profile_id(&runtime.execution.shell.program)
            .map(str::to_owned),
        history,
        activity_expanded: false,
        pending_shell_submission: None,
        no_color: std::env::var_os("NO_COLOR").is_some(),
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
                                UiControl::ContinueIncomplete(message_id) => {
                                    if let Err(error) = continue_incomplete_output(
                                        &client,
                                        &session_id,
                                        &mut state,
                                        message_id,
                                    )
                                    .await
                                    {
                                        state.notice(format!(
                                            "Continue failed; retry keeps the same request identity: {error:#}"
                                        ));
                                    }
                                }
                                UiControl::Notice(message) => state.notice(message),
                                UiControl::Submission(submission) => {
                                    if let ComposerSubmission::Shell(command) = &submission {
                                        match begin_shell_submission(
                                            &session_id,
                                            &mut state,
                                            command.clone(),
                                            &terminal_stream,
                                        ) {
                                            Ok(Some(task)) => {
                                                spawn_shell_submission(
                                                    client.clone(),
                                                    task,
                                                    async_sender.clone(),
                                                );
                                            }
                                            Ok(None) => state.notice(
                                                "Shell command admission is already pending",
                                            ),
                                            Err(error) => state.notice(format!(
                                                "Shell submission was not started: {error:#}"
                                            )),
                                        }
                                        continue;
                                    }
                                    match handle_submission(
                                        &client,
                                        &session_id,
                                        &mut state,
                                        submission,
                                        &async_sender,
                                    ).await {
                                        Err(error) => state.notice(format!(
                                            "submission failed; session remains active: {error:#}"
                                        )),
                                        Ok(SubmissionOutcome::Continue) => {}
                                        Ok(SubmissionOutcome::Disconnect) => break Ok(()),
                                        Ok(SubmissionOutcome::EnterTerminal(request)) => {
                                            pending_terminal = Some(request);
                                        }
                                        Ok(SubmissionOutcome::SwitchSession { session_id: next_session_id }) => {
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
                            let _ = update(&mut state, UiEvent::Paste(text));
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
                        let outcome = apply_presentation_event(&mut state, event.event.clone());
                        if outcome.resync_required {
                            state.notice("presentation delta gap; installing a fresh snapshot");
                            resubscribe_presentation(&client, &mut state, &mut presentation)
                                .await?;
                        } else if outcome.command_catalog_changed {
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
                    apply_async_event(
                        &mut state,
                        &session_id,
                        event,
                        Some(&mut terminal_stream),
                    );
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
        ApplicationToolResult::SessionOpened { session_id, .. } => Ok(session_id),
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
        ClientError::DaemonUnavailable(_) => missing_daemon_message(socket_path),
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
    ContinueIncomplete(MessageId),
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
    let (history, warnings) = InputHistory::load(
        state_dir,
        &snapshot.header.workspace_history_scope,
        input_history,
    );
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
    state.continuation_submission_ids.clear();
    state.picker = None;
    state.active_run = state
        .snapshot
        .active_run
        .as_ref()
        .map(|active| active.run_id.clone());
    state.exit_armed = false;
    state.workspace_change_armed = None;
    state.pending_shell_submission = None;
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
    state.continuation_submission_ids.retain(|message_id, _| {
        state.snapshot.items.iter().any(|item| {
            matches!(
                item,
                SessionPresentationItem::IncompleteAssistant { item }
                    if &item.message_id == message_id
            )
        })
    });
}

type TerminalPhysicalIo<'a> = (
    &'a mut Terminal<CrosstermBackend<io::Stdout>>,
    &'a mut tokio::signal::unix::Signal,
    &'a mut tokio::signal::unix::Signal,
    &'a mut tokio::signal::unix::Signal,
    &'a mut tokio::signal::unix::Signal,
);

fn shell_submission_allows_edit(state: &mut UiState) -> bool {
    let Some(pending) = state.pending_shell_submission.as_ref() else {
        return true;
    };
    if pending.in_flight || pending.outcome_uncertain {
        state.notice(if pending.outcome_uncertain {
            "Shell command outcome is uncertain; retry with Enter keeps the same request identity"
        } else {
            "Shell command admission is pending; the exact command remains read-only"
        });
        return false;
    }
    state.pending_shell_submission = None;
    true
}

fn handle_key(state: &mut UiState, key: KeyEvent) -> Option<UiControl> {
    if state.picker.is_some() {
        return handle_picker_key(state, key);
    }
    update(state, UiEvent::Key(key))
        .into_iter()
        .next()
        .map(|effect| match effect {
            UiEffect::Disconnect => UiControl::Disconnect,
            UiEffect::CancelRun(run_id) => UiControl::CancelRun(run_id),
            UiEffect::ContinueIncomplete(message_id) => UiControl::ContinueIncomplete(message_id),
            UiEffect::SubmitPrompt(prompt) => {
                UiControl::Submission(ComposerSubmission::Prompt(prompt))
            }
            UiEffect::SubmitHumanTerminalCommand(command) => {
                UiControl::Submission(ComposerSubmission::Shell(command))
            }
            UiEffect::AttachHumanTerminal => {
                UiControl::Submission(ComposerSubmission::SwitchTerminal)
            }
            UiEffect::InvokeCommand(command) => {
                UiControl::Submission(ComposerSubmission::Command(command))
            }
            UiEffect::SubmitPicker(picker) => {
                UiControl::Submission(ComposerSubmission::Picker(picker))
            }
            UiEffect::Notice(message) => UiControl::Notice(message),
        })
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

fn canonical_human_command(command: &str) -> Result<String> {
    let command = command.replace("\r\n", "\n");
    if command.contains('\r') {
        bail!("Shell command contains a lone carriage return");
    }
    Ok(command)
}

fn selected_live_human_terminal(state: &UiState) -> Option<TerminalSessionView> {
    state
        .last_terminal
        .as_ref()
        .and_then(|terminal_id| {
            state.snapshot.terminals.iter().find(|terminal| {
                &terminal.terminal_id == terminal_id
                    && terminal.process_state.is_live()
                    && matches!(terminal.owner, TerminalOwnerView::Human { .. })
            })
        })
        .or_else(|| {
            state.snapshot.terminals.iter().find(|terminal| {
                terminal.process_state.is_live()
                    && matches!(terminal.owner, TerminalOwnerView::Human { .. })
            })
        })
        .cloned()
}

fn begin_shell_submission(
    session_id: &SessionId,
    state: &mut UiState,
    command: String,
    terminal_stream: &Option<TerminalStreamState>,
) -> Result<Option<ShellSubmissionTask>> {
    let command = canonical_human_command(&command)?;
    if let Some(pending) = state.pending_shell_submission.as_mut() {
        if pending.command != command {
            bail!("Shell command changed while its submission identity was retained");
        }
        if pending.in_flight {
            return Ok(None);
        }
        pending.in_flight = true;
    } else {
        state.pending_shell_submission = Some(PendingShellSubmission {
            command: command.clone(),
            client_submission_id: format!("cli-shell-{}", RequestId::generate()),
            terminal_ensure_submission_id: format!("cli-terminal-{}", RequestId::generate()),
            in_flight: true,
            outcome_uncertain: false,
        });
    }

    let pending = state
        .pending_shell_submission
        .as_ref()
        .expect("pending Shell submission was installed")
        .clone();
    let selected_terminal = selected_live_human_terminal(state);
    if selected_terminal.is_none() && state.shell_profile_id.is_none() {
        if let Some(pending) = state.pending_shell_submission.as_mut() {
            pending.in_flight = false;
        }
        bail!("configured shell is not an admitted managed Bash/Zsh profile");
    }
    let reusable_writer_lease_id = selected_terminal.as_ref().and_then(|terminal| {
        terminal_stream.as_ref().and_then(|stream| {
            (stream.terminal.terminal_id == terminal.terminal_id && stream.writable)
                .then(|| stream.attachment.writer_lease_id().cloned())
                .flatten()
        })
    });
    let attach_after_sequence = selected_terminal.as_ref().map_or(0, |terminal| {
        terminal_stream
            .as_ref()
            .filter(|stream| stream.terminal.terminal_id == terminal.terminal_id)
            .map(|stream| stream.drained_cursor)
            .or_else(|| state.terminal_cursors.get(&terminal.execution_id).copied())
            .unwrap_or_default()
    });
    Ok(Some(ShellSubmissionTask {
        session_id: session_id.clone(),
        command,
        client_submission_id: pending.client_submission_id,
        terminal_ensure_submission_id: pending.terminal_ensure_submission_id,
        execution_context_revision: state.snapshot.header.execution_context_revision,
        shell_profile_id: state.shell_profile_id.clone(),
        terminal_size: current_terminal_size(),
        agl_env: current_terminal_environment(),
        selected_terminal,
        reusable_writer_lease_id,
        attach_after_sequence,
    }))
}

fn shell_submission_failure(
    task: &ShellSubmissionTask,
    terminal: Option<TerminalSessionView>,
    attachment: Option<ShellSubmissionAttachment>,
    message: impl Into<String>,
    outcome_uncertain: bool,
) -> ShellSubmissionCompletion {
    ShellSubmissionCompletion {
        session_id: task.session_id.clone(),
        command: task.command.clone(),
        client_submission_id: task.client_submission_id.clone(),
        terminal,
        attachment,
        outcome: Err(ShellSubmissionFailure {
            message: message.into(),
            outcome_uncertain,
        }),
    }
}

fn shell_submit_outcome_uncertain(error: &ClientError) -> bool {
    !matches!(
        error,
        ClientError::Protocol { .. }
            | ClientError::InputBackpressure
            | ClientError::FrameTooLarge
            | ClientError::InvalidRequest(_)
    )
}

async fn execute_shell_submission(
    client: AgentLibreClient,
    task: ShellSubmissionTask,
) -> ShellSubmissionCompletion {
    let (mut terminal, newly_created) = if let Some(terminal) = task.selected_terminal.clone() {
        (terminal, false)
    } else {
        let Some(shell_profile_id) = task.shell_profile_id.clone() else {
            return shell_submission_failure(
                &task,
                None,
                None,
                "configured shell is not an admitted managed Bash/Zsh profile",
                false,
            );
        };
        match client
            .ensure_human_terminal(HumanTerminalEnsureRequest {
                session_id: task.session_id.clone(),
                client_submission_id: task.terminal_ensure_submission_id.clone(),
                execution_context_revision: task.execution_context_revision,
                profile: ExecutionProfile::Workspace,
                shell_profile_id,
                terminal_size: task.terminal_size,
                agl_env: task.agl_env.clone(),
                host_startup: HostStartupPolicy::ManagedOnly,
            })
            .await
        {
            Ok(ensured) => (
                ensured.terminal,
                ensured.disposition == agl_protocol::TerminalEnsureDisposition::Created,
            ),
            Err(error) => {
                return shell_submission_failure(
                    &task,
                    None,
                    None,
                    format!("failed to ensure the Human workspace terminal: {error}"),
                    false,
                );
            }
        }
    };

    if terminal.prompt_state == TerminalPromptState::Starting && newly_created {
        let deadline = tokio::time::Instant::now() + SHELL_STARTUP_HANDSHAKE_TIMEOUT;
        while terminal.prompt_state == TerminalPromptState::Starting {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return shell_submission_failure(
                    &task,
                    Some(terminal),
                    None,
                    "new Human terminal did not reach a trusted prompt before the startup deadline",
                    false,
                );
            }
            tokio::time::sleep(SHELL_STARTUP_OBSERVE_INTERVAL.min(deadline - now)).await;
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let snapshot = match tokio::time::timeout(
                remaining,
                client.session_presentation(SessionPresentationRequest {
                    session_id: task.session_id.clone(),
                    page_cursor: None,
                }),
            )
            .await
            {
                Ok(Ok(snapshot)) => snapshot,
                Ok(Err(error)) => {
                    return shell_submission_failure(
                        &task,
                        Some(terminal),
                        None,
                        format!("failed to observe the new Human terminal startup: {error}"),
                        false,
                    );
                }
                Err(_) => {
                    return shell_submission_failure(
                        &task,
                        Some(terminal),
                        None,
                        "new Human terminal startup observation timed out",
                        false,
                    );
                }
            };
            let Some(latest) = snapshot
                .terminals
                .into_iter()
                .find(|candidate| candidate.terminal_id == terminal.terminal_id)
            else {
                return shell_submission_failure(
                    &task,
                    Some(terminal),
                    None,
                    "new Human terminal disappeared during startup",
                    false,
                );
            };
            terminal = latest;
        }
    }

    if terminal.prompt_state != TerminalPromptState::Ready {
        return shell_submission_failure(
            &task,
            Some(terminal),
            None,
            "Shell is busy or owns a foreground program; no bytes were sent. Attach Terminal to interact with it.",
            false,
        );
    }
    let Some(prompt_generation) = terminal.prompt_generation else {
        return shell_submission_failure(
            &task,
            Some(terminal),
            None,
            "Shell prompt readiness is stale; no bytes were sent",
            false,
        );
    };

    let (writer_lease_id, attachment) =
        if let Some(writer_lease_id) = task.reusable_writer_lease_id.clone() {
            (writer_lease_id, None)
        } else {
            let after_sequence = match client
                .execution_status(ExecutionStatusRequest {
                    execution_id: terminal.execution_id.clone(),
                    include_private_command: false,
                })
                .await
            {
                Ok(status) => status.status.last_sequence,
                Err(_) => task.attach_after_sequence,
            };
            match client
                .attach_execution(terminal.execution_id.clone(), after_sequence, true)
                .await
            {
                Ok(attachment) if attachment.writer_lease_id().is_some() => {
                    let writer_lease_id = attachment
                        .writer_lease_id()
                        .expect("guarded writable attachment has a writer lease")
                        .clone();
                    (
                        writer_lease_id,
                        Some(ShellSubmissionAttachment {
                            terminal: terminal.clone(),
                            attachment,
                            after_sequence,
                        }),
                    )
                }
                Ok(attachment) => {
                    return shell_submission_failure(
                        &task,
                        Some(terminal.clone()),
                        Some(ShellSubmissionAttachment {
                            terminal,
                            attachment,
                            after_sequence,
                        }),
                        "Human terminal writer lease is not writable; no bytes were sent",
                        false,
                    );
                }
                Err(error) => {
                    return shell_submission_failure(
                        &task,
                        Some(terminal),
                        None,
                        format!("failed to attach the Human terminal writer: {error}"),
                        false,
                    );
                }
            }
        };

    let outcome = client
        .submit_human_terminal_command(HumanTerminalCommandSubmitRequest {
            session_id: task.session_id.clone(),
            terminal_id: terminal.terminal_id.clone(),
            client_submission_id: task.client_submission_id.clone(),
            writer_lease_id,
            expected_command_sequence: terminal.command_sequence,
            expected_prompt_generation: prompt_generation,
            command: task.command.clone(),
        })
        .await;
    match outcome {
        Ok(accepted) => ShellSubmissionCompletion {
            session_id: task.session_id,
            command: task.command,
            client_submission_id: task.client_submission_id,
            terminal: Some(terminal),
            attachment,
            outcome: Ok(accepted),
        },
        Err(error) => {
            let outcome_uncertain = shell_submit_outcome_uncertain(&error);
            shell_submission_failure(
                &task,
                Some(terminal),
                attachment,
                format!("Shell command was not admitted: {error}"),
                outcome_uncertain,
            )
        }
    }
}

fn spawn_shell_submission(
    client: AgentLibreClient,
    task: ShellSubmissionTask,
    sender: mpsc::Sender<UiAsyncEvent>,
) {
    tokio::spawn(async move {
        let completion = execute_shell_submission(client, task).await;
        let _ = sender
            .send(UiAsyncEvent::ShellSubmission(Box::new(completion)))
            .await;
    });
}

async fn handle_submission(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
    submission: ComposerSubmission,
    sender: &mpsc::Sender<UiAsyncEvent>,
) -> Result<SubmissionOutcome> {
    if let ComposerSubmission::Prompt(input) = &submission
        && let Err(error) = state.history.record_prompt(input)
    {
        state.notice(format!("prompt history write failed: {error:#}"));
    }
    match submission {
        ComposerSubmission::Prompt(content) => {
            spawn_prompt(client.clone(), session_id.clone(), content, sender.clone());
        }
        ComposerSubmission::Shell(_) => {
            unreachable!("Shell submissions use the nonblocking admission path")
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
            ApplicationToolResult::SessionOpened { session_id, .. } => {
                Ok(SubmissionOutcome::SwitchSession { session_id })
            }
            ApplicationToolResult::ModelChanged { header } => {
                state.snapshot.header = header;
                reload_command_catalog(client, state).await?;
                state.notice("model selection changed");
                Ok(SubmissionOutcome::Continue)
            }
            ApplicationToolResult::ModeChanged { header } => {
                state.snapshot.header = header;
                reload_command_catalog(client, state).await?;
                state.notice("operation mode changed");
                Ok(SubmissionOutcome::Continue)
            }
            ApplicationToolResult::SkillsChanged { header } => {
                state.snapshot.header = header;
                reload_command_catalog(client, state).await?;
                state.notice("skill selection changed");
                Ok(SubmissionOutcome::Continue)
            }
            ApplicationToolResult::AttachAccepted {
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
            ApplicationToolResult::KillAccepted { execution_id, mode } => {
                state.notice(format!(
                    "execution {execution_id} termination requested ({mode:?})"
                ));
                Ok(SubmissionOutcome::Continue)
            }
            ApplicationToolResult::TerminalPromoted { terminal } => {
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
        "\r! agentLIBRE Terminal · {} · {} · {} · ! Enter at prompt · Esc then ! in foreground → Chat\r",
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
                        let outcome = apply_presentation_event(state, event.event.clone());
                        if outcome.resync_required {
                            state.notice("presentation delta gap; installing a fresh snapshot");
                            resubscribe_presentation(client, state, presentation).await?;
                        } else if outcome.command_catalog_changed {
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
                    apply_async_event(state, &session_id, event, None);
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
    terminal_id: &TerminalId,
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
        _ => None,
    }
}

fn terminal_prompt_from_snapshot(
    snapshot: &SessionPresentationSnapshot,
    terminal_id: &TerminalId,
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
    install_presentation_snapshot(state, replacement.snapshot.clone());
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
                if let Some(notice) = run_finished_notice(&finished) {
                    let _ = sender.send(UiAsyncEvent::Notice(notice)).await;
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

fn run_finished_notice(finished: &RunSubscriptionFinishedEvent) -> Option<String> {
    let prefix = match finished.state {
        ProtocolRunState::Succeeded => return None,
        ProtocolRunState::Incomplete => "turn incomplete".to_owned(),
        ProtocolRunState::Failed => "turn failed".to_owned(),
        ProtocolRunState::Cancelled => "turn cancelled".to_owned(),
        state => format!("turn finished: {state:?}"),
    };
    let message_budget = MAX_RUN_FINISHED_NOTICE_BYTES.saturating_sub(prefix.len() + 2);
    if let Some(message) = finished
        .error_message
        .as_deref()
        .and_then(|message| sanitize_notice_detail(message, message_budget))
    {
        return Some(format!("{prefix}: {message}"));
    }
    let code_budget = MAX_RUN_FINISHED_NOTICE_BYTES.saturating_sub(prefix.len() + 3);
    if let Some(code) = finished
        .error_code
        .as_deref()
        .and_then(|code| sanitize_notice_detail(code, code_budget))
    {
        return Some(format!("{prefix} ({code})"));
    }
    Some(prefix)
}

fn sanitize_notice_detail(value: &str, maximum_bytes: usize) -> Option<String> {
    const ELLIPSIS: &str = "…";

    let value = value.trim();
    if value.is_empty() || maximum_bytes == 0 {
        return None;
    }
    let mut output = String::new();
    let mut truncated = false;
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        let fragment = if character.is_control() || is_unicode_format_control(character as u32) {
            format!("\\u{{{:X}}}", character as u32)
        } else {
            character.to_string()
        };
        let truncation_reserve = if characters.peek().is_some() {
            ELLIPSIS.len()
        } else {
            0
        };
        if output
            .len()
            .saturating_add(fragment.len())
            .saturating_add(truncation_reserve)
            > maximum_bytes
        {
            truncated = true;
            break;
        }
        output.push_str(&fragment);
    }
    if truncated && output.len().saturating_add(ELLIPSIS.len()) <= maximum_bytes {
        output.push_str(ELLIPSIS);
    }
    (!output.is_empty()).then_some(output)
}

fn is_unicode_format_control(code: u32) -> bool {
    matches!(
        code,
        0x00ad
            | 0x061c
            | 0x06dd
            | 0x070f
            | 0x180e
            | 0xfeff
            | 0x110bd
            | 0x110cd
            | 0xe0001
            | 0x0600..=0x0605
            | 0x0890..=0x0891
            | 0x08e2
            | 0x200b..=0x200f
            | 0x202a..=0x202e
            | 0x2060..=0x2064
            | 0x2066..=0x206f
            | 0xfff9..=0xfffb
            | 0x13430..=0x1343f
            | 0x1bca0..=0x1bca3
            | 0x1d173..=0x1d17a
    )
}

async fn continue_incomplete_output(
    client: &AgentLibreClient,
    session_id: &SessionId,
    state: &mut UiState,
    message_id: MessageId,
) -> Result<()> {
    let available = state.snapshot.items.iter().any(|item| {
        matches!(
            item,
            SessionPresentationItem::IncompleteAssistant { item }
                if item.message_id == message_id
                    && matches!(
                        item.continue_action,
                        agl_protocol::ContinueActionView::Available
                    )
        )
    });
    if !available {
        state.notice("incomplete output is no longer available to continue");
        return Ok(());
    }
    let client_submission_id = state
        .continuation_submission_ids
        .entry(message_id.clone())
        .or_insert_with(|| format!("cli-incomplete-continue-{}", agl_ids::RequestId::generate()))
        .clone();
    let response = client
        .application_action(ApplicationActionRequest {
            session_id: Some(session_id.clone()),
            client_submission_id,
            action: ApplicationAction::IncompleteTurnContinue {
                message_id: message_id.clone(),
                expected_execution_context_revision: state
                    .snapshot
                    .header
                    .execution_context_revision,
            },
        })
        .await
        .context("daemon rejected incomplete-output continuation")?;
    let ApplicationToolResult::IncompleteTurnContinued { admission } = response.result else {
        bail!("daemon returned an invalid incomplete-output continuation result")
    };
    if admission.session_id != *session_id {
        bail!("daemon returned a continuation for a different session");
    }
    for item in &mut state.snapshot.items {
        if let SessionPresentationItem::IncompleteAssistant { item } = item
            && item.message_id == message_id
        {
            item.continue_action = agl_protocol::ContinueActionView::Claimed {
                continuation_run_id: admission.run_id.clone(),
            };
        }
    }
    if admission.state == agl_protocol::PromptAdmissionState::Running {
        state.active_run = Some(admission.run_id.clone());
    }
    state.notice(format!(
        "Continue admitted as {} ({:?}, position {})",
        admission.run_id, admission.state, admission.ordinal
    ));
    Ok(())
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
    let ApplicationToolResult::Terminals { terminals } = terminals.result else {
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
    let ApplicationToolResult::Executions { executions } = executions.result else {
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
                cwd: display_path(&terminal.cwd),
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
            cwd: display_path(&execution.cwd),
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
                display_path(&terminal.cwd),
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
                // A presentation-only display path must never round-trip into
                // authority. The daemon inherits the source session's
                // canonical workspace for this session-scoped action.
                workspace_root: None,
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
            ApplicationToolResult::SessionOpened { session_id, .. } => {
                return Ok(CommandOutcome::SwitchSession { session_id });
            }
            ApplicationToolResult::SessionExited { .. } => {
                return Ok(CommandOutcome::Disconnect);
            }
            ApplicationToolResult::Status { header } => {
                let notice = match name.as_str() {
                    "workspace" => Some(display_path(&header.workspace_root)),
                    _ => None,
                };
                state.snapshot.header = header;
                if let Some(notice) = notice {
                    state.notice(notice);
                }
            }
            ApplicationToolResult::WorkspaceChanged { header } => {
                state.snapshot.header = header;
                state.workspace_change_armed = None;
            }
            ApplicationToolResult::ModelChanged { header }
            | ApplicationToolResult::ModeChanged { header }
            | ApplicationToolResult::SkillsChanged { header } => {
                state.snapshot.header = header;
            }
            ApplicationToolResult::Executions { executions } => {
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
            ApplicationToolResult::AttachAccepted { .. } => {
                let (terminal, writable) = attach_candidate
                    .context("daemon accepted an attachment for an unknown terminal")?;
                state.last_terminal = Some(terminal.terminal_id.clone());
                return Ok(CommandOutcome::EnterTerminal(Box::new(
                    TerminalViewRequest { terminal, writable },
                )));
            }
            ApplicationToolResult::Cleared { .. } => {
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

fn install_shell_submission_attachment(
    state: &mut UiState,
    terminal_stream: &mut Option<TerminalStreamState>,
    returned: ShellSubmissionAttachment,
) {
    finish_terminal_stream(terminal_stream, state);
    state
        .seen_terminals
        .insert(returned.terminal.terminal_id.clone());
    state.terminal_cursors.insert(
        returned.terminal.execution_id.clone(),
        returned.after_sequence,
    );
    let writable = returned.attachment.started.writable;
    *terminal_stream = Some(TerminalStreamState {
        terminal: returned.terminal,
        attachment: returned.attachment,
        filter: TerminalOutputFilter::new(false),
        visible_cursor: returned.after_sequence,
        drained_cursor: returned.after_sequence,
        hidden_normal_output: false,
        replay_through_cursor: None,
        physical_alternate_screen: Arc::new(AtomicBool::new(false)),
        panic_restore_bytes: Arc::new(Mutex::new(Vec::new())),
        writable,
    });
}

fn apply_shell_submission_completion(
    state: &mut UiState,
    session_id: &SessionId,
    terminal_stream: Option<&mut Option<TerminalStreamState>>,
    mut completion: ShellSubmissionCompletion,
) {
    let matches_pending = &completion.session_id == session_id
        && state
            .pending_shell_submission
            .as_ref()
            .is_some_and(|pending| {
                pending.command == completion.command
                    && pending.client_submission_id == completion.client_submission_id
            });
    if !matches_pending {
        return;
    }
    if let Some(terminal) = completion.terminal.as_ref() {
        state.last_terminal = Some(terminal.terminal_id.clone());
    }
    if let (Some(terminal_stream), Some(attachment)) =
        (terminal_stream, completion.attachment.take())
    {
        install_shell_submission_attachment(state, terminal_stream, attachment);
    }

    match completion.outcome {
        Ok(accepted) => {
            let accepted_matches = completion
                .terminal
                .as_ref()
                .is_some_and(|terminal| terminal.terminal_id == accepted.terminal_id);
            if !accepted_matches {
                let _ = update(
                    state,
                    UiEvent::ShellRejected {
                        message: "Shell acceptance named a different terminal; exact command and request identity retained".to_owned(),
                        client_submission_id: completion.client_submission_id,
                        outcome_uncertain: true,
                    },
                );
                return;
            }
            state.last_terminal = Some(accepted.terminal_id);
            let _ = update(
                state,
                UiEvent::ShellAccepted {
                    command_sequence: accepted.command_sequence,
                },
            );
        }
        Err(failure) => {
            let _ = update(
                state,
                UiEvent::ShellRejected {
                    message: failure.message,
                    client_submission_id: completion.client_submission_id,
                    outcome_uncertain: failure.outcome_uncertain,
                },
            );
        }
    }
}

fn apply_async_event(
    state: &mut UiState,
    session_id: &SessionId,
    event: UiAsyncEvent,
    terminal_stream: Option<&mut Option<TerminalStreamState>>,
) {
    match event {
        UiAsyncEvent::ShellSubmission(completion) => {
            apply_shell_submission_completion(state, session_id, terminal_stream, *completion)
        }
        UiAsyncEvent::RunAccepted {
            session_id: event_session_id,
            ..
        }
        | UiAsyncEvent::Snapshot {
            session_id: event_session_id,
            ..
        } if &event_session_id != session_id => {}
        UiAsyncEvent::RunAccepted {
            run_id,
            state: run_state,
            ..
        } => {
            let _ = update(
                state,
                UiEvent::RunAccepted {
                    run_id,
                    state: run_state,
                },
            );
        }
        UiAsyncEvent::Snapshot { snapshot, .. } => {
            let _ = update(state, UiEvent::Snapshot(snapshot));
        }
        UiAsyncEvent::Notice(message) => {
            let _ = update(state, UiEvent::Notice(message));
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PresentationApplyOutcome {
    command_catalog_changed: bool,
    resync_required: bool,
}

fn apply_presentation_event(
    state: &mut UiState,
    event: agl_protocol::SessionPresentationEventPayload,
) -> PresentationApplyOutcome {
    let command_catalog_changed = matches!(
        &event,
        agl_protocol::SessionPresentationEventPayload::CommandAvailabilityChanged
    );
    let mut resync_required = false;
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
                AssistantDeltaApply::SequenceGap => {
                    resync_required = true;
                    state.notice("assistant presentation delta gap; fresh snapshot required");
                }
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
        agl_protocol::SessionPresentationEventPayload::HumanCommandCardUpsert { card } => {
            if let Some(existing) = state.snapshot.human_commands.iter_mut().find(|existing| {
                existing.terminal_id == card.terminal_id
                    && existing.command_sequence == card.command_sequence
            }) {
                *existing = card;
            } else {
                state.snapshot.human_commands.push(card);
            }
            while state.snapshot.human_commands.len() > agl_protocol::MAX_HUMAN_COMMAND_CARDS
                || state
                    .snapshot
                    .human_commands
                    .iter()
                    .map(|card| card.output.as_str().len())
                    .sum::<usize>()
                    > agl_protocol::MAX_HUMAN_COMMAND_AGGREGATE_OUTPUT_BYTES
            {
                let position = state
                    .snapshot
                    .human_commands
                    .iter()
                    .position(|card| {
                        matches!(
                            card.state,
                            agl_protocol::HumanCommandCardState::Exited
                                | agl_protocol::HumanCommandCardState::OutcomeUnknown
                        )
                    })
                    .unwrap_or(0);
                state.snapshot.human_commands.remove(position);
            }
        }
        agl_protocol::SessionPresentationEventPayload::HumanCommandCardRemoved {
            terminal_id,
            command_sequence,
        } => state.snapshot.human_commands.retain(|card| {
            card.terminal_id != terminal_id || card.command_sequence != command_sequence
        }),
        agl_protocol::SessionPresentationEventPayload::ActivityGraphDelta { batch } => {
            match apply_activity_graph_delta(state.snapshot.activity.as_ref(), &batch) {
                Ok(graph) => state.snapshot.activity = Some(graph),
                Err(error) => {
                    state.snapshot.activity = None;
                    resync_required = true;
                    state.notice(format!("activity graph needs a fresh snapshot: {error}"));
                }
            }
        }
        agl_protocol::SessionPresentationEventPayload::Notice { message, .. } => {
            state.notice(message)
        }
        _ => {}
    }
    PresentationApplyOutcome {
        command_catalog_changed,
        resync_required,
    }
}

fn apply_activity_graph_delta(
    current: Option<&agl_protocol::ActivityGraphView>,
    batch: &agl_protocol::ActivityGraphDeltaBatch,
) -> std::result::Result<agl_protocol::ActivityGraphView, String> {
    let current_revision = current.map_or(0, |graph| graph.graph_revision);
    if batch.graph_revision == current_revision {
        let duplicate = current.is_some_and(|graph| {
            batch.upserts.iter().all(|node| graph.nodes.contains(node))
                && batch.removals.iter().all(|removal| {
                    graph
                        .nodes
                        .iter()
                        .all(|node| node.node_id != removal.subtree_root_id)
                })
                && batch
                    .current_path
                    .as_ref()
                    .is_none_or(|path| path == &graph.current_path)
                && (!batch.truncated || graph.truncated)
        });
        return duplicate
            .then(|| {
                current
                    .expect("duplicate requires an installed graph")
                    .clone()
            })
            .ok_or_else(|| "same revision carried a different batch".to_owned());
    }
    if batch.graph_revision != current_revision.saturating_add(1) {
        return Err(format!(
            "expected revision {}, received {}",
            current_revision.saturating_add(1),
            batch.graph_revision
        ));
    }
    let mut graph = current.cloned().unwrap_or(agl_protocol::ActivityGraphView {
        graph_revision: 0,
        roots: Vec::new(),
        nodes: Vec::new(),
        current_path: Vec::new(),
        truncated: false,
    });
    for node in &batch.upserts {
        if let Some(existing) = graph
            .nodes
            .iter_mut()
            .find(|existing| existing.node_id == node.node_id)
        {
            *existing = node.clone();
        } else {
            graph.nodes.push(node.clone());
        }
    }
    if let Some(path) = &batch.current_path {
        graph.current_path = path.clone();
    }
    for removal in &batch.removals {
        let mut removed = BTreeSet::from([removal.subtree_root_id.clone()]);
        if graph
            .nodes
            .iter()
            .all(|node| node.node_id != removal.subtree_root_id)
        {
            return Err(format!(
                "removal references unknown node {}",
                removal.subtree_root_id
            ));
        }
        loop {
            let before = removed.len();
            for node in &graph.nodes {
                if node
                    .parent_node_id
                    .as_ref()
                    .is_some_and(|parent| removed.contains(parent))
                {
                    removed.insert(node.node_id.clone());
                }
            }
            if removed.len() == before {
                break;
            }
        }
        if graph.current_path.iter().any(|id| removed.contains(id)) {
            return Err("removal intersects the current activity path".to_owned());
        }
        graph.nodes.retain(|node| !removed.contains(&node.node_id));
    }
    let mut by_id = graph
        .nodes
        .drain(..)
        .map(|node| (node.node_id.clone(), node))
        .collect::<BTreeMap<_, _>>();
    let mut children = BTreeMap::<Option<String>, Vec<String>>::new();
    for node in by_id.values() {
        children
            .entry(node.parent_node_id.clone())
            .or_default()
            .push(node.node_id.clone());
    }
    for ids in children.values_mut() {
        ids.sort_by(|left, right| {
            let left = by_id.get(left).expect("activity child exists");
            let right = by_id.get(right).expect("activity child exists");
            (left.order_index, left.node_id.as_str())
                .cmp(&(right.order_index, right.node_id.as_str()))
        });
    }
    fn visit(
        parent: Option<String>,
        children: &BTreeMap<Option<String>, Vec<String>>,
        by_id: &mut BTreeMap<String, agl_protocol::ActivityNodeView>,
        output: &mut Vec<agl_protocol::ActivityNodeView>,
    ) {
        for id in children.get(&parent).into_iter().flatten() {
            let Some(node) = by_id.remove(id) else {
                continue;
            };
            output.push(node);
            visit(Some(id.clone()), children, by_id, output);
        }
    }
    visit(None, &children, &mut by_id, &mut graph.nodes);
    if !by_id.is_empty() {
        return Err("graph contains a cycle or disconnected nodes".to_owned());
    }
    graph.roots = graph
        .nodes
        .iter()
        .filter(|node| node.parent_node_id.is_none())
        .map(|node| node.node_id.clone())
        .collect();
    graph.graph_revision = batch.graph_revision;
    graph.truncated |= batch.truncated;
    graph.validate().map_err(|error| error.to_string())?;
    Ok(graph)
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
        SessionPresentationItem::IncompleteAssistant { item } => item.message_id.to_string(),
        SessionPresentationItem::AgentAction {
            run_id, step_id, ..
        } => format!("{run_id}:{step_id}"),
        SessionPresentationItem::ContextBoundary { event_id, .. }
        | SessionPresentationItem::Notice { event_id, .. } => event_id.to_string(),
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, state: &UiState) {
    let model = view(state, frame.area());
    frame.render_widget(Paragraph::new(model.header_text.clone()), model.header);
    frame.render_widget(
        Paragraph::new(model.transcript_text.clone())
            .wrap(Wrap { trim: false })
            .scroll((model.transcript_scroll, 0)),
        model.transcript,
    );
    if let (Some(area), Some(text)) = (model.palette, model.palette_text.as_ref()) {
        frame.render_widget(
            Paragraph::new(text.clone())
                .block(Block::default().borders(Borders::ALL).title(" Commands ")),
            area,
        );
    }
    draw_composer(frame, model.composer, &model.composer_content);
    frame.render_widget(
        Paragraph::new(model.footer_text).style(model.footer_style),
        model.footer,
    );
    if let Some(picker) = &model.picker {
        draw_picker(frame, picker);
    }
}

fn draw_picker(frame: &mut ratatui::Frame<'_>, picker: &PickerRenderModel) {
    frame.render_widget(Clear, picker.area);
    frame.render_widget(
        Paragraph::new(picker.text.clone()).block(
            Block::default()
                .borders(Borders::ALL)
                .title(picker.title.clone()),
        ),
        picker.area,
    );
    frame.set_cursor_position(picker.cursor);
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

fn header_text(state: &UiState) -> Text<'static> {
    let header = &state.snapshot.header;
    let model = header.model_id.as_deref().unwrap_or("local");
    let status = state
        .snapshot
        .activity
        .as_ref()
        .and_then(|graph| {
            graph.current_path.last().and_then(|id| {
                graph
                    .nodes
                    .iter()
                    .find(|node| &node.node_id == id)
                    .map(|node| activity_phase_label(node.phase).to_owned())
            })
        })
        .unwrap_or_else(|| {
            if state.active_run.is_some() {
                "working".to_owned()
            } else {
                "ready".to_owned()
            }
        });
    Text::from(vec![
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
            display_path(&header.cwd),
            header.session_id
        )),
    ])
}

#[cfg(test)]
fn draw_transcript(frame: &mut ratatui::Frame<'_>, area: Rect, state: &UiState) {
    draw_transcript_with_activity_mode(frame, area, state, state.no_color);
}

fn transcript_model(
    state: &UiState,
    width: u16,
    height: u16,
    no_color: bool,
) -> (Text<'static>, u16) {
    let presentation_style = |style| {
        if no_color { Style::default() } else { style }
    };
    let mut lines = Vec::new();
    append_activity_lines(&mut lines, state, width, no_color);
    for item in &state.snapshot.items {
        match item {
            SessionPresentationItem::UserMessage { content, .. } => {
                lines.push(Line::styled(
                    "you",
                    presentation_style(Style::default().fg(Color::Cyan)),
                ));
                lines.extend(text_lines(content_text(content)));
            }
            SessionPresentationItem::AssistantMessage { content, .. } => {
                lines.push(Line::styled(
                    "agentLIBRE",
                    presentation_style(Style::default().fg(Color::Green)),
                ));
                lines.extend(text_lines(content_text(content)));
            }
            SessionPresentationItem::IncompleteAssistant { item } => {
                lines.push(Line::styled(
                    "agentLIBRE · incomplete · output limit",
                    presentation_style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                ));
                lines.extend(text_lines(content_text(&item.content)));
                let reason = match item.reason {
                    agl_protocol::IncompleteOutputReason::ModelLength => "model length limit",
                    agl_protocol::IncompleteOutputReason::ContentByteLimit => "content byte limit",
                };
                let action = match &item.continue_action {
                    agl_protocol::ContinueActionView::Available
                        if state.latest_available_incomplete().as_ref()
                            == Some(&item.message_id) =>
                    {
                        "Ctrl+Y Continue".to_owned()
                    }
                    agl_protocol::ContinueActionView::Available => {
                        "Continue available · Ctrl+Y targets the newest incomplete output"
                            .to_owned()
                    }
                    agl_protocol::ContinueActionView::Claimed {
                        continuation_run_id,
                    } => format!("Continue claimed · run {continuation_run_id}"),
                    agl_protocol::ContinueActionView::Unavailable { reason } => match reason {
                        agl_protocol::ContinueUnavailableReason::StaleContext => {
                            "Continue unavailable · context changed".to_owned()
                        }
                        agl_protocol::ContinueUnavailableReason::PolicyDenied => {
                            "Continue unavailable · policy denied".to_owned()
                        }
                        agl_protocol::ContinueUnavailableReason::SessionFinished => {
                            "Continue unavailable · session finished".to_owned()
                        }
                    },
                };
                lines.push(Line::styled(
                    format!("{reason} · {action}"),
                    presentation_style(Style::default().fg(Color::Yellow)),
                ));
            }
            SessionPresentationItem::ContextBoundary { .. } => lines.push(Line::styled(
                "──────── context cleared ────────",
                presentation_style(Style::default().fg(Color::DarkGray)),
            )),
            SessionPresentationItem::Notice { message, .. } => lines.push(Line::styled(
                message.clone(),
                presentation_style(Style::default().fg(Color::Yellow)),
            )),
            SessionPresentationItem::AgentAction { summary, state, .. } => {
                lines.push(Line::styled(
                    format!("agent action · {summary} · {state:?}"),
                    presentation_style(Style::default().fg(Color::Magenta)),
                ))
            }
        }
        lines.push(Line::raw(""));
    }
    for card in &state.snapshot.human_commands {
        let (status, style) = match card.state {
            agl_protocol::HumanCommandCardState::Starting => {
                ("starting", Style::default().fg(Color::Yellow))
            }
            agl_protocol::HumanCommandCardState::Running => {
                ("running", Style::default().fg(Color::Cyan))
            }
            agl_protocol::HumanCommandCardState::Exited if card.exit_status == Some(0) => {
                ("exited", Style::default().fg(Color::Green))
            }
            agl_protocol::HumanCommandCardState::Exited => {
                ("failed", Style::default().fg(Color::Red))
            }
            agl_protocol::HumanCommandCardState::OutcomeUnknown => {
                ("outcome unknown", Style::default().fg(Color::Yellow))
            }
        };
        lines.push(Line::styled(
            format!("! #{} · {status}", card.command_sequence),
            presentation_style(style.add_modifier(Modifier::BOLD)),
        ));
        lines.extend(text_lines(format!("$ {}", card.command.as_str())));
        if !card.output.as_str().is_empty() {
            lines.extend(text_lines(card.output.as_str().to_owned()));
        }
        let mut outcome = match card.state {
            agl_protocol::HumanCommandCardState::Starting => "waiting for shell start".to_owned(),
            agl_protocol::HumanCommandCardState::Running => "running".to_owned(),
            agl_protocol::HumanCommandCardState::Exited => card
                .exit_status
                .map(|status| format!("exit {status}"))
                .unwrap_or_else(|| "exit status unavailable".to_owned()),
            agl_protocol::HumanCommandCardState::OutcomeUnknown => "outcome unknown".to_owned(),
        };
        outcome.push_str(&format!(" · cwd:{}", display_path(&card.cwd)));
        if card.truncated {
            outcome.push_str(" · output truncated");
        }
        if card.filtered_effects > 0 {
            outcome.push_str(&format!(
                " · {} terminal effect(s) filtered",
                card.filtered_effects
            ));
        }
        outcome.push_str(" · empty Shell Enter to Attach");
        lines.push(Line::styled(
            outcome,
            presentation_style(Style::default().fg(Color::DarkGray)),
        ));
        lines.push(Line::raw(""));
    }
    for terminal in &state.snapshot.terminals {
        let authority = terminal_authority_label(terminal.profile);
        lines.push(Line::styled(
            format!(
                "! {} · {authority} · cwd:{} · {:?} · {:?}",
                terminal_owner_label(&terminal.owner),
                display_path(&terminal.cwd),
                terminal.prompt_state,
                terminal.process_state,
            ),
            presentation_style(if terminal.profile == ExecutionProfile::Host {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Magenta)
            }),
        ));
    }
    for delta in state.assistant_deltas.values().filter(|delta| delta.valid) {
        lines.push(Line::styled(
            "agentLIBRE · streaming",
            presentation_style(Style::default().fg(Color::Green)),
        ));
        lines.extend(text_lines(delta.text.clone()));
        lines.push(Line::raw(""));
    }
    for notice in &state.notices {
        lines.push(Line::styled(
            format!("· {notice}"),
            presentation_style(Style::default().fg(Color::Yellow)),
        ));
    }
    let text = Text::from(lines);
    let paragraph = Paragraph::new(text.clone()).wrap(Wrap { trim: false });
    let scroll = paragraph
        .line_count(width)
        .saturating_sub(height as usize)
        .min(u16::MAX as usize) as u16;
    (text, scroll)
}

#[cfg(test)]
fn draw_transcript_with_activity_mode(
    frame: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &UiState,
    no_color: bool,
) {
    let (text, scroll) = transcript_model(state, area.width, area.height, no_color);
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

fn append_activity_lines(
    lines: &mut Vec<Line<'static>>,
    state: &UiState,
    width: u16,
    no_color: bool,
) {
    let Some(graph) = &state.snapshot.activity else {
        return;
    };
    let ascii = width < 50 || no_color;
    let arrow = if ascii { " -> " } else { " → " };
    let current = graph
        .current_path
        .iter()
        .filter_map(|id| graph.nodes.iter().find(|node| &node.node_id == id))
        .map(activity_node_label)
        .collect::<Vec<_>>();
    if !current.is_empty() {
        lines.push(Line::styled(
            format!("activity · {}", current.join(arrow)),
            if no_color {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD)
            },
        ));
    }
    if state.activity_expanded {
        for node in &graph.nodes {
            let prefix = activity_tree_prefix(graph, node, ascii, 8);
            let marker = match node.state {
                agl_protocol::ActivityNodeState::Pending => "○",
                agl_protocol::ActivityNodeState::Waiting => "○",
                agl_protocol::ActivityNodeState::Running => "▶",
                agl_protocol::ActivityNodeState::Succeeded => "✓",
                agl_protocol::ActivityNodeState::Failed => "!",
                agl_protocol::ActivityNodeState::Cancelled => "×",
                agl_protocol::ActivityNodeState::Incomplete => "…",
                agl_protocol::ActivityNodeState::Truncated => "…",
            };
            let marker = if ascii {
                match node.state {
                    agl_protocol::ActivityNodeState::Pending => "o",
                    agl_protocol::ActivityNodeState::Waiting => "o",
                    agl_protocol::ActivityNodeState::Running => ">",
                    agl_protocol::ActivityNodeState::Succeeded => "+",
                    agl_protocol::ActivityNodeState::Failed => "!",
                    agl_protocol::ActivityNodeState::Cancelled => "x",
                    agl_protocol::ActivityNodeState::Incomplete => "...",
                    agl_protocol::ActivityNodeState::Truncated => "...",
                }
            } else {
                marker
            };
            lines.push(Line::styled(
                format!("{prefix}{marker} {}", activity_node_label(node)),
                if no_color {
                    Style::default()
                } else if matches!(
                    node.state,
                    agl_protocol::ActivityNodeState::Failed
                        | agl_protocol::ActivityNodeState::Incomplete
                        | agl_protocol::ActivityNodeState::Truncated
                ) {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ));
        }
        lines.push(Line::styled(
            "Ctrl+G collapse activity",
            if no_color {
                Style::default()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ));
    } else if !graph.nodes.is_empty() {
        for node in graph.nodes.iter().filter(|node| {
            matches!(
                node.state,
                agl_protocol::ActivityNodeState::Waiting
                    | agl_protocol::ActivityNodeState::Failed
                    | agl_protocol::ActivityNodeState::Incomplete
                    | agl_protocol::ActivityNodeState::Truncated
            ) && !graph.current_path.iter().any(|id| id == &node.node_id)
        }) {
            let marker = if ascii { "!" } else { "↳" };
            lines.push(Line::styled(
                format!("{marker} {}", activity_node_label(node)),
                if no_color {
                    Style::default()
                } else {
                    Style::default().fg(Color::Yellow)
                },
            ));
        }
        lines.push(Line::styled(
            if graph.truncated {
                "Ctrl+G expand activity graph · retained history truncated"
            } else {
                "Ctrl+G expand activity graph"
            },
            if no_color {
                Style::default()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ));
    }
    lines.push(Line::raw(""));
}

fn activity_node_label(node: &agl_protocol::ActivityNodeView) -> String {
    let mut label = format!("{} · {}", activity_phase_label(node.phase), node.summary);
    use agl_protocol::{ActivityDetailView as Detail, CapabilityActivityDetail as Capability};
    match &node.detail {
        Detail::None => {}
        Detail::Capability(Capability::FilesystemList {
            path,
            entries,
            completeness,
        }) => label.push_str(&format!(
            " · {} · {entries} entries · {}",
            display_path(path),
            format!("{completeness:?}").to_ascii_lowercase()
        )),
        Detail::Capability(Capability::FilesystemRead { path, bytes }) => {
            label.push_str(&format!(" · {} · {bytes} bytes", display_path(path)));
        }
        Detail::Capability(Capability::RepositorySearch {
            scope,
            matches,
            complete,
        }) => label.push_str(&format!(
            " · {} · {matches} matches · {}",
            display_path(scope),
            if *complete { "complete" } else { "partial" }
        )),
        Detail::Capability(Capability::ProcessExecution {
            profile,
            exit_status,
        }) => label.push_str(&format!(" · {profile:?} · exit {exit_status:?}")),
        Detail::Capability(Capability::PolicyCheck {
            capability_id,
            outcome,
        }) => label.push_str(&format!(
            " · {capability_id} · {}",
            format!("{outcome:?}").to_ascii_lowercase()
        )),
        Detail::Inference(detail) => {
            label.push_str(&format!(
                " · {}",
                format!("{:?}", detail.stage).to_ascii_lowercase()
            ));
            if let Some(completed) = detail.completed {
                let unit = match detail.unit {
                    Some(agl_protocol::InferenceProgressUnit::Tokens) => "tokens",
                    Some(agl_protocol::InferenceProgressUnit::Chunks) => "chunks",
                    None => "units",
                };
                label.push_str(&format!(
                    " · {completed}/{} {unit}",
                    detail.total.unwrap_or(completed),
                ));
            }
            if detail.cache != agl_protocol::ActivityCacheDisposition::NotApplicable {
                label.push_str(&format!(
                    " · {}",
                    format!("{:?}", detail.cache).to_ascii_lowercase()
                ));
            }
        }
        Detail::Aggregate(detail) => label.push_str(&format!(
            " · {} collapsed · {} failed · {} incomplete",
            detail.collapsed_nodes, detail.failed, detail.incomplete
        )),
        Detail::UnknownCapability { capability_id } => {
            label.push_str(&format!(" · {capability_id}"));
        }
    }
    if node.elapsed_ms > 0 {
        label.push_str(&format!(" · {:.1}s", node.elapsed_ms as f64 / 1000.0));
    }
    label
}

fn activity_tree_prefix(
    graph: &agl_protocol::ActivityGraphView,
    node: &agl_protocol::ActivityNodeView,
    ascii: bool,
    maximum_depth: usize,
) -> String {
    let mut ancestors = Vec::new();
    let mut parent = node.parent_node_id.as_deref();
    while let Some(parent_id) = parent {
        let Some(parent_node) = graph
            .nodes
            .iter()
            .find(|candidate| candidate.node_id == parent_id)
        else {
            break;
        };
        ancestors.push(parent_node);
        parent = parent_node.parent_node_id.as_deref();
    }
    ancestors.reverse();

    let omitted = ancestors.len().saturating_sub(maximum_depth);
    let mut prefix = if omitted > 0 {
        if ascii { ".. " } else { "… " }.to_owned()
    } else {
        String::new()
    };
    for ancestor in ancestors.into_iter().skip(omitted) {
        let connector = if activity_node_is_last_sibling(graph, ancestor) {
            "  "
        } else if ascii {
            "| "
        } else {
            "│ "
        };
        prefix.push_str(connector);
    }
    prefix.push_str(if activity_node_is_last_sibling(graph, node) {
        if ascii { "`- " } else { "└─ " }
    } else if ascii {
        "+- "
    } else {
        "├─ "
    });
    prefix
}

fn activity_node_is_last_sibling(
    graph: &agl_protocol::ActivityGraphView,
    node: &agl_protocol::ActivityNodeView,
) -> bool {
    graph
        .nodes
        .iter()
        .rev()
        .find(|candidate| candidate.parent_node_id == node.parent_node_id)
        .is_some_and(|candidate| candidate.node_id == node.node_id)
}

fn activity_phase_label(phase: agl_protocol::ActivityPhase) -> &'static str {
    match phase {
        agl_protocol::ActivityPhase::Queued => "queued",
        agl_protocol::ActivityPhase::Policy => "policy",
        agl_protocol::ActivityPhase::Model => "model",
        agl_protocol::ActivityPhase::Tool => "tool",
        agl_protocol::ActivityPhase::ChildRun => "child run",
        agl_protocol::ActivityPhase::InferenceQueue => "inference queue",
        agl_protocol::ActivityPhase::InferenceAdmission => "inference admission",
        agl_protocol::ActivityPhase::ModelLoad => "model load",
        agl_protocol::ActivityPhase::Context => "context",
        agl_protocol::ActivityPhase::Prefill => "prefill",
        agl_protocol::ActivityPhase::Generation => "generation",
        agl_protocol::ActivityPhase::OutputParsing => "output parsing",
        agl_protocol::ActivityPhase::Terminal => "terminal",
        agl_protocol::ActivityPhase::Retention => "retention",
    }
}

fn palette_text(state: &UiState) -> Text<'static> {
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
                if state.no_color {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                }
            } else if matches!(command.availability, CommandAvailability::Disabled { .. }) {
                if state.no_color {
                    Style::default()
                } else {
                    Style::default().fg(Color::DarkGray)
                }
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
    Text::from(lines)
}

fn draw_composer(frame: &mut ratatui::Frame<'_>, area: Rect, model: &ComposerRenderModel) {
    let paragraph = Paragraph::new(model.text.clone()).block(
        Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(model.title.clone(), model.title_style)),
    );
    frame.render_widget(paragraph, area);
    let cursor_x = area.x.saturating_add(1).saturating_add(model.cursor.0);
    let cursor_y = area.y.saturating_add(1).saturating_add(model.cursor.1);
    frame.set_cursor_position((
        cursor_x.min(area.right().saturating_sub(1)),
        cursor_y.min(area.bottom().saturating_sub(1)),
    ));
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

fn display_path(path: &agl_protocol::SanitizedDisplayPath) -> String {
    if path.truncated {
        format!("{}…", path.text)
    } else {
        path.text.clone()
    }
}

fn workspace_label(workspace: &agl_protocol::SanitizedDisplayPath) -> String {
    let label = workspace
        .text
        .rsplit('/')
        .find(|component| !component.is_empty())
        .unwrap_or(&workspace.text);
    if workspace.truncated {
        format!("{label}…")
    } else {
        label.to_owned()
    }
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

    #[test]
    fn failed_run_notice_prefers_and_renders_the_detailed_message() {
        let message = "inference resource admission failed (accelerator_capacity_exceeded): \
            inference needs 23347593216 bytes with 0 already reserved, but only 23093305344 \
            bytes are available under 2659721216 bytes of device pressure";
        let finished = RunSubscriptionFinishedEvent {
            run_id: RunId::generate(),
            state: ProtocolRunState::Failed,
            last_sequence: 4,
            terminal_result: None,
            error_code: Some("accelerator_capacity_exceeded".to_owned()),
            error_message: Some(message.to_owned()),
        };

        assert_eq!(
            run_finished_notice(&finished),
            Some(format!("turn failed: {message}"))
        );

        let mut state = test_ui_state(SessionId::generate(), Vec::new());
        state.notice(run_finished_notice(&finished).unwrap());
        let backend = ratatui::backend::TestBackend::new(160, 8);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_transcript(frame, frame.area(), &state))
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("turn failed: inference resource admission failed"));
        assert!(rendered.contains("23093305344"));
    }

    #[test]
    fn failed_run_notice_falls_back_to_code_then_state() {
        let mut finished = RunSubscriptionFinishedEvent {
            run_id: RunId::generate(),
            state: ProtocolRunState::Failed,
            last_sequence: 0,
            terminal_result: None,
            error_code: Some("worker_lost".to_owned()),
            error_message: Some("  ".to_owned()),
        };

        assert_eq!(
            run_finished_notice(&finished),
            Some("turn failed (worker_lost)".to_owned())
        );
        finished.error_code = None;
        assert_eq!(
            run_finished_notice(&finished),
            Some("turn failed".to_owned())
        );
    }

    #[test]
    fn failed_run_notice_sanitizes_and_bounds_untrusted_protocol_text() {
        let hostile = format!(
            "osc=\u{1b}]52;c;secret\u{7}\ncolor=\u{1b}[31m bidi=\u{202e} {}",
            "x".repeat(MAX_RUN_FINISHED_NOTICE_BYTES * 2)
        );
        let mut finished = RunSubscriptionFinishedEvent {
            run_id: RunId::generate(),
            state: ProtocolRunState::Failed,
            last_sequence: 4,
            terminal_result: None,
            error_code: Some(hostile.clone()),
            error_message: Some(hostile),
        };

        for use_message in [true, false] {
            if !use_message {
                finished.error_message = Some("  ".to_owned());
            }
            let notice = run_finished_notice(&finished).unwrap();
            assert!(notice.len() <= MAX_RUN_FINISHED_NOTICE_BYTES);
            assert!(notice.contains("\\u{1B}]52;c;secret\\u{7}\\u{A}"));
            assert!(notice.contains("\\u{202E}"));
            assert!(!notice.chars().any(|character| {
                character.is_control() || is_unicode_format_control(character as u32)
            }));
            assert!(notice.contains('…'));
        }
    }

    #[test]
    fn succeeded_run_has_no_failure_notice() {
        let finished = RunSubscriptionFinishedEvent {
            run_id: RunId::generate(),
            state: ProtocolRunState::Succeeded,
            last_sequence: 1,
            terminal_result: None,
            error_code: Some("ignored".to_owned()),
            error_message: Some("ignored".to_owned()),
        };

        assert_eq!(run_finished_notice(&finished), None);
    }

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
    fn physical_bang_enters_shell_and_second_bang_escapes_to_prompt() {
        let mut composer = Composer::default();
        composer.insert_char('!');
        assert_eq!(composer.mode, ComposerMode::Shell);
        assert!(composer.buffer.is_empty());
        assert_eq!(composer.submit(), Some(ComposerSubmission::SwitchTerminal));

        composer.insert_char('!');
        composer.insert_text("ls");
        assert_eq!(
            composer.submit(),
            Some(ComposerSubmission::Shell("ls".to_owned()))
        );
        assert_eq!(composer.mode, ComposerMode::Shell);
        assert_eq!(composer.buffer, "ls");
        composer.reset();

        composer.insert_char('!');
        composer.insert_char('!');
        assert_eq!(composer.mode, ComposerMode::Prompt);
        assert_eq!(composer.buffer, "!");
        assert_eq!(
            composer.submit(),
            Some(ComposerSubmission::Prompt("!".to_owned()))
        );

        composer.insert_char('!');
        composer.backspace();
        assert_eq!(composer.mode, ComposerMode::Prompt);
        assert!(composer.buffer.is_empty());

        composer.insert_char('!');
        composer.insert_char('e');
        composer.insert_char('\n');
        assert_eq!(
            composer.submit(),
            Some(ComposerSubmission::Shell("e\n".to_owned()))
        );
        composer.reset();

        composer.insert_char('/');
        assert_eq!(composer.mode, ComposerMode::Command);
        composer.insert_char('/');
        assert_eq!(composer.mode, ComposerMode::Prompt);
        assert_eq!(composer.buffer, "/");
    }

    #[test]
    fn empty_shell_editor_key_reducer_escapes_or_attaches_exactly() {
        let mut state = test_ui_state(SessionId::generate(), Vec::new());

        assert!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE)
            )
            .is_none()
        );
        assert_eq!(state.composer.mode, ComposerMode::Shell);
        assert!(handle_key(&mut state, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)).is_none());
        assert_eq!(state.composer.mode, ComposerMode::Prompt);
        assert!(state.composer.buffer.is_empty());

        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE),
        );
        assert!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
            )
            .is_none()
        );
        assert_eq!(state.composer.mode, ComposerMode::Prompt);
        assert!(state.composer.buffer.is_empty());

        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE),
        );
        assert!(matches!(
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
            ),
            Some(UiControl::Submission(ComposerSubmission::SwitchTerminal))
        ));
        assert_eq!(state.composer.mode, ComposerMode::Prompt);
        assert!(state.composer.buffer.is_empty());
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

        composer.insert_char('!');
        composer.insert_paste("!printf shell-paste");
        assert_eq!(composer.mode, ComposerMode::Shell);
        assert_eq!(composer.buffer, "!printf shell-paste");
        assert_eq!(
            composer.submit(),
            Some(ComposerSubmission::Shell("!printf shell-paste".to_owned()))
        );
        assert_eq!(composer.buffer, "!printf shell-paste");
    }

    #[test]
    fn sanitized_display_paths_mark_truncation_without_becoming_authority_paths() {
        let complete = test_display_path("/workspace/repository");
        assert_eq!(display_path(&complete), "/workspace/repository");
        assert_eq!(workspace_label(&complete), "repository");

        let truncated = agl_protocol::SanitizedDisplayPath {
            text: "/workspace/partial".to_owned(),
            truncated: true,
        };
        assert_eq!(display_path(&truncated), "/workspace/partial…");
        assert_eq!(workspace_label(&truncated), "partial…");
    }

    #[test]
    fn shell_submission_keeps_exact_buffer_and_identity_until_explicit_acceptance() {
        let session_id = SessionId::generate();
        let terminal = test_terminal(
            TerminalOwnerView::Human {
                session_id: session_id.clone(),
            },
            ExecutionProfile::Workspace,
        );
        let mut state = test_ui_state(session_id.clone(), vec![terminal.clone()]);
        let command = "printf 'λ'\nprintf done".to_owned();
        state.composer.mode = ComposerMode::Shell;
        state.composer.buffer = command.clone();
        state.composer.cursor = command.len();

        let first = begin_shell_submission(&session_id, &mut state, command.clone(), &None)
            .unwrap()
            .unwrap();
        let submission_id = first.client_submission_id.clone();
        let ensure_id = first.terminal_ensure_submission_id.clone();
        assert_eq!(first.command, command);
        assert_eq!(state.composer.buffer, command);
        assert_eq!(state.composer.mode, ComposerMode::Shell);

        let busy = shell_submission_failure(&first, Some(terminal.clone()), None, "busy", false);
        apply_shell_submission_completion(&mut state, &session_id, None, busy);
        let pending = state.pending_shell_submission.as_ref().unwrap();
        assert_eq!(pending.command, command);
        assert_eq!(pending.client_submission_id, submission_id);
        assert!(!pending.in_flight);
        assert!(!pending.outcome_uncertain);
        assert_eq!(state.composer.buffer, command);

        let second = begin_shell_submission(&session_id, &mut state, command.clone(), &None)
            .unwrap()
            .unwrap();
        assert_eq!(second.client_submission_id, submission_id);
        assert_eq!(second.terminal_ensure_submission_id, ensure_id);
        let uncertain = shell_submission_failure(
            &second,
            Some(terminal.clone()),
            None,
            "connection closed",
            true,
        );
        apply_shell_submission_completion(&mut state, &session_id, None, uncertain);
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        );
        assert_eq!(state.composer.buffer, command);
        assert_eq!(
            state
                .pending_shell_submission
                .as_ref()
                .unwrap()
                .client_submission_id,
            submission_id
        );

        let retry = handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        let Some(UiControl::Submission(ComposerSubmission::Shell(retry_command))) = retry else {
            panic!("uncertain Shell command did not remain retryable");
        };
        let third = begin_shell_submission(&session_id, &mut state, retry_command, &None)
            .unwrap()
            .unwrap();
        assert_eq!(third.client_submission_id, submission_id);
        let accepted = ShellSubmissionCompletion {
            session_id: session_id.clone(),
            command: command.clone(),
            client_submission_id: submission_id,
            terminal: Some(terminal.clone()),
            attachment: None,
            outcome: Ok(agl_protocol::HumanTerminalCommandAcceptedEvent {
                terminal_id: terminal.terminal_id,
                command_sequence: 1,
                output_after_sequence: 0,
            }),
        };
        apply_shell_submission_completion(&mut state, &session_id, None, accepted);
        assert!(state.pending_shell_submission.is_none());
        assert_eq!(state.composer.mode, ComposerMode::Prompt);
        assert!(state.composer.buffer.is_empty());
    }

    #[test]
    fn unicode_editing_moves_and_deletes_whole_graphemes() {
        let mut composer = Composer::default();
        composer.insert_text("a👩‍💻б");
        composer.move_left(false);
        composer.backspace();
        assert_eq!(composer.buffer, "aб");
        assert_eq!(composer.cursor, 1);
    }

    #[test]
    fn composer_render_goldens_cover_wide_narrow_no_color_and_selection() {
        let render = |width: u16, no_color: bool| {
            let mut state = test_ui_state(SessionId::generate(), Vec::new());
            state.no_color = no_color;
            state.composer.insert_paste("alpha\t界\n👩‍💻 beta");
            state.composer.move_word_left(false);
            state.composer.move_word_right(true);
            let backend = ratatui::backend::TestBackend::new(width, 12);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| draw(frame, &state)).unwrap();
            let buffer = terminal.backend().buffer();
            let rows = (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>();
            let has_color = buffer
                .content
                .iter()
                .any(|cell| cell.fg != Color::Reset || cell.bg != Color::Reset);
            let has_selection = buffer
                .content
                .iter()
                .any(|cell| cell.modifier.contains(Modifier::REVERSED) || cell.bg == Color::Cyan);
            (rows, has_color, has_selection)
        };

        let (wide, wide_color, wide_selection) = render(80, false);
        assert!(wide_color);
        assert!(wide_selection);
        assert!(wide.iter().any(|row| row.contains("Prompt >")));
        assert!(wide.iter().any(|row| row.contains("alpha   界")));
        assert!(wide.iter().any(|row| row.contains("👩‍💻")));
        assert!(wide.iter().any(|row| row.contains("beta")));

        let (narrow, _, narrow_selection) = render(28, false);
        assert!(narrow_selection);
        assert_eq!(narrow.iter().map(String::len).min(), Some(28));
        assert!(narrow.iter().any(|row| row.contains("Prompt >")));

        let (no_color, has_color, no_color_selection) = render(80, true);
        assert!(!has_color);
        assert!(no_color_selection);
        assert!(no_color.iter().any(|row| row.contains("alpha   界")));
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

    fn test_display_path(text: &str) -> agl_protocol::SanitizedDisplayPath {
        agl_protocol::SanitizedDisplayPath {
            text: text.to_owned(),
            truncated: false,
        }
    }

    pub(super) fn test_ui_state(
        session_id: SessionId,
        terminals: Vec<TerminalSessionView>,
    ) -> UiState {
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
                    workspace_root: test_display_path("/workspace"),
                    cwd: test_display_path("/workspace"),
                    workspace_history_scope: format!("sha256:{}", "a".repeat(64)),
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
                human_commands: Vec::new(),
                activity: None,
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
            continuation_submission_ids: BTreeMap::new(),
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
            activity_expanded: false,
            pending_shell_submission: None,
            no_color: false,
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
            terminal_id: TerminalId::generate(),
            execution_id: ExecutionId::generate(),
            owner,
            profile,
            shell: agl_protocol::ShellProfileView {
                profile_id: "bash-managed".to_owned(),
                program: test_display_path("/bin/bash"),
                executable_digest: "sha256:executable".to_owned(),
                config_digest: "sha256:config".to_owned(),
            },
            workspace_root: test_display_path("/workspace"),
            cwd: test_display_path("/workspace"),
            initial_environment_digest: "sha256:environment".to_owned(),
            environment_names: vec!["PATH".to_owned()],
            command_sequence: 0,
            prompt_generation: Some(1),
            prompt_state: TerminalPromptState::Ready,
            process_state: agl_protocol::ExecutionState::Running,
            exit: None,
            writer: agl_protocol::TerminalWriterView::Owner,
            promoted: false,
        }
    }

    #[test]
    fn command_finished_does_not_synthesize_a_trusted_prompt() {
        let session_id = SessionId::generate();
        let mut terminal = test_terminal(
            TerminalOwnerView::Human {
                session_id: session_id.clone(),
            },
            ExecutionProfile::Workspace,
        );
        terminal.prompt_state = TerminalPromptState::ForegroundProcess;
        terminal.prompt_generation = None;

        let finished = agl_protocol::SessionPresentationEventPayload::TerminalCommandFinished {
            terminal_id: terminal.terminal_id.clone(),
            sequence: 1,
            exit_status: 0,
            cwd: test_display_path("/workspace"),
        };
        assert_eq!(
            terminal_prompt_from_event(&finished, &terminal.terminal_id),
            None
        );

        terminal.prompt_state = TerminalPromptState::Ready;
        terminal.prompt_generation = Some(2);
        let changed = agl_protocol::SessionPresentationEventPayload::TerminalChanged {
            terminal: terminal.clone(),
        };
        assert_eq!(
            terminal_prompt_from_event(&changed, &terminal.terminal_id),
            Some(TerminalPromptState::Ready)
        );
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
    fn presentation_delta_gaps_require_fresh_snapshot_without_losing_pending_shell() {
        let session_id = SessionId::generate();
        let mut state = test_ui_state(session_id, Vec::new());
        let pending = PendingShellSubmission {
            command: "printf safe".to_owned(),
            client_submission_id: "stable-shell-id".to_owned(),
            terminal_ensure_submission_id: "stable-terminal-id".to_owned(),
            in_flight: true,
            outcome_uncertain: false,
        };
        state.pending_shell_submission = Some(pending.clone());
        let run_id = RunId::generate();
        let turn_id = agl_ids::TurnId::generate();
        let message_id = MessageId::generate();
        let first = apply_presentation_event(
            &mut state,
            agl_protocol::SessionPresentationEventPayload::AssistantTextDelta {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                provisional_message_id: message_id.clone(),
                sequence: 1,
                text: "one".to_owned(),
            },
        );
        assert!(!first.resync_required);
        let gap = apply_presentation_event(
            &mut state,
            agl_protocol::SessionPresentationEventPayload::AssistantTextDelta {
                run_id,
                turn_id,
                provisional_message_id: message_id,
                sequence: 3,
                text: "three".to_owned(),
            },
        );
        assert!(gap.resync_required);

        let activity_gap = apply_presentation_event(
            &mut state,
            agl_protocol::SessionPresentationEventPayload::ActivityGraphDelta {
                batch: agl_protocol::ActivityGraphDeltaBatch {
                    graph_revision: 3,
                    upserts: Vec::new(),
                    removals: Vec::new(),
                    current_path: None,
                    truncated: false,
                },
            },
        );
        assert!(activity_gap.resync_required);

        let mut fresh = state.snapshot.clone();
        fresh.cursor.revision = fresh.cursor.revision.saturating_add(1);
        install_presentation_snapshot(&mut state, fresh);
        assert_eq!(state.pending_shell_submission, Some(pending));
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
        replacement.header.cwd = test_display_path("/workspace/replaced");
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
    fn incomplete_output_is_visible_without_color_and_targets_the_newest_available_item() {
        let session_id = SessionId::generate();
        let mut state = test_ui_state(session_id, Vec::new());
        let older_message_id = MessageId::generate();
        let newest_message_id = MessageId::generate();
        let continuation_run_id = RunId::generate();
        let incomplete_item =
            |message_id: MessageId,
             content: &str,
             continue_action: agl_protocol::ContinueActionView| {
                SessionPresentationItem::IncompleteAssistant {
                    item: agl_protocol::IncompleteAssistantItemView {
                        message_id,
                        content: agl_content::Content::text(content).unwrap(),
                        source_run_id: RunId::generate(),
                        source_turn_id: agl_ids::TurnId::generate(),
                        source_attempt_id: agl_ids::AttemptId::generate(),
                        reason: agl_protocol::IncompleteOutputReason::ContentByteLimit,
                        continuation_index: 0,
                        continue_action,
                    },
                }
            };
        state.snapshot.items = vec![
            incomplete_item(
                older_message_id,
                "older partial",
                agl_protocol::ContinueActionView::Claimed {
                    continuation_run_id,
                },
            ),
            incomplete_item(
                newest_message_id.clone(),
                "newest partial survives",
                agl_protocol::ContinueActionView::Available,
            ),
        ];

        assert_eq!(state.latest_available_incomplete(), Some(newest_message_id));
        let backend = ratatui::backend::TestBackend::new(120, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_transcript(frame, area, &state);
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("agentLIBRE · incomplete · output limit"));
        assert!(rendered.contains("newest partial survives"));
        assert!(rendered.contains("content byte limit · Ctrl+Y Continue"));
    }

    #[allow(clippy::too_many_arguments)]
    fn test_activity_node(
        run_id: &RunId,
        node_id: String,
        parent_node_id: Option<String>,
        order_index: u64,
        kind: agl_protocol::ActivityNodeKind,
        phase: agl_protocol::ActivityPhase,
        state: agl_protocol::ActivityNodeState,
        summary: &str,
        detail: agl_protocol::ActivityDetailView,
    ) -> agl_protocol::ActivityNodeView {
        let terminal = state.is_terminal();
        agl_protocol::ActivityNodeView {
            node_id,
            parent_node_id,
            order_index,
            run_id: run_id.clone(),
            turn_id: None,
            attempt_id: None,
            step_id: None,
            kind,
            phase,
            state,
            retry: 0,
            started_at_unix_ms: 1,
            updated_at_unix_ms: 5,
            finished_at_unix_ms: terminal.then_some(5),
            elapsed_ms: if terminal { 4 } else { 0 },
            summary: summary.to_owned(),
            detail,
        }
    }

    #[test]
    fn activity_delta_is_atomic_revisioned_idempotent_and_parent_ordered() {
        let run_id = RunId::generate();
        let root_id = format!("run:{run_id}");
        let step_id = "step:safe".to_owned();
        let root = test_activity_node(
            &run_id,
            root_id.clone(),
            None,
            1,
            agl_protocol::ActivityNodeKind::Run,
            agl_protocol::ActivityPhase::Model,
            agl_protocol::ActivityNodeState::Running,
            "run",
            agl_protocol::ActivityDetailView::None,
        );
        let step = test_activity_node(
            &run_id,
            step_id.clone(),
            Some(root_id.clone()),
            2,
            agl_protocol::ActivityNodeKind::Step,
            agl_protocol::ActivityPhase::Tool,
            agl_protocol::ActivityNodeState::Waiting,
            "core.workspace:fs.list",
            agl_protocol::ActivityDetailView::UnknownCapability {
                capability_id: "core.workspace:fs.list".to_owned(),
            },
        );
        let first = agl_protocol::ActivityGraphDeltaBatch {
            graph_revision: 1,
            upserts: vec![root, step],
            removals: Vec::new(),
            current_path: Some(vec![root_id.clone(), step_id.clone()]),
            truncated: false,
        };
        let graph = apply_activity_graph_delta(None, &first).unwrap();
        assert_eq!(graph.graph_revision, 1);
        assert_eq!(graph.roots, std::slice::from_ref(&root_id));
        assert_eq!(
            graph
                .nodes
                .iter()
                .map(|node| node.node_id.as_str())
                .collect::<Vec<_>>(),
            [root_id.as_str(), step_id.as_str()]
        );

        let duplicate = apply_activity_graph_delta(Some(&graph), &first).unwrap();
        assert_eq!(duplicate, graph);
        let mut conflicting = first.clone();
        conflicting.upserts[1].summary = "different".to_owned();
        assert!(apply_activity_graph_delta(Some(&graph), &conflicting).is_err());
        let mut gap = first;
        gap.graph_revision = 3;
        assert!(apply_activity_graph_delta(Some(&graph), &gap).is_err());
    }

    #[test]
    fn activity_render_has_compact_and_expanded_unicode_ascii_fallbacks() {
        let session_id = SessionId::generate();
        let mut state = test_ui_state(session_id, Vec::new());
        let run_id = RunId::generate();
        let root_id = format!("run:{run_id}");
        let failed_id = "step:failed".to_owned();
        let root = test_activity_node(
            &run_id,
            root_id.clone(),
            None,
            1,
            agl_protocol::ActivityNodeKind::Run,
            agl_protocol::ActivityPhase::Model,
            agl_protocol::ActivityNodeState::Running,
            "turn",
            agl_protocol::ActivityDetailView::None,
        );
        let failed = test_activity_node(
            &run_id,
            failed_id.clone(),
            Some(root_id.clone()),
            2,
            agl_protocol::ActivityNodeKind::Step,
            agl_protocol::ActivityPhase::Tool,
            agl_protocol::ActivityNodeState::Failed,
            "repository search",
            agl_protocol::ActivityDetailView::Capability(
                agl_protocol::CapabilityActivityDetail::RepositorySearch {
                    scope: test_display_path("crates/agl-app"),
                    matches: 7,
                    complete: false,
                },
            ),
        );
        state.snapshot.activity = Some(agl_protocol::ActivityGraphView {
            graph_revision: 9,
            roots: vec![root_id.clone()],
            nodes: vec![root, failed],
            current_path: vec![root_id],
            truncated: true,
        });
        state
            .notices
            .push("NO_COLOR must cover the complete Chat surface".to_owned());

        let render = |state: &UiState, width: u16, no_color: bool| {
            let backend = ratatui::backend::TestBackend::new(width, 12);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    draw_transcript_with_activity_mode(frame, area, state, no_color);
                })
                .unwrap();
            let buffer = terminal.backend().buffer();
            let rendered = buffer
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            let has_color = buffer.content.iter().any(|cell| cell.fg != Color::Reset);
            (rendered, has_color)
        };

        let (compact, _) = render(&state, 120, false);
        assert!(compact.contains("activity · model · turn"));
        assert!(compact.contains("repository search"));
        assert!(compact.contains("retained history truncated"));
        assert!(!compact.contains("├─"));

        state.activity_expanded = true;
        let (unicode, _) = render(&state, 120, false);
        assert!(unicode.contains("├─") || unicode.contains("└─"));
        assert!(unicode.contains("crates/agl-app · 7 matches · partial"));
        let (narrow, _) = render(&state, 42, false);
        let (no_color, has_no_color_style) = render(&state, 120, true);
        assert!(!has_no_color_style);
        for fallback in [&narrow, &no_color] {
            assert!(fallback.contains("+- ") || fallback.contains("`- "));
            for forbidden in [" → ", "├─", "└─", "\u{1b}_G", "\u{1b}Pq"] {
                assert!(!fallback.contains(forbidden));
            }
        }
        for sentinel in ["raw prompt", "super-secret-token", "/home/private"] {
            assert!(!unicode.contains(sentinel));
        }
    }

    #[test]
    fn activity_tree_connectors_follow_siblings_instead_of_global_node_order() {
        let first_run = RunId::generate();
        let second_run = RunId::generate();
        let first_root_id = format!("run:{first_run}");
        let first_child_id = "step:first-child".to_owned();
        let grandchild_id = "step:grandchild".to_owned();
        let second_child_id = "step:second-child".to_owned();
        let second_root_id = format!("run:{second_run}");
        let nodes = vec![
            test_activity_node(
                &first_run,
                first_root_id.clone(),
                None,
                1,
                agl_protocol::ActivityNodeKind::Run,
                agl_protocol::ActivityPhase::Model,
                agl_protocol::ActivityNodeState::Running,
                "first root",
                agl_protocol::ActivityDetailView::None,
            ),
            test_activity_node(
                &first_run,
                first_child_id.clone(),
                Some(first_root_id.clone()),
                2,
                agl_protocol::ActivityNodeKind::Step,
                agl_protocol::ActivityPhase::Tool,
                agl_protocol::ActivityNodeState::Running,
                "first child",
                agl_protocol::ActivityDetailView::None,
            ),
            test_activity_node(
                &first_run,
                grandchild_id.clone(),
                Some(first_child_id.clone()),
                3,
                agl_protocol::ActivityNodeKind::Step,
                agl_protocol::ActivityPhase::Tool,
                agl_protocol::ActivityNodeState::Running,
                "grandchild",
                agl_protocol::ActivityDetailView::None,
            ),
            test_activity_node(
                &first_run,
                second_child_id.clone(),
                Some(first_root_id.clone()),
                4,
                agl_protocol::ActivityNodeKind::Step,
                agl_protocol::ActivityPhase::Tool,
                agl_protocol::ActivityNodeState::Waiting,
                "second child",
                agl_protocol::ActivityDetailView::None,
            ),
            test_activity_node(
                &second_run,
                second_root_id.clone(),
                None,
                5,
                agl_protocol::ActivityNodeKind::Run,
                agl_protocol::ActivityPhase::Queued,
                agl_protocol::ActivityNodeState::Pending,
                "second root",
                agl_protocol::ActivityDetailView::None,
            ),
        ];
        let graph = agl_protocol::ActivityGraphView {
            graph_revision: 1,
            roots: vec![first_root_id, second_root_id],
            nodes,
            current_path: Vec::new(),
            truncated: false,
        };

        assert_eq!(
            activity_tree_prefix(&graph, &graph.nodes[0], false, 8),
            "├─ "
        );
        assert_eq!(
            activity_tree_prefix(&graph, &graph.nodes[1], false, 8),
            "│ ├─ "
        );
        assert_eq!(
            activity_tree_prefix(&graph, &graph.nodes[2], false, 8),
            "│ │ └─ "
        );
        assert_eq!(
            activity_tree_prefix(&graph, &graph.nodes[3], false, 8),
            "│ └─ "
        );
        assert_eq!(
            activity_tree_prefix(&graph, &graph.nodes[4], false, 8),
            "└─ "
        );
        assert_eq!(
            activity_tree_prefix(&graph, &graph.nodes[2], true, 8),
            "| | `- "
        );
    }

    #[test]
    fn daemon_connection_errors_distinguish_missing_and_incompatible_servers() {
        let socket = Path::new("/tmp/agentlibre-test.sock");
        let missing =
            interactive_connect_error(socket, ClientError::DaemonUnavailable("refused".to_owned()));
        assert!(missing.to_string().contains("daemon is unavailable"));

        let incompatible = interactive_connect_error(
            socket,
            ClientError::SchemaMismatch {
                expected: "agentlibre.event.v8alpha",
            },
        );
        assert!(incompatible.to_string().contains("incompatible protocol"));
        assert!(format!("{incompatible:#}").contains("v8alpha"));
    }

    #[test]
    fn interactive_unavailable_never_falls_back_to_a_process_local_worker() {
        let root = std::env::temp_dir().join(format!(
            "agl-cli-interactive-no-fallback-{}",
            agl_ids::RequestId::generate()
        ));
        let runtime = AgentLibreRuntimeConfig {
            paths: agl_runtime::AgentLibrePaths::from_agl_home(root.join("home")),
            logging: agl_runtime::AgentLibreLoggingConfig::default(),
            history: agl_runtime::AgentLibreHistoryConfig::default(),
            workspace: agl_runtime::AgentLibreWorkspaceConfig::default(),
            inference: agl_runtime::AgentLibreInferenceConfig::default(),
            execution: agl_runtime::AgentLibreExecutionConfig::default(),
        };
        let options = InteractiveOptions {
            resume: None,
            input_history: false,
            socket_path: Some(root.join("missing-daemon.sock")),
            workspace_root: None,
            function_ref: None,
            model_id: None,
            operation_mode: None,
            skills: Vec::new(),
        };

        let error = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run_interactive_async(options, &runtime))
            .expect_err("interactive mode must require its daemon");

        assert!(format!("{error:#}").contains("daemon is unavailable"));
        assert!(!runtime.paths.inference_worker_temp_root().exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn private_history_is_bounded_and_uses_only_opaque_workspace_scope() {
        let root = std::env::temp_dir().join(format!(
            "agl-cli-history-test-{}",
            agl_ids::RequestId::generate()
        ));
        let state_dir = root.join("state");
        let workspace_history_scope = format!("sha256:{}", "b".repeat(64));
        let (mut history, warnings) =
            InputHistory::load(&state_dir, &workspace_history_scope, true);
        assert!(warnings.is_empty());
        history.record_prompt("hello").unwrap();
        history.record_prompt("hello").unwrap();
        let history_root = history.root.clone().unwrap();
        assert!(
            !history_root
                .to_string_lossy()
                .contains(&workspace_history_scope)
        );
        assert_eq!(history.prompt, vec!["hello"]);
        assert!(history.entries(ComposerMode::Shell).is_empty());
        assert!(!history_root.join("shell.jsonl").exists());
        let (reloaded, warnings) = InputHistory::load(&state_dir, &workspace_history_scope, true);
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
