use std::fmt::{self, Debug, Formatter};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use agl_exec::{ProcessError, ProcessErrorCode, Result};

pub const SHELL_INTEGRATION_VERSION: u8 = 2;
pub use agl_pty::MAX_SHELL_INTEGRATION_FRAME_BYTES;
pub const MAX_SHELL_INTEGRATION_COMMAND_BYTES: usize = 64 * 1024;
pub const MAX_SHELL_INTEGRATION_PATH_BYTES: usize = 4 * 1024;

const SHELL_INTEGRATION_MAGIC: &[u8] = b"AGL2";
const SHELL_INTEGRATION_TOKEN_BYTES: usize = 32;
const SHELL_INTEGRATION_TOKEN_HEX_BYTES: usize = SHELL_INTEGRATION_TOKEN_BYTES * 2;

/// Per-terminal authentication material for the private shell integration
/// transport. It is deliberately not serializable and its debug form never
/// renders the secret. The shell startup file is the only consumer of the
/// textual value and removes itself after loading it.
#[derive(Clone, Eq, PartialEq)]
pub struct ShellIntegrationToken(String);

impl ShellIntegrationToken {
    pub fn generate() -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let mut random = [0u8; SHELL_INTEGRATION_TOKEN_BYTES];
            let mut offset = 0usize;
            while offset < random.len() {
                let read = unsafe {
                    libc::getrandom(
                        random[offset..].as_mut_ptr().cast(),
                        random.len() - offset,
                        0,
                    )
                };
                if read > 0 {
                    offset += read as usize;
                    continue;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(ProcessError::new(
                    ProcessErrorCode::Internal,
                    format!("failed to create private shell integration token: {error}"),
                ));
            }
            let mut encoded = String::with_capacity(SHELL_INTEGRATION_TOKEN_HEX_BYTES);
            const HEX: &[u8; 16] = b"0123456789abcdef";
            for byte in random {
                encoded.push(HEX[usize::from(byte >> 4)] as char);
                encoded.push(HEX[usize::from(byte & 0x0f)] as char);
            }
            Ok(Self(encoded))
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(ProcessError::new(
                ProcessErrorCode::PlatformUnsupported,
                "private shell integration is supported only on Linux",
            ))
        }
    }

    pub fn expose_to_managed_startup(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    fn fixed(byte: u8) -> Self {
        Self(format!("{byte:02x}").repeat(SHELL_INTEGRATION_TOKEN_BYTES))
    }
}

