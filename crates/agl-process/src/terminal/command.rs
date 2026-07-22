use std::collections::VecDeque;
use std::fmt::{self, Debug, Formatter};
use std::path::PathBuf;
use std::time::Instant;

use agl_ids::{ExecutionId, TerminalSessionId};

use crate::terminal::shell::{ShellExit, TerminalPromptState};
use crate::{ProcessBytes, ProcessError, ProcessErrorCode, Result};

pub const DEFAULT_AGENT_TERMINAL_QUEUE_CAPACITY: usize = 32;
pub const MAX_TYPED_TERMINAL_COMMAND_BYTES: usize = 64 * 1024;
pub const MAX_AGENT_TERMINAL_COMMAND_BYTES: usize = MAX_TYPED_TERMINAL_COMMAND_BYTES;
pub const MAX_HUMAN_TERMINAL_COMMAND_BYTES: usize = MAX_TYPED_TERMINAL_COMMAND_BYTES;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HumanTerminalCommandAdmission {
    pub terminal_id: TerminalSessionId,
    pub execution_id: ExecutionId,
    pub command_sequence: u64,
    pub output_after_sequence: u64,
    pub submission: ProcessBytes,
}

pub fn human_terminal_command_submission(command: &str) -> Result<ProcessBytes> {
    if command.is_empty() || command.len() > MAX_TYPED_TERMINAL_COMMAND_BYTES {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "Human terminal command must be nonempty and at most 64 KiB",
        ));
    }
    if command.chars().any(forbidden_human_command_character) {
        return Err(ProcessError::new(
            ProcessErrorCode::InvalidRequest,
            "Human terminal command contains a forbidden control character",
        ));
    }
    // Bracketed paste makes a multi-line editor buffer one explicit user
    // submission instead of letting the first newline race ahead of the
    // remainder. Managed Bash/Zsh profiles own and enable this input mode.
    let mut bytes = Vec::with_capacity(command.len().saturating_add(13));
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(command.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~\n");
    Ok(ProcessBytes::from_bytes(&bytes))
}

fn forbidden_human_command_character(character: char) -> bool {
    let code = character as u32;
    (code <= 0x1f && character != '\n' && character != '\t') || (0x7f..=0x9f).contains(&code)
}

#[derive(Clone, Eq, PartialEq)]
pub struct SanitizedTerminalOutput {
    text: String,
    filtered_control_sequences: u32,
    truncated: bool,
}

impl Debug for SanitizedTerminalOutput {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SanitizedTerminalOutput")
            .field("bytes", &self.text.len())
            .field("filtered_effects", &self.filtered_control_sequences)
            .field("truncated", &self.truncated)
            .finish()
    }
}

impl SanitizedTerminalOutput {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn filtered_effects(&self) -> u32 {
        self.filtered_control_sequences
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }
}

const FILTERED_CONTROL_MARKER: &str = "[filtered-control]";
const TRUNCATED_OUTPUT_MARKER: &str = "[truncated]";
const MAX_CONTROL_SEQUENCE_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandCardControlState {
    Ground,
    Escape {
        bytes: usize,
    },
    Csi {
        bytes: usize,
    },
    String {
        bytes: usize,
        escape_pending: bool,
        allow_bel: bool,
    },
}

/// Incremental, fail-closed sanitizer for private Human command cards.
///
/// This is intentionally separate from raw terminal passthrough filtering.
/// It preserves printable text, newline and tab, but no terminal control
/// payload can cross this boundary. The caller may continue feeding bytes
/// after the presentation limit is reached so the authoritative spool remains
/// fully drained.
pub struct CommandCardSanitizer {
    maximum_bytes: usize,
    output: String,
    filtered_effects: u32,
    truncated: bool,
    state: CommandCardControlState,
    utf8_pending: Vec<u8>,
    pending_carriage_return: bool,
}

impl CommandCardSanitizer {
    pub fn new(maximum_bytes: usize) -> Self {
        Self {
            maximum_bytes,
            output: String::new(),
            filtered_effects: 0,
            truncated: false,
            state: CommandCardControlState::Ground,
            utf8_pending: Vec::with_capacity(4),
            pending_carriage_return: false,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.push_byte(byte);
        }
    }

