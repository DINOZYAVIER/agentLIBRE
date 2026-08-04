use std::fmt::{self, Debug, Formatter};

use agl_exec::{ProcessBytes, ProcessError, ProcessErrorCode, Result};

pub const MAX_TYPED_TERMINAL_COMMAND_BYTES: usize = 64 * 1024;
pub const MAX_HUMAN_TERMINAL_COMMAND_BYTES: usize = MAX_TYPED_TERMINAL_COMMAND_BYTES;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_submission_is_bracketed_and_rejects_controls() {
        assert_eq!(
            human_terminal_command_submission("printf ok")
                .unwrap()
                .decode(64)
                .unwrap(),
            b"\x1b[200~printf ok\x1b[201~\n"
        );
        assert!(human_terminal_command_submission("printf\0bad").is_err());
    }

    #[test]
    fn sanitizer_removes_terminal_effects_and_stays_bounded() {
        let output = sanitize_terminal_card_output(b"safe\x1b[31mred\x1b[0m", 32);
        assert!(output.text().contains("safe"));
        assert!(!output.text().contains("\x1b"));
        assert!(output.filtered_effects() >= 2);
        assert!(output.text().len() <= 32);
    }

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
}