impl Debug for ShellIntegrationToken {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("ShellIntegrationToken([REDACTED])")
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TypedCommandTransactionId(String);

impl TypedCommandTransactionId {
    pub fn generate() -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let mut random = [0_u8; 16];
            let mut offset = 0_usize;
            while offset < random.len() {
                let read = unsafe {
                    libc::getrandom(
                        random[offset..].as_mut_ptr().cast(),
                        random.len() - offset,
                        0,
                    )
                };
                if read > 0 {
                    offset += read as usize;
                    continue;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(ProcessError::new(
                    ProcessErrorCode::Internal,
                    format!("failed to create typed shell transaction identity: {error}"),
                ));
            }
            let mut encoded = String::with_capacity(random.len() * 2);
            use std::fmt::Write as _;
            for byte in random {
                write!(&mut encoded, "{byte:02x}")
                    .expect("writing a transaction identity to String cannot fail");
            }
            Ok(Self(encoded))
        }
        #[cfg(not(target_os = "linux"))]
        {
            Err(ProcessError::new(
                ProcessErrorCode::PlatformUnsupported,
                "typed shell transactions are supported only on Linux",
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn parse(value: &str) -> Result<Self> {
        if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(invalid_frame(
                "typed shell transaction identity must be 32 hexadecimal bytes",
            ));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    #[cfg(test)]
    fn fixed(byte: u8) -> Self {
        Self(format!("{byte:02x}").repeat(16))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandBoundary {
    Started,
    Finished,
}

impl CommandBoundary {
    fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Finished => "finished",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypedCommandAbortReason {
    InputWriteFailed,
    Cancelled,
    ValidationFailed,
}

impl TypedCommandAbortReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::InputWriteFailed => "input_write_failed",
            Self::Cancelled => "cancelled",
            Self::ValidationFailed => "validation_failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ShellIntegrationControl {
    ArmTypedCommand {
        transaction_id: TypedCommandTransactionId,
        expected_command_sequence: u64,
    },
    CommandBoundaryAck {
        transaction_id: TypedCommandTransactionId,
        boundary: CommandBoundary,
    },
    PromptReadyAck {
        event_sequence: u64,
        /// `None` withholds a trusted generation. Managed shells perform one
        /// bounded re-probe before entering their line editor, allowing a
        /// consumed foreground-program input latch to clear without trusting
        /// bytes that raced the prompt boundary.
        prompt_generation: Option<u64>,
    },
    DisarmTypedCommand {
        transaction_id: TypedCommandTransactionId,
        reason: TypedCommandAbortReason,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ShellExit {
    Code { code: i32 },
    Signal { signal: i32 },
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ShellIntegrationEvent {
    PromptReady {
        sequence: u64,
        cwd: PathBuf,
        last_exit: Option<i32>,
        input_pending: bool,
    },
    CommandStarted {
        sequence: u64,
        transaction_id: Option<TypedCommandTransactionId>,
        command: String,
        cwd: PathBuf,
    },
    CommandFinished {
        sequence: u64,
        transaction_id: Option<TypedCommandTransactionId>,
        exit: ShellExit,
        cwd: PathBuf,
    },
    ForegroundChanged {
        sequence: u64,
        process_group: Option<i32>,
    },
}

impl Debug for ShellIntegrationEvent {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::PromptReady {
                sequence,
                cwd,
                last_exit,
                input_pending,
            } => formatter
                .debug_struct("PromptReady")
                .field("sequence", sequence)
                .field("cwd", cwd)
                .field("last_exit", last_exit)
                .field("input_pending", input_pending)
                .finish(),
            Self::CommandStarted {
                sequence,
                transaction_id,
                command,
                cwd,
            } => formatter
                .debug_struct("CommandStarted")
                .field("sequence", sequence)
                .field("transaction_id", transaction_id)
                .field("command_bytes", &command.len())
                .field("cwd", cwd)
                .finish(),
            Self::CommandFinished {
                sequence,
                transaction_id,
                exit,
                cwd,
            } => formatter
                .debug_struct("CommandFinished")
                .field("sequence", sequence)
                .field("transaction_id", transaction_id)
                .field("exit", exit)
                .field("cwd", cwd)
                .finish(),
            Self::ForegroundChanged {
                sequence,
                process_group,
            } => formatter
                .debug_struct("ForegroundChanged")
                .field("sequence", sequence)
                .field("process_group", process_group)
                .finish(),
        }
    }
}

impl ShellIntegrationEvent {
    pub fn sequence(&self) -> u64 {
        match self {
            Self::PromptReady { sequence, .. }
            | Self::CommandStarted { sequence, .. }
            | Self::CommandFinished { sequence, .. }
            | Self::ForegroundChanged { sequence, .. } => *sequence,
        }
    }

    fn set_sequence(&mut self, sequence: u64) {
        match self {
            Self::PromptReady {
                sequence: current, ..
            }
            | Self::CommandStarted {
                sequence: current, ..
            }
            | Self::CommandFinished {
                sequence: current, ..
            }
            | Self::ForegroundChanged {
                sequence: current, ..
            } => *current = sequence,
        }
    }

    fn validate(&self) -> Result<()> {
        if self.sequence() == 0 {
            return Err(invalid_frame("shell integration sequence must be nonzero"));
        }
        match self {
            Self::PromptReady { cwd, last_exit, .. } => {
                if last_exit.is_some_and(|code| !(0..=255).contains(&code)) {
                    return Err(invalid_frame(
                        "shell integration prompt exit code must be between 0 and 255",
                    ));
                }
                validate_cwd(cwd)
            }
            Self::CommandStarted { command, cwd, .. } => {
                if command.is_empty()
                    || command.len() > MAX_SHELL_INTEGRATION_COMMAND_BYTES
                    || command.contains('\0')
                {
                    return Err(invalid_frame(
                        "shell integration command must be nonempty, bounded, and contain no NUL",
                    ));
                }
                validate_cwd(cwd)
            }
            Self::CommandFinished { exit, cwd, .. } => {
                match exit {
                    ShellExit::Code { code } if !(0..=255).contains(code) => {
                        return Err(invalid_frame(
                            "shell integration exit code must be between 0 and 255",
                        ));
                    }
                    ShellExit::Signal { signal } if *signal <= 0 => {
                        return Err(invalid_frame(
                            "shell integration exit signal must be positive",
                        ));
                    }
                    ShellExit::Code { .. } | ShellExit::Signal { .. } => {}
                }
                validate_cwd(cwd)
            }
            Self::ForegroundChanged { process_group, .. } => {
                if process_group.is_some_and(|group| group <= 0) {
                    return Err(invalid_frame(
                        "shell integration process group must be positive",
                    ));
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellIntegrationHealth {
    AwaitingFirstPrompt,
    Trusted,
    Degraded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalPromptState {
    Unknown,
    Ready {
        sequence: u64,
        last_exit: Option<i32>,
    },
    CommandRunning {
        sequence: u64,
    },
    ForegroundProgram {
        sequence: u64,
        process_group: i32,
    },
    Degraded,
}

impl TerminalPromptState {
    pub fn is_trusted_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellIntegrationNotice {
    pub code: &'static str,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct ShellIntegrationState {
    health: ShellIntegrationHealth,
    prompt: TerminalPromptState,
    last_sequence: Option<u64>,
    cwd: Option<PathBuf>,
    active_command_sequence: Option<u64>,
    foreground_process_group: Option<i32>,
}

impl Default for ShellIntegrationState {
    fn default() -> Self {
        Self {
            health: ShellIntegrationHealth::AwaitingFirstPrompt,
            prompt: TerminalPromptState::Unknown,
            last_sequence: None,
            cwd: None,
            active_command_sequence: None,
            foreground_process_group: None,
        }
    }
}

impl ShellIntegrationState {
    pub fn health(&self) -> ShellIntegrationHealth {
        self.health
    }

    pub fn prompt(&self) -> &TerminalPromptState {
        &self.prompt
    }

    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    pub fn last_sequence(&self) -> Option<u64> {
        self.last_sequence
    }

    pub fn foreground_process_group(&self) -> Option<i32> {
        self.foreground_process_group
    }

    fn apply(&mut self, event: &ShellIntegrationEvent) -> Result<()> {
        if self.health == ShellIntegrationHealth::Degraded {
            return Err(invalid_frame("shell integration is already degraded"));
        }
        let sequence = event.sequence();
        match self.last_sequence {
            None if sequence != 1 => {
                return Err(invalid_frame(
                    "shell integration sequence must begin at one",
                ));
            }
            Some(last)
                if sequence
                    != last.checked_add(1).ok_or_else(|| {
                        invalid_frame("shell integration sequence overflowed")
                    })? =>
            {
                return Err(invalid_frame(
                    "shell integration sequence must be contiguous and monotonic",
                ));
            }
            None | Some(_) => {}
        }

        match event {
            ShellIntegrationEvent::PromptReady { cwd, last_exit, .. } => {
                if self.active_command_sequence.is_some() {
                    return Err(invalid_frame(
                        "prompt_ready arrived before command_finished",
                    ));
                }
                self.health = ShellIntegrationHealth::Trusted;
                self.cwd = Some(cwd.clone());
                self.foreground_process_group = None;
                self.prompt = TerminalPromptState::Ready {
                    sequence,
                    last_exit: *last_exit,
                };
            }
            ShellIntegrationEvent::CommandStarted { cwd, .. } => {
                if !self.prompt.is_trusted_ready() || self.active_command_sequence.is_some() {
                    return Err(invalid_frame(
                        "command_started requires one trusted fresh prompt",
                    ));
                }
                self.cwd = Some(cwd.clone());
                self.active_command_sequence = Some(sequence);
                self.prompt = TerminalPromptState::CommandRunning { sequence };
            }
            ShellIntegrationEvent::CommandFinished { cwd, .. } => {
                if self.active_command_sequence.take().is_none() {
                    return Err(invalid_frame(
                        "command_finished requires one active command",
                    ));
                }
                self.cwd = Some(cwd.clone());
                self.foreground_process_group = None;
                self.prompt = TerminalPromptState::Unknown;
            }
            ShellIntegrationEvent::ForegroundChanged { process_group, .. } => {
                if self.active_command_sequence.is_none() {
                    return Err(invalid_frame(
                        "foreground change requires one active command",
                    ));
                }
                self.foreground_process_group = *process_group;
                self.prompt = match process_group {
                    Some(process_group) => TerminalPromptState::ForegroundProgram {
                        sequence,
                        process_group: *process_group,
                    },
                    None => TerminalPromptState::CommandRunning { sequence },
                };
            }
        }
        self.last_sequence = Some(sequence);
        Ok(())
    }

    fn degrade(&mut self) {
        self.health = ShellIntegrationHealth::Degraded;
        self.prompt = TerminalPromptState::Degraded;
        self.active_command_sequence = None;
        self.foreground_process_group = None;
    }
}

/// Incremental NUL-field parser for bytes read exclusively from the private
/// side channel. No PTY output call path feeds this parser.
#[derive(Clone)]
pub struct BoundedShellIntegration {
    token: ShellIntegrationToken,
    fields: Vec<Vec<u8>>,
    field: Vec<u8>,
    frame_bytes: usize,
    expected_fields: Option<usize>,
    last_shell_sequence: Option<u64>,
    observed_event_count: u64,
    state: ShellIntegrationState,
}

impl BoundedShellIntegration {
    pub fn new(token: ShellIntegrationToken) -> Self {
        Self {
            token,
            fields: Vec::with_capacity(7),
            field: Vec::new(),
            frame_bytes: 0,
            expected_fields: None,
            last_shell_sequence: None,
            observed_event_count: 0,
            state: ShellIntegrationState::default(),
        }
    }

    pub fn state(&self) -> &ShellIntegrationState {
        &self.state
    }

    pub fn last_shell_sequence(&self) -> Option<u64> {
        self.last_shell_sequence
    }

    /// Decodes one SOCK_SEQPACKET event. The relay preserves packet
    /// boundaries, so a partial frame or multiple concatenated frames is a
    /// protocol violation rather than an incremental read condition.
    pub fn push_packet(&mut self, bytes: &[u8]) -> IntegrationBatch {
        let batch = self.push(bytes);
        if batch.notice.is_some() {
            return batch;
        }
        if !self.fields.is_empty()
            || !self.field.is_empty()
            || self.frame_bytes != 0
            || self.expected_fields.is_some()
        {
            return self.degrade("shell integration packet ended inside a frame");
        }
        if batch.events.len() != 1 {
            return self.degrade("shell integration packet must contain exactly one event");
        }
        batch
    }

    pub fn push(&mut self, bytes: &[u8]) -> IntegrationBatch {
        if self.state.health == ShellIntegrationHealth::Degraded {
            return IntegrationBatch::default();
        }
        let mut events = Vec::new();
        for byte in bytes {
            if self.frame_bytes >= MAX_SHELL_INTEGRATION_FRAME_BYTES {
                return self.degrade("shell integration frame exceeded its byte bound");
            }
            self.frame_bytes += 1;
            if *byte != 0 {
                self.field.push(*byte);
                continue;
            }
            self.fields.push(std::mem::take(&mut self.field));
            if self.fields.len() == 4 {
                self.expected_fields = match self.fields[3].as_slice() {
                    b"prompt_ready" | b"command_started" => Some(7),
                    b"command_finished" => Some(8),
                    b"foreground_changed" => Some(5),
                    _ => return self.degrade("shell integration event kind is unsupported"),
                };
            }
            let Some(expected) = self.expected_fields else {
                continue;
            };
            if self.fields.len() < expected {
                continue;
            }
            if self.fields.len() != expected {
                return self.degrade("shell integration frame has the wrong field count");
            }
            let mut event = match decode_frame(&self.fields, &self.token) {
                Ok(event) => event,
                Err(error) => return self.degrade(error.message()),
            };
            if let Err(error) = event.validate() {
                return self.degrade(error.message());
            }
            let shell_sequence = event.sequence();
            if let Err(message) = validate_next_sequence(
                self.last_shell_sequence,
                shell_sequence,
                "shell integration frame",
            ) {
                return self.degrade(message);
            }
            let logical_sequence = match shell_sequence.checked_add(self.observed_event_count) {
                Some(sequence) => sequence,
                None => return self.degrade("shell integration sequence overflowed"),
            };
            event.set_sequence(logical_sequence);
            if let Err(error) = self.state.apply(&event) {
                return self.degrade(error.message());
            }
            self.last_shell_sequence = Some(shell_sequence);
            events.push(event);
            self.fields.clear();
            self.field.clear();
            self.frame_bytes = 0;
            self.expected_fields = None;
        }
        IntegrationBatch {
            events,
            notice: None,
        }
    }

    pub fn encode_control(&self, control: &ShellIntegrationControl) -> Result<Vec<u8>> {
        if self.state.health == ShellIntegrationHealth::Degraded {
            return Err(invalid_frame(
                "cannot send control through a degraded shell integration",
            ));
        }
        let mut frame = Vec::new();
        push_field(&mut frame, SHELL_INTEGRATION_MAGIC);
        push_field(
            &mut frame,
            self.token.expose_to_managed_startup().as_bytes(),
        );
        match control {
            ShellIntegrationControl::ArmTypedCommand {
                transaction_id,
                expected_command_sequence,
            } => {
                if *expected_command_sequence == 0 {
                    return Err(invalid_frame(
                        "typed shell command sequence must be nonzero",
                    ));
                }
                push_field(&mut frame, b"arm_typed_command");
                push_field(&mut frame, transaction_id.as_str().as_bytes());
                push_field(&mut frame, expected_command_sequence.to_string().as_bytes());
            }
            ShellIntegrationControl::CommandBoundaryAck {
                transaction_id,
                boundary,
            } => {
                push_field(&mut frame, b"command_boundary_ack");
                push_field(&mut frame, transaction_id.as_str().as_bytes());
                push_field(&mut frame, boundary.as_str().as_bytes());
            }
            ShellIntegrationControl::PromptReadyAck {
                event_sequence,
                prompt_generation,
            } => {
                if *event_sequence == 0 {
                    return Err(invalid_frame(
                        "prompt-ready acknowledgement sequence must be nonzero",
                    ));
                }
                push_field(&mut frame, b"prompt_ready_ack");
                push_field(&mut frame, event_sequence.to_string().as_bytes());
                let generation = prompt_generation
                    .map(|generation| generation.to_string())
                    .unwrap_or_else(|| "-".to_owned());
                push_field(&mut frame, generation.as_bytes());
            }
            ShellIntegrationControl::DisarmTypedCommand {
                transaction_id,
                reason,
            } => {
                push_field(&mut frame, b"disarm_typed_command");
                push_field(&mut frame, transaction_id.as_str().as_bytes());
                push_field(&mut frame, reason.as_str().as_bytes());
            }
        }
        if frame.len() > MAX_SHELL_INTEGRATION_FRAME_BYTES {
            return Err(invalid_frame(
                "shell integration control exceeded its frame bound",
            ));
        }
        Ok(frame)
    }

    /// Merges an authoritative Linux `tcgetpgrp` sample into the private
    /// integration stream. Shell frame and observed-event counters stay
    /// independent so an observation cannot collide with the next frame
    /// sequence emitted by Bash or Zsh.
    pub fn observe_foreground(&mut self, process_group: Option<i32>) -> IntegrationBatch {
        if self.state.health == ShellIntegrationHealth::Degraded
            || self.state.active_command_sequence.is_none()
            || self.state.foreground_process_group == process_group
        {
            return IntegrationBatch::default();
        }
        if process_group.is_some_and(|group| group <= 0) {
            return self.degrade("observed foreground process group must be positive");
        }
        let Some(sequence) = self
            .state
            .last_sequence
            .and_then(|sequence| sequence.checked_add(1))
        else {
            return self.degrade("shell integration sequence overflowed");
        };
        let Some(observed_event_count) = self.observed_event_count.checked_add(1) else {
            return self.degrade("shell integration observation count overflowed");
        };
        let event = ShellIntegrationEvent::ForegroundChanged {
            sequence,
            process_group,
        };
        if let Err(error) = self.state.apply(&event) {
            return self.degrade(error.message());
        }
        self.observed_event_count = observed_event_count;
        IntegrationBatch {
            events: vec![event],
            notice: None,
        }
    }

    pub fn channel_closed(&mut self) -> IntegrationBatch {
        if self.state.health == ShellIntegrationHealth::Degraded {
            IntegrationBatch::default()
        } else {
            self.degrade("shell integration channel closed")
        }
    }

    pub fn mark_unavailable(&mut self) -> IntegrationBatch {
        if self.state.health == ShellIntegrationHealth::Degraded {
            IntegrationBatch::default()
        } else {
            self.degrade("private shell integration channel is unavailable")
        }
    }

    fn degrade(&mut self, message: impl Into<String>) -> IntegrationBatch {
        self.fields.clear();
        self.field.clear();
        self.frame_bytes = 0;
        self.expected_fields = None;
        self.state.degrade();
        IntegrationBatch {
            events: Vec::new(),
            notice: Some(ShellIntegrationNotice {
                code: "shell_integration_degraded",
                message: message.into(),
            }),
        }
    }
}

fn validate_next_sequence(
    previous: Option<u64>,
    sequence: u64,
    label: &str,
) -> std::result::Result<(), String> {
    match previous {
        None if sequence != 1 => Err(format!("{label} sequence must begin at one")),
        Some(previous) => match previous.checked_add(1) {
            Some(expected) if sequence == expected => Ok(()),
            Some(_) => Err(format!("{label} sequence must be contiguous and monotonic")),
            None => Err(format!("{label} sequence overflowed")),
        },
        None => Ok(()),
    }
}

#[derive(Default)]
pub struct IntegrationBatch {
    pub events: Vec<ShellIntegrationEvent>,
    pub notice: Option<ShellIntegrationNotice>,
}

fn decode_frame(
    fields: &[Vec<u8>],
    token: &ShellIntegrationToken,
) -> Result<ShellIntegrationEvent> {
    if fields.first().map(Vec::as_slice) != Some(SHELL_INTEGRATION_MAGIC) {
        return Err(invalid_frame(format!(
            "shell integration frame version is unsupported; expected version {SHELL_INTEGRATION_VERSION}",
        )));
    }
    if fields.get(1).is_none_or(|field| {
        !constant_time_equal(field, token.expose_to_managed_startup().as_bytes())
    }) {
        return Err(invalid_frame(
            "shell integration frame authentication failed",
        ));
    }
    let sequence = parse_u64(field_utf8(fields, 2, "sequence")?, "sequence")?;
    let kind = fields
        .get(3)
        .map(Vec::as_slice)
        .ok_or_else(|| invalid_frame("shell integration frame omitted its event kind"))?;
    match kind {
        b"prompt_ready" => Ok(ShellIntegrationEvent::PromptReady {
            sequence,
            cwd: PathBuf::from(field_utf8(fields, 4, "cwd")?),
            last_exit: parse_optional_i32(field_utf8(fields, 5, "last exit")?)?,
            input_pending: parse_bool(field_utf8(fields, 6, "input pending")?)?,
        }),
        b"command_started" => Ok(ShellIntegrationEvent::CommandStarted {
            sequence,
            transaction_id: parse_optional_transaction(field_utf8(
                fields,
                4,
                "transaction identity",
            )?)?,
            command: field_utf8(fields, 5, "command")?.to_owned(),
            cwd: PathBuf::from(field_utf8(fields, 6, "cwd")?),
        }),
        b"command_finished" => {
            let transaction_id =
                parse_optional_transaction(field_utf8(fields, 4, "transaction identity")?)?;
            let exit = match field_utf8(fields, 5, "exit kind")? {
                "code" => ShellExit::Code {
                    code: parse_i32(field_utf8(fields, 6, "exit code")?, "exit code")?,
                },
                "signal" => ShellExit::Signal {
                    signal: parse_i32(field_utf8(fields, 6, "exit signal")?, "exit signal")?,
                },
                _ => return Err(invalid_frame("shell integration exit kind is unsupported")),
            };
            Ok(ShellIntegrationEvent::CommandFinished {
                sequence,
                transaction_id,
                exit,
                cwd: PathBuf::from(field_utf8(fields, 7, "cwd")?),
            })
        }
        b"foreground_changed" => Ok(ShellIntegrationEvent::ForegroundChanged {
            sequence,
            process_group: parse_optional_i32(field_utf8(fields, 4, "process group")?)?,
        }),
        _ => Err(invalid_frame("shell integration event kind is unsupported")),
    }
}

fn field_utf8<'a>(fields: &'a [Vec<u8>], index: usize, name: &str) -> Result<&'a str> {
    std::str::from_utf8(
        fields
            .get(index)
            .ok_or_else(|| invalid_frame(format!("shell integration frame omitted {name}")))?,
    )
    .map_err(|_| invalid_frame(format!("shell integration {name} is not valid UTF-8")))
}

fn parse_u64(value: &str, name: &str) -> Result<u64> {
    value
        .parse::<u64>()
        .map_err(|_| invalid_frame(format!("shell integration {name} is not a valid integer")))
}

fn parse_i32(value: &str, name: &str) -> Result<i32> {
    value
        .parse::<i32>()
        .map_err(|_| invalid_frame(format!("shell integration {name} is not a valid integer")))
}

fn parse_optional_i32(value: &str) -> Result<Option<i32>> {
    if value == "-" {
        Ok(None)
    } else {
        parse_i32(value, "optional integer").map(Some)
    }
}

fn parse_bool(value: &str) -> Result<bool> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(invalid_frame(
            "shell integration boolean field must be zero or one",
        )),
    }
}

fn parse_optional_transaction(value: &str) -> Result<Option<TypedCommandTransactionId>> {
    if value == "-" {
        Ok(None)
    } else {
        TypedCommandTransactionId::parse(value).map(Some)
    }
}

fn push_field(frame: &mut Vec<u8>, value: &[u8]) {
    frame.extend_from_slice(value);
    frame.push(0);
}

fn constant_time_equal(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |different, (left, right)| different | (left ^ right))
        == 0
}

fn validate_cwd(cwd: &Path) -> Result<()> {
    let bytes = cwd.as_os_str().to_string_lossy();
    if !cwd.is_absolute()
        || bytes.is_empty()
        || bytes.len() > MAX_SHELL_INTEGRATION_PATH_BYTES
        || bytes.contains('\0')
    {
        return Err(invalid_frame(
            "shell integration cwd must be an absolute bounded path",
        ));
    }
    Ok(())
}

fn invalid_frame(message: impl Into<String>) -> ProcessError {
    ProcessError::new(ProcessErrorCode::StateConflict, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(token: &ShellIntegrationToken, event: &ShellIntegrationEvent) -> Vec<u8> {
        let mut fields = vec![
            SHELL_INTEGRATION_MAGIC.to_vec(),
            token.expose_to_managed_startup().as_bytes().to_vec(),
            event.sequence().to_string().into_bytes(),
        ];
        match event {
            ShellIntegrationEvent::PromptReady {
                cwd,
                last_exit,
                input_pending,
                ..
            } => {
                fields.extend([
                    b"prompt_ready".to_vec(),
                    cwd.to_string_lossy().as_bytes().to_vec(),
                    last_exit
                        .map_or_else(|| "-".to_owned(), |code| code.to_string())
                        .into_bytes(),
                    if *input_pending { b"1" } else { b"0" }.to_vec(),
                ]);
            }
            ShellIntegrationEvent::CommandStarted {
                transaction_id,
                command,
                cwd,
                ..
            } => {
                fields.extend([
                    b"command_started".to_vec(),
                    transaction_id
                        .as_ref()
                        .map_or("-", TypedCommandTransactionId::as_str)
                        .as_bytes()
                        .to_vec(),
                    command.as_bytes().to_vec(),
                    cwd.to_string_lossy().as_bytes().to_vec(),
                ]);
            }
            ShellIntegrationEvent::CommandFinished {
                transaction_id,
                exit,
                cwd,
                ..
            } => {
                let (kind, value) = match exit {
                    ShellExit::Code { code } => ("code", code.to_string()),
                    ShellExit::Signal { signal } => ("signal", signal.to_string()),
                };
                fields.extend([
                    b"command_finished".to_vec(),
                    transaction_id
                        .as_ref()
                        .map_or("-", TypedCommandTransactionId::as_str)
                        .as_bytes()
                        .to_vec(),
                    kind.as_bytes().to_vec(),
                    value.into_bytes(),
                    cwd.to_string_lossy().as_bytes().to_vec(),
                ]);
            }
            ShellIntegrationEvent::ForegroundChanged { process_group, .. } => {
                fields.extend([
                    b"foreground_changed".to_vec(),
                    process_group
                        .map_or_else(|| "-".to_owned(), |group| group.to_string())
                        .into_bytes(),
                ]);
            }
        }
        fields
            .into_iter()
            .flat_map(|mut field| {
                field.push(0);
                field
            })
            .collect()
    }

    #[test]
    fn fragmented_frames_preserve_one_multiline_command_boundary() {
        let token = ShellIntegrationToken::fixed(0x11);
        let cwd = PathBuf::from("/workspace");
        let events = [
            ShellIntegrationEvent::PromptReady {
                sequence: 1,
                cwd: cwd.clone(),
                last_exit: None,
                input_pending: false,
            },
            ShellIntegrationEvent::CommandStarted {
                sequence: 2,
                transaction_id: None,
                command: "printf 'one\\ntwo' | sed -n 2p".to_owned(),
                cwd: cwd.clone(),
            },
            ShellIntegrationEvent::ForegroundChanged {
                sequence: 3,
                process_group: Some(42),
            },
            ShellIntegrationEvent::ForegroundChanged {
                sequence: 4,
                process_group: None,
            },
            ShellIntegrationEvent::CommandFinished {
                sequence: 5,
                transaction_id: None,
                exit: ShellExit::Code { code: 0 },
                cwd: cwd.clone(),
            },
            ShellIntegrationEvent::PromptReady {
                sequence: 6,
                cwd: cwd.clone(),
                last_exit: Some(0),
                input_pending: false,
            },
        ];
        let bytes = events
            .iter()
            .flat_map(|event| frame(&token, event))
            .collect::<Vec<_>>();
        let split = bytes.len() / 3;
        let mut integration = BoundedShellIntegration::new(token);
        assert!(integration.push(&bytes[..split]).events.len() < events.len());
        let mut decoded = integration.push(&bytes[split..]).events;
        assert_eq!(decoded.pop(), Some(events[5].clone()));
        assert_eq!(
            integration.state().health(),
            ShellIntegrationHealth::Trusted
        );
        assert_eq!(integration.state().cwd(), Some(cwd.as_path()));
        assert!(integration.state().prompt().is_trusted_ready());
    }

    #[test]
    fn kernel_foreground_observations_merge_without_colliding_with_shell_sequences() {
        let token = ShellIntegrationToken::fixed(0x12);
        let cwd = PathBuf::from("/workspace");
        let mut integration = BoundedShellIntegration::new(token.clone());
        let initial = [
            ShellIntegrationEvent::PromptReady {
                sequence: 1,
                cwd: cwd.clone(),
                last_exit: None,
                input_pending: false,
            },
            ShellIntegrationEvent::CommandStarted {
                sequence: 2,
                transaction_id: None,
                command: "sleep 10".to_owned(),
                cwd: cwd.clone(),
            },
        ]
        .iter()
        .flat_map(|event| frame(&token, event))
        .collect::<Vec<_>>();
        assert_eq!(integration.push(&initial).events.len(), 2);

        let foreground = integration.observe_foreground(Some(4242));
        assert_eq!(
            foreground.events,
            vec![ShellIntegrationEvent::ForegroundChanged {
                sequence: 3,
                process_group: Some(4242),
            }]
        );
        assert!(integration.observe_foreground(Some(4242)).events.is_empty());

        let background = integration.observe_foreground(None);
        assert_eq!(
            background.events,
            vec![ShellIntegrationEvent::ForegroundChanged {
                sequence: 4,
                process_group: None,
            }]
        );
        let completed = [
            ShellIntegrationEvent::CommandFinished {
                sequence: 3,
                transaction_id: None,
                exit: ShellExit::Code { code: 0 },
                cwd: cwd.clone(),
            },
            ShellIntegrationEvent::PromptReady {
                sequence: 4,
                cwd: cwd.clone(),
                last_exit: Some(0),
                input_pending: false,
            },
        ]
        .iter()
        .flat_map(|event| frame(&token, event))
        .collect::<Vec<_>>();
        assert_eq!(
            integration
                .push(&completed)
                .events
                .iter()
                .map(ShellIntegrationEvent::sequence)
                .collect::<Vec<_>>(),
            vec![5, 6]
        );
        assert_eq!(
            integration.state().health(),
            ShellIntegrationHealth::Trusted
        );
        assert!(integration.state().prompt().is_trusted_ready());
    }

    #[test]
    fn sequence_gap_or_invalid_transition_permanently_degrades_state() {
        let token = ShellIntegrationToken::fixed(0x22);
        let mut integration = BoundedShellIntegration::new(token.clone());
        let first = frame(
            &token,
            &ShellIntegrationEvent::PromptReady {
                sequence: 1,
                cwd: PathBuf::from("/workspace"),
                last_exit: None,
                input_pending: false,
            },
        );
        assert_eq!(integration.push(&first).events.len(), 1);
        let gap = frame(
            &token,
            &ShellIntegrationEvent::CommandStarted {
                sequence: 3,
                transaction_id: None,
                command: "true".to_owned(),
                cwd: PathBuf::from("/workspace"),
            },
        );
        let batch = integration.push(&gap);
        assert!(batch.events.is_empty());
        assert_eq!(batch.notice.unwrap().code, "shell_integration_degraded");
        assert_eq!(
            integration.state().health(),
            ShellIntegrationHealth::Degraded
        );
        assert_eq!(integration.state().prompt(), &TerminalPromptState::Degraded);
        assert!(integration.push(&first).events.is_empty());
    }

    #[test]
    fn token_mismatch_is_rejected_as_a_spoof_attempt() {
        let expected = ShellIntegrationToken::fixed(0x33);
        let attacker = ShellIntegrationToken::fixed(0x44);
        let mut integration = BoundedShellIntegration::new(expected);
        let spoof = frame(
            &attacker,
            &ShellIntegrationEvent::PromptReady {
                sequence: 1,
                cwd: PathBuf::from("/spoofed"),
                last_exit: None,
                input_pending: false,
            },
        );

        let batch = integration.push(&spoof);

        assert!(batch.events.is_empty());
        assert!(
            batch
                .notice
                .unwrap()
                .message
                .contains("authentication failed")
        );
        assert_eq!(
            integration.state().health(),
            ShellIntegrationHealth::Degraded
        );
    }

    #[test]
    fn pty_text_is_not_an_integration_event_without_private_parser_input() {
        let integration = BoundedShellIntegration::new(ShellIntegrationToken::fixed(0x55));
        let fake_pty = b"AGL1\\0token\\01\\0prompt_ready\\0/spoofed\\0-\\0";
        assert_eq!(
            integration.state().health(),
            ShellIntegrationHealth::AwaitingFirstPrompt
        );
        assert!(!fake_pty.is_empty());
        assert_eq!(integration.state().last_sequence(), None);
    }

    #[test]
    fn token_debug_output_is_always_redacted() {
        let token = ShellIntegrationToken::fixed(0xaa);
        let debug = format!("{token:?}");
        assert!(!debug.contains(token.expose_to_managed_startup()));
        assert!(debug.contains("REDACTED"));
    }

    #[test]
    fn typed_controls_bind_one_authenticated_transaction_and_boundary() {
        let token = ShellIntegrationToken::fixed(0x66);
        let transaction_id = TypedCommandTransactionId::fixed(0x77);
        let integration = BoundedShellIntegration::new(token.clone());
        let arm = integration
            .encode_control(&ShellIntegrationControl::ArmTypedCommand {
                transaction_id: transaction_id.clone(),
                expected_command_sequence: 42,
            })
            .unwrap();
        let fields = arm
            .split(|byte| *byte == 0)
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(fields[0], SHELL_INTEGRATION_MAGIC);
        assert_eq!(fields[1], token.expose_to_managed_startup().as_bytes());
        assert_eq!(fields[2], b"arm_typed_command");
        assert_eq!(fields[3], transaction_id.as_str().as_bytes());
        assert_eq!(fields[4], b"42");

        let started = integration
            .encode_control(&ShellIntegrationControl::CommandBoundaryAck {
                transaction_id,
                boundary: CommandBoundary::Started,
            })
            .unwrap();
        assert!(
            started
                .windows(b"command_boundary_ack".len())
                .any(|window| window == b"command_boundary_ack")
        );
        assert!(started.ends_with(b"started\0"));
    }

    #[test]
    fn command_started_debug_never_exposes_canonical_command_text() {
        const SENTINEL: &str = "AGL_PRIVATE_SHELL_INTEGRATION_COMMAND_148";
        let event = ShellIntegrationEvent::CommandStarted {
            sequence: 1,
            transaction_id: None,
            command: SENTINEL.to_owned(),
            cwd: PathBuf::from("/workspace"),
        };
        let rendered = format!("{event:?}");
        assert!(!rendered.contains(SENTINEL));
        assert!(rendered.contains(&format!("command_bytes: {}", SENTINEL.len())));
    }
}