    pub fn finish(mut self) -> SanitizedTerminalOutput {
        if self.pending_carriage_return {
            self.pending_carriage_return = false;
            self.append_fragment("\n");
        }
        self.flush_incomplete_utf8();
        SanitizedTerminalOutput {
            text: self.output,
            filtered_control_sequences: self.filtered_effects,
            truncated: self.truncated,
        }
    }

    fn push_byte(&mut self, byte: u8) {
        if !matches!(self.state, CommandCardControlState::Ground) {
            self.consume_control_byte(byte);
            return;
        }

        if self.pending_carriage_return {
            self.pending_carriage_return = false;
            self.append_fragment("\n");
            if byte == b'\n' {
                return;
            }
        }

        if !self.utf8_pending.is_empty() {
            if byte.is_ascii() {
                self.flush_incomplete_utf8();
            } else {
                self.utf8_pending.push(byte);
                self.drain_utf8_pending();
                return;
            }
        }

        match byte {
            b'\r' => {
                self.flush_incomplete_utf8();
                self.pending_carriage_return = true;
            }
            b'\n' => {
                self.flush_incomplete_utf8();
                self.append_fragment("\n");
            }
            b'\t' => {
                self.flush_incomplete_utf8();
                self.append_fragment("\t");
            }
            0x1b => {
                self.flush_incomplete_utf8();
                self.begin_control(CommandCardControlState::Escape { bytes: 1 });
            }
            0x9b => {
                self.flush_incomplete_utf8();
                self.begin_control(CommandCardControlState::Csi { bytes: 1 });
            }
            0x90 => {
                self.flush_incomplete_utf8();
                self.begin_string_control(false);
            }
            0x9d => {
                self.flush_incomplete_utf8();
                self.begin_string_control(true);
            }
            0x9e | 0x9f => {
                self.flush_incomplete_utf8();
                self.begin_string_control(false);
            }
            0x00..=0x1f | 0x7f..=0x9f => {
                self.flush_incomplete_utf8();
                self.escape_byte(byte);
            }
            0x20..=0x7e => {
                self.flush_incomplete_utf8();
                let mut ascii = [0_u8; 1];
                ascii[0] = byte;
                self.append_fragment(
                    std::str::from_utf8(&ascii).expect("printable ASCII is valid UTF-8"),
                );
            }
            _ => {
                self.utf8_pending.push(byte);
                self.drain_utf8_pending();
            }
        }
    }

    fn begin_string_control(&mut self, allow_bel: bool) {
        self.begin_control(CommandCardControlState::String {
            bytes: 1,
            escape_pending: false,
            allow_bel,
        });
    }

    fn begin_control(&mut self, state: CommandCardControlState) {
        self.filtered_effects = self.filtered_effects.saturating_add(1);
        self.append_fragment(FILTERED_CONTROL_MARKER);
        self.state = state;
    }

    fn consume_control_byte(&mut self, byte: u8) {
        self.state = match self.state {
            CommandCardControlState::Ground => return self.push_byte(byte),
            CommandCardControlState::Escape { bytes } => {
                let bytes = bytes.saturating_add(1).min(MAX_CONTROL_SEQUENCE_BYTES + 1);
                match byte {
                    b'[' => CommandCardControlState::Csi { bytes },
                    b']' => CommandCardControlState::String {
                        bytes,
                        escape_pending: false,
                        allow_bel: true,
                    },
                    b'P' | b'_' | b'^' => CommandCardControlState::String {
                        bytes,
                        escape_pending: false,
                        allow_bel: false,
                    },
                    0x20..=0x2f => CommandCardControlState::Escape { bytes },
                    _ => CommandCardControlState::Ground,
                }
            }
            CommandCardControlState::Csi { bytes } => {
                let bytes = bytes.saturating_add(1).min(MAX_CONTROL_SEQUENCE_BYTES + 1);
                if (0x40..=0x7e).contains(&byte) {
                    CommandCardControlState::Ground
                } else {
                    CommandCardControlState::Csi { bytes }
                }
            }
            CommandCardControlState::String {
                bytes,
                escape_pending,
                allow_bel,
            } => {
                let bytes = bytes.saturating_add(1).min(MAX_CONTROL_SEQUENCE_BYTES + 1);
                if (allow_bel && byte == 0x07) || byte == 0x9c || (escape_pending && byte == b'\\')
                {
                    CommandCardControlState::Ground
                } else {
                    CommandCardControlState::String {
                        bytes,
                        escape_pending: byte == 0x1b,
                        allow_bel,
                    }
                }
            }
        };
    }

