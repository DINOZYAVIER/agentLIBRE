use std::collections::BTreeMap;
use std::fmt::Write as _;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const C1_DCS: u8 = 0x90;
const C1_SOS: u8 = 0x98;
const C1_CSI: u8 = 0x9b;
const C1_ST: u8 = 0x9c;
const C1_OSC: u8 = 0x9d;
const C1_PM: u8 = 0x9e;
const C1_APC: u8 = 0x9f;
const MAX_CONTROL_SEQUENCE_BYTES: usize = 4096;
const MAX_KEYBOARD_MODE_SEQUENCE_BYTES: usize = 64;
const MAX_KEYBOARD_MODE_STACK_DEPTH: usize = 16;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct FilterReport {
    pub bytes: Vec<u8>,
    pub blocked_sequences: u64,
    pub malformed_sequences: u64,
}

#[derive(Clone, Debug)]
enum ParserState {
    Ground,
    Utf8 {
        bytes: Vec<u8>,
        expected_len: usize,
    },
    Escape(Vec<u8>),
    Csi(Vec<u8>),
    BlockedString {
        kind: StringKind,
        saw_escape: bool,
        utf8_remaining: u8,
        length: usize,
        overflow_reported: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StringKind {
    Osc,
    Dcs,
    Apc,
    Pm,
    Sos,
    LegacyTitle,
}

#[derive(Clone, Debug)]
pub(crate) struct TerminalOutputFilter {
    state: ParserState,
    visible: bool,
    alternate_screen: bool,
    application_keypad: Option<bool>,
    private_modes: BTreeMap<u16, bool>,
    keyboard_mode_set: Option<Vec<u8>>,
    keyboard_mode_stack: Vec<Vec<u8>>,
    modify_other_keys: Option<Vec<u8>>,
    blocked_total: u64,
    malformed_total: u64,
}

impl Default for TerminalOutputFilter {
    fn default() -> Self {
        Self::new(false)
    }
}

impl TerminalOutputFilter {
    pub(crate) fn new(visible: bool) -> Self {
        Self {
            state: ParserState::Ground,
            visible,
            alternate_screen: false,
            application_keypad: None,
            private_modes: BTreeMap::new(),
            keyboard_mode_set: None,
            keyboard_mode_stack: Vec::new(),
            modify_other_keys: None,
            blocked_total: 0,
            malformed_total: 0,
        }
    }

    pub(crate) fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    pub(crate) fn alternate_screen(&self) -> bool {
        self.alternate_screen
    }

    pub(crate) fn filter(&mut self, input: &[u8]) -> FilterReport {
        self.filter_with_alternate_policy(input, false)
    }

    pub(crate) fn filter_stale_replay(&mut self, input: &[u8]) -> FilterReport {
        self.filter_with_alternate_policy(input, true)
    }

    fn filter_with_alternate_policy(
        &mut self,
        input: &[u8],
        suppress_alternate: bool,
    ) -> FilterReport {
        let blocked_before = self.blocked_total;
        let malformed_before = self.malformed_total;
        let mut output = Vec::with_capacity(input.len());
        for &byte in input {
            let was_alternate = self.alternate_screen;
            let output_start = output.len();
            self.accept_byte(byte, &mut output);
            if suppress_alternate && (was_alternate || self.alternate_screen) {
                output.truncate(output_start);
            }
        }
        FilterReport {
            bytes: output,
            blocked_sequences: self.blocked_total - blocked_before,
            malformed_sequences: self.malformed_total - malformed_before,
        }
    }

    pub(crate) fn finish(&mut self) -> FilterReport {
        let malformed = !matches!(self.state, ParserState::Ground);
        self.state = ParserState::Ground;
        if malformed {
            self.malformed_total = self.malformed_total.saturating_add(1);
        }
        FilterReport {
            bytes: Vec::new(),
            blocked_sequences: 0,
            malformed_sequences: u64::from(malformed),
        }
    }

    pub(crate) fn blocked_total(&self) -> u64 {
        self.blocked_total
    }

    pub(crate) fn malformed_total(&self) -> u64 {
        self.malformed_total
    }

    /// Reassert the terminal modes last requested by the attached program after
    /// Chat temporarily restored its own baseline. This is routing state, not a
    /// screen model.
    pub(crate) fn terminal_mode_restore_bytes(&self) -> Vec<u8> {
        let mut sequence = String::new();
        for (mode, enabled) in &self.private_modes {
            let _ = write!(sequence, "\x1b[?{mode}{}", if *enabled { 'h' } else { 'l' });
        }
        if let Some(keyboard_mode_set) = &self.keyboard_mode_set {
            sequence.push_str(std::str::from_utf8(keyboard_mode_set).unwrap_or_default());
        }
        for keyboard_mode in &self.keyboard_mode_stack {
            sequence.push_str(std::str::from_utf8(keyboard_mode).unwrap_or_default());
        }
        if let Some(modify_other_keys) = &self.modify_other_keys {
            sequence.push_str(std::str::from_utf8(modify_other_keys).unwrap_or_default());
        }
        if let Some(application_keypad) = self.application_keypad {
            sequence.push_str(if application_keypad { "\x1b=" } else { "\x1b>" });
        }
        sequence.into_bytes()
    }

    /// Return only modes touched by the attached program to the Chat renderer's
    /// known baseline. Untouched parent-terminal state is left alone.
    pub(crate) fn chat_mode_restore_bytes(&self) -> Vec<u8> {
        let mut sequence = String::from("\x1b[0m");
        for mode in self.private_modes.keys() {
            let enabled = matches!(*mode, 7 | 25 | 2004);
            let _ = write!(sequence, "\x1b[?{mode}{}", if enabled { 'h' } else { 'l' });
        }
        for _ in &self.keyboard_mode_stack {
            sequence.push_str("\x1b[<u");
        }
        if self.keyboard_mode_set.is_some() {
            sequence.push_str("\x1b[=0u");
        }
        if self.modify_other_keys.is_some() {
            sequence.push_str("\x1b[>4;0m");
        }
        if self.application_keypad.is_some() {
            sequence.push_str("\x1b>");
        }
        sequence.into_bytes()
    }

    fn accept_byte(&mut self, byte: u8, output: &mut Vec<u8>) {
        let state = std::mem::replace(&mut self.state, ParserState::Ground);
        self.state = match state {
            ParserState::Ground => self.accept_ground(byte, output),
            ParserState::Utf8 {
                mut bytes,
                expected_len,
            } => {
                if (0x80..=0xbf).contains(&byte) {
                    bytes.push(byte);
                    if bytes.len() == expected_len {
                        match std::str::from_utf8(&bytes) {
                            Ok(text) if !text.chars().any(char::is_control) => {
                                output.extend_from_slice(&bytes);
                            }
                            Ok(_) => {
                                self.blocked_total = self.blocked_total.saturating_add(1);
                            }
                            Err(_) => {
                                self.malformed_total = self.malformed_total.saturating_add(1);
                            }
                        }
                        ParserState::Ground
                    } else {
                        ParserState::Utf8 {
                            bytes,
                            expected_len,
                        }
                    }
                } else {
                    self.malformed_total = self.malformed_total.saturating_add(1);
                    self.accept_ground(byte, output)
                }
            }
            ParserState::Escape(mut sequence) => {
                if byte == b'[' {
                    sequence.push(byte);
                    ParserState::Csi(sequence)
                } else if let Some(kind) = string_kind_after_escape(byte) {
                    self.blocked_total = self.blocked_total.saturating_add(1);
                    ParserState::BlockedString {
                        kind,
                        saw_escape: false,
                        utf8_remaining: 0,
                        length: 2,
                        overflow_reported: false,
                    }
                } else if (0x20..=0x2f).contains(&byte) {
                    sequence.push(byte);
                    if sequence.len() > MAX_CONTROL_SEQUENCE_BYTES {
                        self.malformed_total = self.malformed_total.saturating_add(1);
                        ParserState::Ground
                    } else {
                        ParserState::Escape(sequence)
                    }
                } else if (0x30..=0x7e).contains(&byte) && byte != b'\\' {
                    sequence.push(byte);
                    self.track_escape(&sequence);
                    output.extend_from_slice(&sequence);
                    ParserState::Ground
                } else {
                    self.malformed_total = self.malformed_total.saturating_add(1);
                    self.accept_ground(byte, output)
                }
            }
            ParserState::Csi(mut sequence) => {
                sequence.push(byte);
                if sequence.len() > MAX_CONTROL_SEQUENCE_BYTES {
                    self.malformed_total = self.malformed_total.saturating_add(1);
                    ParserState::Ground
                } else if (0x40..=0x7e).contains(&byte) {
                    if self.csi_allowed(&sequence, byte) {
                        self.track_csi(&sequence, byte);
                        output.extend_from_slice(&sequence);
                    } else {
                        self.blocked_total = self.blocked_total.saturating_add(1);
                    }
                    ParserState::Ground
                } else if (0x20..=0x3f).contains(&byte) {
                    ParserState::Csi(sequence)
                } else {
                    self.malformed_total = self.malformed_total.saturating_add(1);
                    self.accept_ground(byte, output)
                }
            }
            ParserState::BlockedString {
                kind,
                mut saw_escape,
                mut utf8_remaining,
                mut length,
                mut overflow_reported,
            } => {
                length = length.saturating_add(1);
                if length > MAX_CONTROL_SEQUENCE_BYTES && !overflow_reported {
                    self.malformed_total = self.malformed_total.saturating_add(1);
                    overflow_reported = true;
                }
                let byte_is_utf8_payload = if utf8_remaining > 0 {
                    if (0x80..=0xbf).contains(&byte) {
                        utf8_remaining -= 1;
                        true
                    } else {
                        self.malformed_total = self.malformed_total.saturating_add(1);
                        utf8_remaining = 0;
                        false
                    }
                } else if let Some(remaining) = utf8_continuation_count(byte) {
                    utf8_remaining = remaining;
                    true
                } else {
                    false
                };
                let terminated = !byte_is_utf8_payload
                    && (byte == C1_ST
                        || (kind == StringKind::Osc && byte == BEL)
                        || (saw_escape && byte == b'\\'));
                if terminated {
                    ParserState::Ground
                } else {
                    saw_escape = !byte_is_utf8_payload && byte == ESC;
                    ParserState::BlockedString {
                        kind,
                        saw_escape,
                        utf8_remaining,
                        length,
                        overflow_reported,
                    }
                }
            }
        };
    }

    fn accept_ground(&mut self, byte: u8, output: &mut Vec<u8>) -> ParserState {
        match byte {
            ESC => ParserState::Escape(vec![ESC]),
            C1_CSI => ParserState::Csi(vec![C1_CSI]),
            C1_OSC | C1_DCS | C1_APC | C1_PM | C1_SOS => {
                self.blocked_total = self.blocked_total.saturating_add(1);
                ParserState::BlockedString {
                    kind: match byte {
                        C1_OSC => StringKind::Osc,
                        C1_DCS => StringKind::Dcs,
                        C1_APC => StringKind::Apc,
                        C1_PM => StringKind::Pm,
                        _ => StringKind::Sos,
                    },
                    saw_escape: false,
                    utf8_remaining: 0,
                    length: 1,
                    overflow_reported: false,
                }
            }
            0xc2..=0xdf => ParserState::Utf8 {
                bytes: vec![byte],
                expected_len: 2,
            },
            0xe0..=0xef => ParserState::Utf8 {
                bytes: vec![byte],
                expected_len: 3,
            },
            0xf0..=0xf4 => ParserState::Utf8 {
                bytes: vec![byte],
                expected_len: 4,
            },
            BEL | C1_ST | 0x00 | 0x7f => {
                self.blocked_total = self.blocked_total.saturating_add(1);
                ParserState::Ground
            }
            0x08..=0x0f => {
                output.push(byte);
                ParserState::Ground
            }
            0x01..=0x06 | 0x10..=0x1a | 0x1c..=0x1f => {
                self.blocked_total = self.blocked_total.saturating_add(1);
                ParserState::Ground
            }
            0x80..=0x9f | 0xa0..=0xbf | 0xc0..=0xc1 | 0xf5..=0xff => {
                self.malformed_total = self.malformed_total.saturating_add(1);
                ParserState::Ground
            }
            _ => {
                output.push(byte);
                ParserState::Ground
            }
        }
    }

    fn csi_allowed(&self, sequence: &[u8], final_byte: u8) -> bool {
        match final_byte {
            b't' => false,
            b'c' | b'n' => self.visible && safe_device_query(sequence, final_byte),
            b'u' => self.keyboard_mode_allowed(sequence),
            b'm' if csi_parameters(sequence).starts_with(b">4;") => {
                sequence.len() <= MAX_KEYBOARD_MODE_SEQUENCE_BYTES
            }
            _ => true,
        }
    }

    fn track_csi(&mut self, sequence: &[u8], final_byte: u8) {
        self.track_keyboard_mode(sequence, final_byte);
        if !matches!(final_byte, b'h' | b'l') {
            return;
        }
        let parameters = csi_parameters(sequence);
        let Some(private) = parameters.strip_prefix(b"?") else {
            return;
        };
        let enabled = final_byte == b'h';
        for mode in private.split(|byte| *byte == b';') {
            let Ok(mode) = std::str::from_utf8(mode).unwrap_or_default().parse::<u16>() else {
                continue;
            };
            if tracked_private_mode(mode) {
                self.private_modes.insert(mode, enabled);
            }
            if matches!(mode, 47 | 1047 | 1049) {
                self.alternate_screen = enabled;
            }
        }
    }

    fn track_escape(&mut self, sequence: &[u8]) {
        match sequence {
            [ESC, b'='] => self.application_keypad = Some(true),
            [ESC, b'>'] => self.application_keypad = Some(false),
            _ => {}
        }
    }

    fn keyboard_mode_allowed(&self, sequence: &[u8]) -> bool {
        let parameters = csi_parameters(sequence);
        if !matches!(parameters.first(), Some(b'>') | Some(b'=') | Some(b'<')) {
            return true;
        }
        if sequence.len() > MAX_KEYBOARD_MODE_SEQUENCE_BYTES {
            return false;
        }
        !parameters.starts_with(b">")
            || self.keyboard_mode_stack.len() < MAX_KEYBOARD_MODE_STACK_DEPTH
    }

    fn track_keyboard_mode(&mut self, sequence: &[u8], final_byte: u8) {
        let parameters = csi_parameters(sequence);
        if final_byte == b'u' {
            if parameters.starts_with(b">") {
                self.keyboard_mode_stack.push(sequence.to_vec());
            } else if let Some(pop) = parameters.strip_prefix(b"<") {
                let count = std::str::from_utf8(pop)
                    .ok()
                    .and_then(|count| count.parse::<usize>().ok())
                    .unwrap_or(1)
                    .max(1);
                self.keyboard_mode_stack
                    .truncate(self.keyboard_mode_stack.len().saturating_sub(count));
            } else if parameters.starts_with(b"=") {
                self.keyboard_mode_set = Some(sequence.to_vec());
            }
        } else if final_byte == b'm'
            && parameters.starts_with(b">4;")
            && sequence.len() <= MAX_KEYBOARD_MODE_SEQUENCE_BYTES
        {
            self.modify_other_keys = Some(sequence.to_vec());
        }
    }
}

fn utf8_continuation_count(byte: u8) -> Option<u8> {
    match byte {
        0xc2..=0xdf => Some(1),
        0xe0..=0xef => Some(2),
        0xf0..=0xf4 => Some(3),
        _ => None,
    }
}

fn tracked_private_mode(mode: u16) -> bool {
    matches!(
        mode,
        1 | 6
            | 7
            | 25
            | 47
            | 1000
            | 1002
            | 1003
            | 1004
            | 1005
            | 1006
            | 1015
            | 1047
            | 1049
            | 2004
            | 2026
    )
}

fn string_kind_after_escape(byte: u8) -> Option<StringKind> {
    match byte {
        b']' => Some(StringKind::Osc),
        b'P' => Some(StringKind::Dcs),
        b'_' => Some(StringKind::Apc),
        b'^' => Some(StringKind::Pm),
        b'X' => Some(StringKind::Sos),
        b'k' => Some(StringKind::LegacyTitle),
        _ => None,
    }
}

fn safe_device_query(sequence: &[u8], final_byte: u8) -> bool {
    let parameters = csi_parameters(sequence);
    match final_byte {
        b'c' => matches!(parameters, b"" | b">"),
        b'n' => matches!(parameters, b"5" | b"6" | b"?6"),
        _ => false,
    }
}

fn csi_parameters(sequence: &[u8]) -> &[u8] {
    if sequence.starts_with(&[ESC, b'[']) {
        &sequence[2..sequence.len().saturating_sub(1)]
    } else {
        &sequence[1..sequence.len().saturating_sub(1)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filtered_in_splits(input: &[u8], split: usize, visible: bool) -> FilterReport {
        let mut filter = TerminalOutputFilter::new(visible);
        let first = filter.filter(&input[..split]);
        let second = filter.filter(&input[split..]);
        FilterReport {
            bytes: [first.bytes, second.bytes].concat(),
            blocked_sequences: first.blocked_sequences + second.blocked_sequences,
            malformed_sequences: first.malformed_sequences + second.malformed_sequences,
        }
    }

    #[test]
    fn safe_terminal_rendering_survives_every_chunk_boundary() {
        let input = b"plain\x1b[31mred\x1b[0m\x1b[?1049hvim\x1b[?1049l\r\n";
        for split in 0..=input.len() {
            let report = filtered_in_splits(input, split, true);
            assert_eq!(report.bytes, input, "split {split}");
            assert_eq!(report.blocked_sequences, 0, "split {split}");
            assert_eq!(report.malformed_sequences, 0, "split {split}");
        }
    }

    #[test]
    fn alternate_screen_state_tracks_split_private_modes() {
        let mut filter = TerminalOutputFilter::new(true);
        assert_eq!(filter.filter(b"\x1b[?1049").bytes, b"");
        assert_eq!(filter.filter(b"h").bytes, b"\x1b[?1049h");
        assert!(filter.alternate_screen());
        assert_eq!(filter.filter(b"\x1b[?1049l").bytes, b"\x1b[?1049l");
        assert!(!filter.alternate_screen());
    }

    #[test]
    fn host_effect_strings_are_removed_at_every_chunk_boundary() {
        let input = b"a\x1b]52;c;c2VjcmV0\x07b\x1b]0;title\x1b\\c\x1bPfile\x1b\\d\x1b_payload\x1b\\e\x1b^notice\x1b\\f\x1bklegacy title\x1b\\g";
        for split in 0..=input.len() {
            let report = filtered_in_splits(input, split, true);
            assert_eq!(report.bytes, b"abcdefg", "split {split}");
            assert_eq!(report.blocked_sequences, 6, "split {split}");
        }
    }

    #[test]
    fn concrete_host_effect_protocols_are_blocked_at_every_chunk_boundary() {
        let input = b"a\x1b]52;c;Y2xpcGJvYXJk\x07\
                      b\x1b]0;window title\x1b\\\
                      c\x1b]1337;File=name=dGVzdA==:cGF5bG9hZA==\x07\
                      d\x1bPqSIXEL_PAYLOAD\x1b\\\
                      e\x1b_Gf=100;KITTY_IMAGE\x1b\\\
                      f\x1b^PRIVATE_MESSAGE\x1b\\\
                      g";
        for split in 0..=input.len() {
            let report = filtered_in_splits(input, split, true);
            assert_eq!(report.bytes, b"abcdefg", "split {split}");
            assert_eq!(report.blocked_sequences, 6, "split {split}");
            assert_eq!(report.malformed_sequences, 0, "split {split}");
        }
    }

    #[test]
    fn safe_terminal_queries_survive_every_chunk_boundary_only_while_visible() {
        let input = b"a\x1b[c b\x1b[>c c\x1b[5n d\x1b[6n e\x1b[?6n f";
        for split in 0..=input.len() {
            let visible = filtered_in_splits(input, split, true);
            assert_eq!(visible.bytes, input, "visible split {split}");
            assert_eq!(visible.blocked_sequences, 0, "visible split {split}");
            assert_eq!(visible.malformed_sequences, 0, "visible split {split}");

            let hidden = filtered_in_splits(input, split, false);
            assert_eq!(hidden.bytes, b"a b c d e f", "hidden split {split}");
            assert_eq!(hidden.blocked_sequences, 5, "hidden split {split}");
            assert_eq!(hidden.malformed_sequences, 0, "hidden split {split}");
        }
    }

    #[test]
    fn device_queries_are_allowed_only_while_visible_and_window_ops_are_blocked() {
        let input = b"x\x1b[6ny\x1b[8;40;120tz";
        let mut hidden = TerminalOutputFilter::new(false);
        let hidden_report = hidden.filter(input);
        assert_eq!(hidden_report.bytes, b"xyz");
        assert_eq!(hidden_report.blocked_sequences, 2);

        let mut visible = TerminalOutputFilter::new(true);
        let visible_report = visible.filter(input);
        assert_eq!(visible_report.bytes, b"x\x1b[6nyz");
        assert_eq!(visible_report.blocked_sequences, 1);
    }

    #[test]
    fn oversized_control_string_stays_bounded_and_recovers_at_terminator() {
        let mut input = b"before\x1b]52;c;".to_vec();
        input.extend(std::iter::repeat_n(b'x', MAX_CONTROL_SEQUENCE_BYTES + 10));
        input.extend_from_slice(b"\x07after");
        let mut filter = TerminalOutputFilter::new(true);
        let report = filter.filter(&input);
        assert_eq!(report.bytes, b"beforeafter");
        assert_eq!(report.blocked_sequences, 1);
        assert_eq!(report.malformed_sequences, 1);
    }

    #[test]
    fn incomplete_sequence_is_reported_without_exposing_payload() {
        let mut filter = TerminalOutputFilter::new(true);
        let report = filter.filter(b"ok\x1b]52;c;secret");
        assert_eq!(report.bytes, b"ok");
        assert_eq!(filter.finish().malformed_sequences, 1);
        assert_eq!(filter.blocked_total(), 1);
        assert_eq!(filter.malformed_total(), 1);
    }

    #[test]
    fn touched_interactive_modes_can_be_suspended_for_chat_and_restored() {
        let mut filter = TerminalOutputFilter::new(true);
        let controls = b"\x1b=\x1b[?1049h\x1b[?1000;1004;1006h\x1b[?25l\x1b[?2004h";
        assert_eq!(filter.filter(controls).bytes, controls);

        let chat = String::from_utf8(filter.chat_mode_restore_bytes()).unwrap();
        assert!(chat.contains("\x1b[?1049l"));
        assert!(chat.contains("\x1b[?1000l"));
        assert!(chat.contains("\x1b[?1004l"));
        assert!(chat.contains("\x1b[?1006l"));
        assert!(chat.contains("\x1b[?25h"));
        assert!(chat.contains("\x1b[?2004h"));
        assert!(chat.ends_with("\x1b>"));

        let terminal = String::from_utf8(filter.terminal_mode_restore_bytes()).unwrap();
        assert!(terminal.contains("\x1b[?1049h"));
        assert!(terminal.contains("\x1b[?1000h"));
        assert!(terminal.contains("\x1b[?1004h"));
        assert!(terminal.contains("\x1b[?1006h"));
        assert!(terminal.contains("\x1b[?25l"));
        assert!(terminal.ends_with("\x1b="));
    }

    #[test]
    fn stale_replay_keeps_normal_output_and_suppresses_alternate_screen_bytes() {
        let input = b"before\x1b[?1049hstale vim\x1b[?1049lafter";
        for split in 0..=input.len() {
            let mut filter = TerminalOutputFilter::new(true);
            let first = filter.filter_stale_replay(&input[..split]).bytes;
            let second = filter.filter_stale_replay(&input[split..]).bytes;
            assert_eq!([first, second].concat(), b"beforeafter", "split {split}");
            assert!(!filter.alternate_screen());
        }
    }

    #[test]
    fn enhanced_keyboard_modes_are_suspended_for_chat_and_replayed_for_terminal() {
        let mut filter = TerminalOutputFilter::new(true);
        let controls = b"\x1b[=3u\x1b[>5u\x1b[>4;2m";
        assert_eq!(filter.filter(controls).bytes, controls);

        let chat = String::from_utf8(filter.chat_mode_restore_bytes()).unwrap();
        assert!(chat.contains("\x1b[<u"));
        assert!(chat.contains("\x1b[=0u"));
        assert!(chat.contains("\x1b[>4;0m"));

        let terminal = String::from_utf8(filter.terminal_mode_restore_bytes()).unwrap();
        assert!(terminal.contains("\x1b[=3u"));
        assert!(terminal.contains("\x1b[>5u"));
        assert!(terminal.contains("\x1b[>4;2m"));

        assert_eq!(filter.filter(b"\x1b[<u").bytes, b"\x1b[<u");
        assert!(
            !String::from_utf8(filter.chat_mode_restore_bytes())
                .unwrap()
                .contains("\x1b[<u")
        );
    }

    #[test]
    fn printable_unicode_survives_every_chunk_boundary_without_c1_reinterpretation() {
        let input = "plain · שלום · мир · 界 · 👩‍💻\n".as_bytes();
        assert!(input.contains(&C1_OSC));
        for split in 0..=input.len() {
            let report = filtered_in_splits(input, split, true);
            assert_eq!(report.bytes, input, "split {split}");
            assert_eq!(report.blocked_sequences, 0, "split {split}");
            assert_eq!(report.malformed_sequences, 0, "split {split}");
        }
    }

    #[test]
    fn raw_c1_effects_are_blocked_but_utf8_continuation_bytes_are_data() {
        let raw_c1 = b"before\x9d52;c;secret\x07after";
        let report = filtered_in_splits(raw_c1, 7, true);
        assert_eq!(report.bytes, b"beforeafter");
        assert_eq!(report.blocked_sequences, 1);

        let printable = "beforeם52;c;visible\u{7}after".as_bytes();
        assert!(printable.windows(2).any(|bytes| bytes == [0xd7, C1_OSC]));
        for split in 0..=printable.len() {
            let report = filtered_in_splits(printable, split, true);
            assert_eq!(
                report.bytes,
                "beforeם52;c;visibleafter".as_bytes(),
                "split {split}"
            );
            assert_eq!(report.blocked_sequences, 1, "split {split}");
            assert_eq!(report.malformed_sequences, 0, "split {split}");
        }
    }

    #[test]
    fn utf8_continuations_cannot_terminate_a_blocked_control_string() {
        let input = "a\u{1b}]52;c;שלם;still-secret\u{7}z".as_bytes();
        assert!(input.contains(&C1_ST));
        for split in 0..=input.len() {
            let report = filtered_in_splits(input, split, true);
            assert_eq!(report.bytes, b"az", "split {split}");
            assert_eq!(report.blocked_sequences, 1, "split {split}");
            assert_eq!(report.malformed_sequences, 0, "split {split}");
        }
    }

    #[test]
    fn encoded_unicode_c1_and_malformed_utf8_fail_closed_without_hiding_later_controls() {
        let mut filter = TerminalOutputFilter::new(true);
        let report = filter.filter(b"a\xc2\x9db\xf5\x1b]0;title\x07c");
        assert_eq!(report.bytes, b"abc");
        assert_eq!(report.blocked_sequences, 2);
        assert_eq!(report.malformed_sequences, 1);

        let mut incomplete = TerminalOutputFilter::new(true);
        assert!(incomplete.filter(b"ok\xf0\x9f").bytes.ends_with(b"ok"));
        assert_eq!(incomplete.finish().malformed_sequences, 1);
    }
}
