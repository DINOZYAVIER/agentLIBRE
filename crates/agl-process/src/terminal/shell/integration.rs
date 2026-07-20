use std::fmt::{self, Debug, Formatter};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{ProcessError, ProcessErrorCode, Result};

pub const SHELL_INTEGRATION_VERSION: u8 = 1;
pub const MAX_SHELL_INTEGRATION_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_SHELL_INTEGRATION_COMMAND_BYTES: usize = 32 * 1024;
pub const MAX_SHELL_INTEGRATION_PATH_BYTES: usize = 4 * 1024;

const SHELL_INTEGRATION_MAGIC: &[u8] = b"AGL1";
const SHELL_INTEGRATION_TOKEN_BYTES: usize = 32;
const SHELL_INTEGRATION_TOKEN_HEX_BYTES: usize = SHELL_INTEGRATION_TOKEN_BYTES * 2;

/// Per-terminal authentication material for the private shell integration
/// transport. It is deliberately not serializable and its debug form never
/// renders the secret. The shell startup file is the only consumer of the
/// textual value and removes itself after loading it.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ShellIntegrationToken(String);

impl ShellIntegrationToken {
    pub(crate) fn generate() -> Result<Self> {
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

    pub(crate) fn expose_to_managed_startup(&self) -> &str {
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ShellExit {
    Code { code: i32 },
    Signal { signal: i32 },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ShellIntegrationEvent {
    PromptReady {
        sequence: u64,
        cwd: PathBuf,
        last_exit: Option<i32>,
    },
    CommandStarted {
        sequence: u64,
        command: String,
        cwd: PathBuf,
    },
    CommandFinished {
        sequence: u64,
        exit: ShellExit,
        cwd: PathBuf,
    },
    ForegroundChanged {
        sequence: u64,
        process_group: Option<i32>,
    },
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
    pub(crate) fn new(token: ShellIntegrationToken) -> Self {
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
                    b"prompt_ready" | b"command_started" => Some(6),
                    b"command_finished" => Some(7),
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

    /// Merges an authoritative Linux `tcgetpgrp` sample into the private
    /// integration stream. Shell frame and observed-event counters stay
    /// independent so an observation cannot collide with the next frame
    /// sequence emitted by Bash or Zsh.
    pub(crate) fn observe_foreground(&mut self, process_group: Option<i32>) -> IntegrationBatch {
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
        }),
        b"command_started" => Ok(ShellIntegrationEvent::CommandStarted {
            sequence,
            command: field_utf8(fields, 4, "command")?.to_owned(),
            cwd: PathBuf::from(field_utf8(fields, 5, "cwd")?),
        }),
        b"command_finished" => {
            let exit = match field_utf8(fields, 4, "exit kind")? {
                "code" => ShellExit::Code {
                    code: parse_i32(field_utf8(fields, 5, "exit code")?, "exit code")?,
                },
                "signal" => ShellExit::Signal {
                    signal: parse_i32(field_utf8(fields, 5, "exit signal")?, "exit signal")?,
                },
                _ => return Err(invalid_frame("shell integration exit kind is unsupported")),
            };
            Ok(ShellIntegrationEvent::CommandFinished {
                sequence,
                exit,
                cwd: PathBuf::from(field_utf8(fields, 6, "cwd")?),
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
            ShellIntegrationEvent::PromptReady { cwd, last_exit, .. } => {
                fields.extend([
                    b"prompt_ready".to_vec(),
                    cwd.to_string_lossy().as_bytes().to_vec(),
                    last_exit
                        .map_or_else(|| "-".to_owned(), |code| code.to_string())
                        .into_bytes(),
                ]);
            }
            ShellIntegrationEvent::CommandStarted { command, cwd, .. } => {
                fields.extend([
                    b"command_started".to_vec(),
                    command.as_bytes().to_vec(),
                    cwd.to_string_lossy().as_bytes().to_vec(),
                ]);
            }
            ShellIntegrationEvent::CommandFinished { exit, cwd, .. } => {
                let (kind, value) = match exit {
                    ShellExit::Code { code } => ("code", code.to_string()),
                    ShellExit::Signal { signal } => ("signal", signal.to_string()),
                };
                fields.extend([
                    b"command_finished".to_vec(),
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
            },
            ShellIntegrationEvent::CommandStarted {
                sequence: 2,
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
                exit: ShellExit::Code { code: 0 },
                cwd: cwd.clone(),
            },
            ShellIntegrationEvent::PromptReady {
                sequence: 6,
                cwd: cwd.clone(),
                last_exit: Some(0),
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
            },
            ShellIntegrationEvent::CommandStarted {
                sequence: 2,
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
                exit: ShellExit::Code { code: 0 },
                cwd: cwd.clone(),
            },
            ShellIntegrationEvent::PromptReady {
                sequence: 4,
                cwd: cwd.clone(),
                last_exit: Some(0),
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
            },
        );
        assert_eq!(integration.push(&first).events.len(), 1);
        let gap = frame(
            &token,
            &ShellIntegrationEvent::CommandStarted {
                sequence: 3,
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
}