    fn drain_utf8_pending(&mut self) {
        loop {
            match std::str::from_utf8(&self.utf8_pending) {
                Ok(valid) => {
                    let character = valid
                        .chars()
                        .next()
                        .expect("nonempty UTF-8 pending buffer has one scalar");
                    self.utf8_pending.clear();
                    self.push_unicode_scalar(character);
                    return;
                }
                Err(error) if error.error_len().is_none() && self.utf8_pending.len() < 4 => {
                    return;
                }
                Err(error) => {
                    let invalid = error.error_len().unwrap_or(1).max(1);
                    let invalid = invalid.min(self.utf8_pending.len());
                    let bytes = self.utf8_pending.drain(..invalid).collect::<Vec<_>>();
                    for byte in bytes {
                        self.escape_byte(byte);
                    }
                    if self.utf8_pending.is_empty() {
                        return;
                    }
                }
            }
        }
    }

    fn flush_incomplete_utf8(&mut self) {
        let bytes = std::mem::take(&mut self.utf8_pending);
        for byte in bytes {
            self.escape_byte(byte);
        }
    }

    fn push_unicode_scalar(&mut self, character: char) {
        let code = character as u32;
        match code {
            0x9b => self.begin_control(CommandCardControlState::Csi { bytes: 2 }),
            0x90 => self.begin_string_control(false),
            0x9d => self.begin_string_control(true),
            0x9e | 0x9f => self.begin_string_control(false),
            _ if character.is_control() || is_unicode_format_control(code) => {
                self.filtered_effects = self.filtered_effects.saturating_add(1);
                self.append_fragment(&format!("\\u{{{code:X}}}"));
            }
            _ => self.append_fragment(&character.to_string()),
        }
    }

    fn escape_byte(&mut self, byte: u8) {
        self.filtered_effects = self.filtered_effects.saturating_add(1);
        self.append_fragment(&format!("\\x{byte:02X}"));
    }

    fn append_fragment(&mut self, fragment: &str) {
        if self.truncated {
            return;
        }
        if self.output.len().saturating_add(fragment.len()) <= self.maximum_bytes {
            self.output.push_str(fragment);
            return;
        }
        self.mark_truncated();
    }

    fn mark_truncated(&mut self) {
        if self.truncated {
            return;
        }
        self.truncated = true;
        if self.maximum_bytes >= TRUNCATED_OUTPUT_MARKER.len() {
            let mut keep = self.maximum_bytes - TRUNCATED_OUTPUT_MARKER.len();
            while !self.output.is_char_boundary(keep) {
                keep -= 1;
            }
            self.output.truncate(keep);
            self.output.push_str(TRUNCATED_OUTPUT_MARKER);
        } else {
            let mut keep = self.maximum_bytes.min(self.output.len());
            while !self.output.is_char_boundary(keep) {
                keep -= 1;
            }
            self.output.truncate(keep);
        }
    }
}

pub fn sanitize_terminal_card_output(
    bytes: &[u8],
    maximum_bytes: usize,
) -> SanitizedTerminalOutput {
    let mut sanitizer = CommandCardSanitizer::new(maximum_bytes);
    sanitizer.push(bytes);
    sanitizer.finish()
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
            | 0xe0020..=0xe007f
    )
}

pub struct QueuedTerminalCommand {
    pub command_sequence: u64,
    command: String,
    pub deadline: Option<Instant>,
}

impl QueuedTerminalCommand {
    pub fn command(&self) -> &str {
        &self.command
    }

    pub fn submission(&self) -> ProcessBytes {
        human_terminal_command_submission(&self.command)
            .expect("queued agent commands passed the shared typed-command validator")
    }
}

