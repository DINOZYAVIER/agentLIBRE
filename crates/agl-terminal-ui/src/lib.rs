use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal as _};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use self::composer::{Composer, ComposerMode, MAX_COMPOSER_BYTES};
use self::reducer::{UiEffect, UiEvent, update};
use self::render_model::{ComposerRenderModel, PickerRenderModel, view};
use self::terminal_filter::TerminalOutputFilter;
use self::terminal_input::{RawTerminalInputGate, TerminalInputAction};
#[cfg(target_os = "linux")]
use self::terminal_view::RawTtyInput;
use agl_client::{
    AgentLibreClient, ClientError, PresentationSubscription, PresentationSubscriptionEvent,
    RunSubscriptionEvent,
};
use agl_ids::{MessageId, RequestId, RunId, SessionId};
use agl_protocol::{
    ActiveRunView, ApplicationAction, ApplicationActionRequest, ApplicationToolResult,
    ClientEffectKind, CommandAvailability, CommandCatalogRequest, CommandDescriptor,
    CommandSuggestion, CommandSuggestionsRequest, ExecutionId, ExecutionProfile, ExecutionView,
    HostStartupPolicy, HumanHostTerminalEnsureRequest, HumanTerminalEnsureRequest,
    HumanTerminalEnsuredEvent, ProtocolRunState, ProtocolToolMode, RunBudgetRequest,
    RunSubmitRequest, RunSubscribeRequest, RunSubscriptionFinishedEvent, SessionLaunchOptions,
    SessionPresentationItem, SessionPresentationRequest, SessionPresentationSnapshot,
    SessionPresentationSubscribeRequest, SessionSelector, StructuredEnvironmentOverlay,
    TerminalOwnerView, TerminalPromptState, TerminalSessionView, TerminalSize,
};
use agl_terminal::TerminalId;
use agl_terminal_client::{TerminalClient, UnixTerminalTransport};
use agl_terminal_protocol::{
    LOCAL_OPERATOR_AUTHORITY_FINGERPRINT, TerminalEventKind, TerminalGenerationFileRole,
    TerminalGenerationIdentity, VerifiedTerminalGeneration,
};
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
const MAX_LOCAL_HUMAN_COMMAND_CARDS: usize = 32;
mod attachment;
mod runtime;
use attachment::*;
use runtime::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolAccessMode {
    ReadOnly,
    Write,
    Execute,
    Approve,
    Admin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InteractiveOptions {
    pub resume: Option<String>,
    pub input_history: bool,
    pub socket_path: Option<PathBuf>,
    pub workspace_root: Option<PathBuf>,
    pub function_ref: Option<String>,
    pub model_id: Option<String>,
    pub operation_mode: Option<ToolAccessMode>,
    pub skills: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiRuntimeConfig {
    pub agent_state_dir: PathBuf,
    pub ui_state_dir: PathBuf,
    pub terminal_runtime_dir: PathBuf,
    pub shell_program: PathBuf,
}

impl UiRuntimeConfig {
    pub fn from_env() -> Result<Self> {
        if let Some(home) = std::env::var_os("AGL_HOME") {
            return Self::from_home(home);
        }
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is required when AGL_HOME and XDG roots are unset")?;
        let state_home = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state"));
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let runtime_home = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::geteuid() })));
        Self::from_roots(
            state_home.join("agentLIBRE"),
            state_home.join("agl-terminal"),
            runtime_home.join("agl-terminal"),
            config_home.join("agl-terminal"),
        )
    }

    pub fn from_home(home: impl Into<PathBuf>) -> Result<Self> {
        let home = home.into();
        Self::from_roots(
            home.join("state"),
            home.join("agl-terminal"),
            home.join("runtime/agl-terminal"),
            home.join("terminal-config"),
        )
    }

    fn from_roots(
        agent_state_dir: PathBuf,
        ui_state_dir: PathBuf,
        terminal_runtime_dir: PathBuf,
        config_dir: PathBuf,
    ) -> Result<Self> {
        let config_path = config_dir.join("terminal.toml");
        let shell_program = match fs::read_to_string(&config_path) {
            Ok(contents) => {
                let value: toml::Value = toml::from_str(&contents)
                    .with_context(|| format!("failed to parse {}", config_path.display()))?;
                value
                    .get("execution")
                    .and_then(|value| value.get("shell"))
                    .and_then(|value| value.get("program"))
                    .and_then(toml::Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("bash"))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => PathBuf::from("bash"),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read {}", config_path.display()));
            }
        };
        Ok(Self {
            agent_state_dir,
            ui_state_dir,
            terminal_runtime_dir,
            shell_program,
        })
    }
}

mod history;
use history::*;

mod picker;
use picker::*;

mod state;
use state::*;

pub fn run_interactive(options: InteractiveOptions, runtime: &UiRuntimeConfig) -> Result<()> {
    #[cfg(not(target_os = "linux"))]
    bail!("interactive Chat/Terminal UI is currently supported only on Linux");
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("interactive UI requires terminal stdin and stdout");
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build terminal UI runtime")?
        .block_on(run_interactive_async(options, runtime))
}

mod session;
use session::*;

mod commands;
use commands::*;

mod shell_submission;
use shell_submission::*;

mod terminal_passthrough;
use terminal_passthrough::*;

mod presentation;
use presentation::*;

mod render;
use render::*;

mod terminal_mode;
use terminal_mode::*;

#[cfg(test)]
mod tests;