impl Debug for QueuedTerminalCommand {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueuedTerminalCommand")
            .field("command_sequence", &self.command_sequence)
            .field("command_bytes", &self.command.len())
            .field("deadline", &self.deadline)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalCommandOutputRange {
    pub after_sequence: u64,
    pub through_sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TerminalCommandResult {
    pub terminal_id: TerminalSessionId,
    pub execution_id: ExecutionId,
    pub command_sequence: u64,
    pub cwd: PathBuf,
    pub exit: ShellExit,
    pub output: TerminalCommandOutputRange,
}

struct ActiveCommand {
    command: QueuedTerminalCommand,
    integration_start_sequence: Option<u64>,
    output_after_sequence: u64,
    submission_reserved: bool,
    submitted: bool,
}

pub struct AgentTerminalCommandQueue {
    capacity: usize,
    next_sequence: u64,
    queued: VecDeque<QueuedTerminalCommand>,
    active: Option<ActiveCommand>,
}

impl AgentTerminalCommandQueue {
    pub fn new(capacity: usize) -> Result<Self> {
        if capacity == 0 {
            return Err(ProcessError::new(
                ProcessErrorCode::InvalidRequest,
                "agent terminal command queue capacity must be nonzero",
            ));
        }
        Ok(Self {
            capacity,
            next_sequence: 1,
            queued: VecDeque::new(),
            active: None,
        })
    }

    pub fn enqueue(&mut self, command: String, deadline: Option<Instant>) -> Result<u64> {
        human_terminal_command_submission(&command)?;
        if self.queued.len() >= self.capacity {
            return Err(ProcessError::new(
                ProcessErrorCode::InputBackpressure,
                "agent terminal command queue is full",
            ));
        }
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.checked_add(1).ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::StateConflict,
                "agent terminal command sequence overflowed",
            )
        })?;
        self.queued.push_back(QueuedTerminalCommand {
            command_sequence: sequence,
            command,
            deadline,
        });
        Ok(sequence)
    }

    pub fn begin_next(
        &mut self,
        prompt: &TerminalPromptState,
        output_after_sequence: u64,
    ) -> Result<Option<&QueuedTerminalCommand>> {
        if self.active.is_some() {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "agent terminal already has an active command",
            ));
        }
        if !prompt.is_trusted_ready() {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "agent terminal command requires a trusted fresh prompt",
            ));
        }
        let Some(command) = self.queued.pop_front() else {
            return Ok(None);
        };
        self.active = Some(ActiveCommand {
            command,
            integration_start_sequence: None,
            output_after_sequence,
            submission_reserved: false,
            submitted: false,
        });
        Ok(self.active.as_ref().map(|active| &active.command))
    }

    pub fn mark_started(
        &mut self,
        integration_sequence: u64,
        output_after_sequence: u64,
    ) -> Result<()> {
        let active = self.active.as_mut().ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::StateConflict,
                "shell integration started a command with no active queue item",
            )
        })?;
        if integration_sequence == 0
            || active
                .integration_start_sequence
                .replace(integration_sequence)
                .is_some()
        {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "active agent command has an invalid duplicate integration start",
            ));
        }
        active.output_after_sequence = output_after_sequence;
        Ok(())
    }

    pub fn reserve_submission(&mut self) -> Result<ProcessBytes> {
        let active = self.active.as_mut().ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::StateConflict,
                "agent terminal has no active command to submit",
            )
        })?;
        if active.submitted || active.submission_reserved {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "active agent command submission is already reserved or complete",
            ));
        }
        active.submission_reserved = true;
        Ok(active.command.submission())
    }

    pub fn complete_submission(&mut self) -> Result<()> {
        let active = self.active.as_mut().ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::StateConflict,
                "agent terminal has no active command submission",
            )
        })?;
        if !active.submission_reserved || active.submitted {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "active agent command submission reservation is invalid",
            ));
        }
        active.submission_reserved = false;
        active.submitted = true;
        Ok(())
    }

    pub fn abandon_submission(&mut self) {
        if let Some(active) = self.active.as_mut()
            && !active.submitted
        {
            active.submission_reserved = false;
        }
    }

    pub fn finish(
        &mut self,
        terminal_id: TerminalSessionId,
        execution_id: ExecutionId,
        integration_finish_sequence: u64,
        exit: ShellExit,
        cwd: PathBuf,
        output_through_sequence: u64,
    ) -> Result<TerminalCommandResult> {
        let active = self.active.take().ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::StateConflict,
                "shell integration finished a command with no active queue item",
            )
        })?;
        let start = active.integration_start_sequence.ok_or_else(|| {
            ProcessError::new(
                ProcessErrorCode::StateConflict,
                "shell integration finished before command_started",
            )
        })?;
        if integration_finish_sequence <= start
            || output_through_sequence < active.output_after_sequence
        {
            return Err(ProcessError::new(
                ProcessErrorCode::StateConflict,
                "agent terminal command boundary is not monotonic",
            ));
        }
        Ok(TerminalCommandResult {
            terminal_id,
            execution_id,
            command_sequence: active.command.command_sequence,
            cwd,
            exit,
            output: TerminalCommandOutputRange {
                after_sequence: active.output_after_sequence,
                through_sequence: output_through_sequence,
            },
        })
    }

    pub fn cancel_active(&mut self) -> Option<u64> {
        self.active
            .take()
            .map(|active| active.command.command_sequence)
    }

    pub fn cancel_queued(&mut self, command_sequence: u64) -> bool {
        let Some(position) = self
            .queued
            .iter()
            .position(|command| command.command_sequence == command_sequence)
        else {
            return false;
        };
        self.queued.remove(position);
        true
    }

    pub fn cancel_all(&mut self) -> Vec<u64> {
        let mut cancelled = self
            .active
            .take()
            .map(|active| vec![active.command.command_sequence])
            .unwrap_or_default();
        cancelled.extend(
            self.queued
                .drain(..)
                .map(|command| command.command_sequence),
        );
        cancelled
    }

    pub fn is_queued(&self, command_sequence: u64) -> bool {
        self.queued
            .iter()
            .any(|command| command.command_sequence == command_sequence)
    }

    pub fn active_deadline(&self) -> Option<Instant> {
        self.active
            .as_ref()
            .and_then(|active| active.command.deadline)
    }

    pub fn active_is_submitted(&self) -> bool {
        self.active.as_ref().is_some_and(|active| active.submitted)
    }

    pub fn queued_len(&self) -> usize {
        self.queued.len()
    }

    pub fn active_sequence(&self) -> Option<u64> {
        self.active
            .as_ref()
            .map(|active| active.command.command_sequence)
    }

    pub fn active_command(&self) -> Option<&str> {
        self.active
            .as_ref()
            .map(|active| active.command.command.as_str())
    }
}

impl Default for AgentTerminalCommandQueue {
    fn default() -> Self {
        Self::new(DEFAULT_AGENT_TERMINAL_QUEUE_CAPACITY)
            .expect("default terminal queue capacity is nonzero")
    }
}

impl Debug for AgentTerminalCommandQueue {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentTerminalCommandQueue")
            .field("capacity", &self.capacity)
            .field("next_sequence", &self.next_sequence)
            .field("queued", &self.queued.len())
            .field("active_sequence", &self.active_sequence())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_command_card_text_is_inert(text: &str) {
        assert!(text.chars().all(|character| {
            matches!(character, '\n' | '\t')
                || (!character.is_control() && !is_unicode_format_control(character as u32))
        }));
    }

    #[test]
    fn human_command_is_one_bracketed_paste_transaction() {
        let submission = human_terminal_command_submission("cd dir\nprintf '%s' ok")
            .unwrap()
            .decode(MAX_HUMAN_TERMINAL_COMMAND_BYTES + 32)
            .unwrap();
        assert_eq!(submission, b"\x1b[200~cd dir\nprintf '%s' ok\x1b[201~\n");
        for invalid in ["", "bad\rline", "bad\u{1b}[31m", "bad\u{7f}"] {
            assert_eq!(
                human_terminal_command_submission(invalid)
                    .unwrap_err()
                    .code(),
                ProcessErrorCode::InvalidRequest
            );
        }
        let exact_limit = "x".repeat(MAX_TYPED_TERMINAL_COMMAND_BYTES);
        assert!(human_terminal_command_submission(&exact_limit).is_ok());
        assert_eq!(
            human_terminal_command_submission(&format!("{exact_limit}x"))
                .unwrap_err()
                .code(),
            ProcessErrorCode::InvalidRequest
        );
    }

    #[test]
    fn command_card_output_is_plain_bounded_text() {
        let output = sanitize_terminal_card_output(
            b"plain\x1b[31mred\x1b[0m\n\x1b]52;c;secret\x07tail\xe2\x80\xae",
            256,
        );
        assert_eq!(
            output.text,
            "plain[filtered-control]red[filtered-control]\n[filtered-control]tail\\u{202E}"
        );
        assert!(!output.text.contains("secret"));
        assert!(output.filtered_control_sequences >= 3);
        assert!(!output.truncated);

        let bounded = sanitize_terminal_card_output(b"123456", 5);
        assert_eq!(bounded.text, "12345");
        assert!(bounded.truncated);

        let marked = sanitize_terminal_card_output(&[b'x'; 100], 32);
        assert!(marked.text.ends_with(TRUNCATED_OUTPUT_MARKER));
        assert!(marked.text.len() <= 32);
    }

    #[test]
    fn command_card_sanitizer_is_incremental_across_every_boundary() {
        let input = b"unicode:\xf0\x9f\xa6\x80\r\n\x1b[31mred\x1b[0m\x1b]52;c;private\x1b\\tail\xe2\x80\xae\xff";
        let expected = sanitize_terminal_card_output(input, 512);
        for split in 0..=input.len() {
            let mut sanitizer = CommandCardSanitizer::new(512);
            sanitizer.push(&input[..split]);
            sanitizer.push(&input[split..]);
            assert_eq!(sanitizer.finish(), expected, "split at byte {split}");
        }
        let mut bytewise = CommandCardSanitizer::new(512);
        for byte in input {
            bytewise.push(std::slice::from_ref(byte));
        }
        assert_eq!(bytewise.finish(), expected);
        assert!(expected.text.contains("unicode:🦀\n"));
        assert!(!expected.text.contains("private"));
        assert!(expected.text.contains("\\u{202E}"));
        assert!(expected.text.contains("\\xFF"));
    }

    #[test]
    fn command_card_sanitizer_blocks_c0_c1_strings_and_oversized_payloads() {
        let mut input = Vec::new();
        input.extend_from_slice(b"safe\0\x7f");
        input.extend_from_slice(b"\xc2\x9b31mvisible");
        input.extend_from_slice(b"\xc2\x9dprivate\x07");
        input.extend_from_slice(b"\x1bP");
        input.extend(std::iter::repeat_n(b'q', MAX_CONTROL_SEQUENCE_BYTES + 32));
        input.extend_from_slice(b"\x1b\\tail");
        let output = sanitize_terminal_card_output(&input, 512);
        assert!(output.text.starts_with("safe\\x00\\x7F"));
        assert!(output.text.contains("visible"));
        assert!(!output.text.contains("private"));
        assert!(!output.text.contains("qqqq"));
        assert!(output.text.ends_with("tail"));
        assert!(output.filtered_control_sequences >= 4);
        assert_command_card_text_is_inert(&output.text);
    }

    #[test]
    fn command_card_sanitizer_covers_every_c0_c1_del_and_format_class() {
        for byte in (0x00_u8..=0x1f).chain(0x7f..=0x9f) {
            let output = sanitize_terminal_card_output(&[byte], 128);
            assert_command_card_text_is_inert(output.text());
            match byte {
                b'\n' | b'\r' => assert_eq!(output.text(), "\n", "byte {byte:#04x}"),
                b'\t' => assert_eq!(output.text(), "\t", "byte {byte:#04x}"),
                _ => assert_ne!(output.text().as_bytes(), [byte], "byte {byte:#04x}"),
            }
        }

        let format_controls = [
            0x00ad, 0x061c, 0x06dd, 0x070f, 0x180e, 0x200b, 0x200e, 0x202a, 0x202e, 0x2060, 0x2066,
            0x206f, 0xfeff, 0xfff9, 0x110bd, 0x110cd, 0x13430, 0x1bca0, 0x1d173, 0xe0001, 0xe0020,
            0xe007f,
        ];
        for code in format_controls {
            let character = char::from_u32(code).expect("fixture is a Unicode scalar");
            assert!(is_unicode_format_control(code));
            let output = sanitize_terminal_card_output(character.to_string().as_bytes(), 128);
            assert_command_card_text_is_inert(output.text());
            assert_eq!(output.text(), format!("\\u{{{code:X}}}"));
        }
    }

    #[test]
    fn command_card_sanitizer_blocks_private_strings_at_every_byte_boundary() {
        let fixtures: &[&[u8]] = &[
            b"before\x1b]52;c;PRIVATE_OSC\x07after",
            b"before\x1bPPRIVATE_DCS\x1b\\after",
            b"before\x1b_PRIVATE_APC\x1b\\after",
            b"before\x1b^PRIVATE_PM\x1b\\after",
            b"before\x9dPRIVATE_C1_OSC\x9cafter",
            b"before\x90PRIVATE_C1_DCS\x9cafter",
            b"before\x9fPRIVATE_C1_APC\x9cafter",
            b"before\x9ePRIVATE_C1_PM\x9cafter",
        ];
        for input in fixtures {
            let expected = sanitize_terminal_card_output(input, 512);
            assert_eq!(expected.text(), "before[filtered-control]after");
            assert_command_card_text_is_inert(expected.text());
            for split in 0..=input.len() {
                let mut sanitizer = CommandCardSanitizer::new(512);
                sanitizer.push(&input[..split]);
                sanitizer.push(&input[split..]);
                assert_eq!(sanitizer.finish(), expected, "split {split}: {input:?}");
            }
        }
    }

    #[test]
    fn command_card_limit_is_utf8_safe_and_drain_continues_after_truncation() {
        const CARD_LIMIT: usize = 256 * 1024;
        const PRIVATE_AFTER_LIMIT: &str = "PRIVATE_AFTER_CARD_LIMIT";
        let mut input = vec![b'x'; CARD_LIMIT - 5];
        input.extend_from_slice("🦀界".as_bytes());
        input.extend_from_slice(b"tail\x1b]52;c;");
        input.extend_from_slice(PRIVATE_AFTER_LIMIT.as_bytes());
        input.push(0x07);

        let expected = sanitize_terminal_card_output(&input, CARD_LIMIT);
        assert!(expected.truncated());
        assert_eq!(expected.text().len(), CARD_LIMIT);
        assert_eq!(expected.text().matches(TRUNCATED_OUTPUT_MARKER).count(), 1);
        assert!(expected.text().ends_with(TRUNCATED_OUTPUT_MARKER));
        assert!(!expected.text().contains(PRIVATE_AFTER_LIMIT));
        assert!(expected.filtered_effects() > 0);
        assert_command_card_text_is_inert(expected.text());
        let debug = format!("{expected:?}");
        assert!(!debug.contains(PRIVATE_AFTER_LIMIT));
        assert!(!debug.contains(expected.text()));

        for split in CARD_LIMIT.saturating_sub(12)..=(CARD_LIMIT + 12) {
            let mut sanitizer = CommandCardSanitizer::new(CARD_LIMIT);
            sanitizer.push(&input[..split]);
            sanitizer.push(&input[split..]);
            assert_eq!(sanitizer.finish(), expected, "split at byte {split}");
        }
    }

    #[test]
    fn commands_are_fifo_exact_and_never_interleave() {
        let mut queue = AgentTerminalCommandQueue::new(2).unwrap();
        assert_eq!(
            queue
                .enqueue("cd dir && printf '%s' x".to_owned(), None)
                .unwrap(),
            1
        );
        assert_eq!(queue.enqueue("pwd".to_owned(), None).unwrap(), 2);
        assert_eq!(
            queue.enqueue("third".to_owned(), None).unwrap_err().code(),
            ProcessErrorCode::InputBackpressure
        );
        let prompt = TerminalPromptState::Ready {
            sequence: 1,
            last_exit: None,
        };
        let first = queue.begin_next(&prompt, 8).unwrap().unwrap();
        assert_eq!(first.command(), "cd dir && printf '%s' x");
        assert_eq!(
            first.submission().decode(1024).unwrap(),
            b"\x1b[200~cd dir && printf '%s' x\x1b[201~\n"
        );
        assert_eq!(
            queue.begin_next(&prompt, 8).unwrap_err().code(),
            ProcessErrorCode::StateConflict
        );
        queue.mark_started(2, 11).unwrap();
        let result = queue
            .finish(
                TerminalSessionId::generate(),
                ExecutionId::generate(),
                3,
                ShellExit::Code { code: 0 },
                PathBuf::from("/workspace/dir"),
                12,
            )
            .unwrap();
        assert_eq!(result.command_sequence, 1);
        assert_eq!(result.output.after_sequence, 11);
        assert_eq!(result.output.through_sequence, 12);
        assert_eq!(queue.queued_len(), 1);
    }

    #[test]
    fn degraded_or_busy_prompt_cannot_submit_agent_bytes() {
        let mut queue = AgentTerminalCommandQueue::new(1).unwrap();
        queue.enqueue("true".to_owned(), None).unwrap();
        assert_eq!(
            queue
                .begin_next(&TerminalPromptState::Degraded, 0)
                .unwrap_err()
                .code(),
            ProcessErrorCode::StateConflict
        );
        assert_eq!(queue.queued_len(), 1);
    }
}
