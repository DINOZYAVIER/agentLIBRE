use std::time::Duration;

pub(crate) const DEFAULT_ESCAPE_BANG_WINDOW: Duration = Duration::from_millis(750);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PhysicalInputKind {
    Bang,
    Enter,
    Escape,
    Other,
    Paste,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PhysicalInput {
    pub kind: PhysicalInputKind,
    pub bytes: Vec<u8>,
}

impl PhysicalInput {
    pub fn bang() -> Self {
        Self {
            kind: PhysicalInputKind::Bang,
            bytes: vec![b'!'],
        }
    }

    pub fn enter(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            kind: PhysicalInputKind::Enter,
            bytes: bytes.into(),
        }
    }

    pub fn escape() -> Self {
        Self {
            kind: PhysicalInputKind::Escape,
            bytes: vec![0x1b],
        }
    }

    pub fn other(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            kind: PhysicalInputKind::Other,
            bytes: bytes.into(),
        }
    }

    pub fn paste(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            kind: PhysicalInputKind::Paste,
            bytes: bytes.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TerminalInputAction {
    Forward(Vec<u8>),
    SwitchToChat,
}

const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Clone, Debug)]
pub(crate) struct TerminalInputGate {
    prompt_gate_armed: bool,
    held_prompt_bang: bool,
    integration_degraded: bool,
    escape_bang_deadline: Option<Duration>,
    escape_bang_window: Duration,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RawTerminalInputGate {
    gate: TerminalInputGate,
    in_bracketed_paste: bool,
    paste_start_match: usize,
    paste_end_match: usize,
}

impl RawTerminalInputGate {
    pub fn prompt_ready(&mut self) {
        self.gate.prompt_ready();
    }

    pub fn prompt_busy(&mut self) -> Vec<TerminalInputAction> {
        self.gate.prompt_busy()
    }

    pub fn integration_degraded(&mut self) -> Vec<TerminalInputAction> {
        self.gate.integration_degraded()
    }

    pub fn handle_bytes(&mut self, bytes: &[u8], now: Duration) -> Vec<TerminalInputAction> {
        let mut actions = Vec::new();
        for &byte in bytes {
            let input = if self.in_bracketed_paste {
                PhysicalInput::paste(vec![byte])
            } else {
                match byte {
                    b'!' => PhysicalInput::bang(),
                    b'\r' | b'\n' => PhysicalInput::enter(vec![byte]),
                    0x1b => PhysicalInput::escape(),
                    _ => PhysicalInput::other(vec![byte]),
                }
            };
            for action in self.gate.handle(input, now) {
                push_coalesced(&mut actions, action);
            }

            if self.in_bracketed_paste {
                self.paste_end_match =
                    advance_match(self.paste_end_match, BRACKETED_PASTE_END, byte);
                if self.paste_end_match == BRACKETED_PASTE_END.len() {
                    self.in_bracketed_paste = false;
                    self.paste_end_match = 0;
                }
            } else {
                self.paste_start_match =
                    advance_match(self.paste_start_match, BRACKETED_PASTE_START, byte);
                if self.paste_start_match == BRACKETED_PASTE_START.len() {
                    self.in_bracketed_paste = true;
                    self.paste_start_match = 0;
                }
            }
        }
        actions
    }
}

fn advance_match(current: usize, pattern: &[u8], byte: u8) -> usize {
    if pattern.get(current) == Some(&byte) {
        current + 1
    } else if pattern.first() == Some(&byte) {
        1
    } else {
        0
    }
}

fn push_coalesced(actions: &mut Vec<TerminalInputAction>, action: TerminalInputAction) {
    match action {
        TerminalInputAction::Forward(bytes) => {
            if let Some(TerminalInputAction::Forward(previous)) = actions.last_mut() {
                previous.extend_from_slice(&bytes);
            } else {
                actions.push(TerminalInputAction::Forward(bytes));
            }
        }
        TerminalInputAction::SwitchToChat => actions.push(TerminalInputAction::SwitchToChat),
    }
}

impl Default for TerminalInputGate {
    fn default() -> Self {
        Self::new(DEFAULT_ESCAPE_BANG_WINDOW)
    }
}

impl TerminalInputGate {
    pub fn new(escape_bang_window: Duration) -> Self {
        Self {
            prompt_gate_armed: false,
            held_prompt_bang: false,
            integration_degraded: false,
            escape_bang_deadline: None,
            escape_bang_window,
        }
    }

    pub fn prompt_ready(&mut self) {
        if !self.integration_degraded {
            self.prompt_gate_armed = true;
        }
        self.escape_bang_deadline = None;
    }

    pub fn prompt_busy(&mut self) -> Vec<TerminalInputAction> {
        self.prompt_gate_armed = false;
        self.escape_bang_deadline = None;
        self.flush_held_bang()
    }

    pub fn integration_degraded(&mut self) -> Vec<TerminalInputAction> {
        self.integration_degraded = true;
        self.prompt_gate_armed = false;
        self.escape_bang_deadline = None;
        self.flush_held_bang()
    }

    pub fn handle(&mut self, input: PhysicalInput, now: Duration) -> Vec<TerminalInputAction> {
        let mut actions = Vec::with_capacity(2);
        if self
            .escape_bang_deadline
            .is_some_and(|deadline| now > deadline)
        {
            self.escape_bang_deadline = None;
        }

        if input.kind == PhysicalInputKind::Paste {
            actions.extend(self.flush_held_bang());
            self.prompt_gate_armed = false;
            self.escape_bang_deadline = None;
            if !input.bytes.is_empty() {
                actions.push(TerminalInputAction::Forward(input.bytes));
            }
            return actions;
        }

        if self.held_prompt_bang {
            self.held_prompt_bang = false;
            self.prompt_gate_armed = false;
            if input.kind == PhysicalInputKind::Enter {
                actions.push(TerminalInputAction::SwitchToChat);
                return actions;
            }
            actions.push(TerminalInputAction::Forward(vec![b'!']));
        } else if self.prompt_gate_armed {
            if input.kind == PhysicalInputKind::Bang {
                self.held_prompt_bang = true;
                return actions;
            }
            self.prompt_gate_armed = false;
        }

        if self.escape_bang_deadline.take().is_some() && input.kind == PhysicalInputKind::Bang {
            actions.push(TerminalInputAction::SwitchToChat);
            return actions;
        }

        if input.kind == PhysicalInputKind::Escape {
            self.escape_bang_deadline = Some(now.saturating_add(self.escape_bang_window));
        }
        if !input.bytes.is_empty() {
            actions.push(TerminalInputAction::Forward(input.bytes));
        }
        actions
    }

    fn flush_held_bang(&mut self) -> Vec<TerminalInputAction> {
        if self.held_prompt_bang {
            self.held_prompt_bang = false;
            vec![TerminalInputAction::Forward(vec![b'!'])]
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(milliseconds: u64) -> Duration {
        Duration::from_millis(milliseconds)
    }

    #[test]
    fn exact_prompt_bang_enter_switches_without_shell_bytes() {
        let mut gate = TerminalInputGate::default();
        gate.prompt_ready();
        assert!(gate.handle(PhysicalInput::bang(), at(0)).is_empty());
        assert_eq!(
            gate.handle(PhysicalInput::enter(b"\r".to_vec()), at(1)),
            vec![TerminalInputAction::SwitchToChat]
        );
    }

    #[test]
    fn repeated_prompt_ready_does_not_drop_a_held_bang() {
        let mut gate = TerminalInputGate::default();
        gate.prompt_ready();
        assert!(gate.handle(PhysicalInput::bang(), at(0)).is_empty());

        gate.prompt_ready();
        assert_eq!(
            gate.handle(PhysicalInput::enter(b"\r".to_vec()), at(1)),
            vec![TerminalInputAction::SwitchToChat]
        );
    }

    #[test]
    fn prompt_bang_followed_by_another_key_flushes_exact_bytes() {
        let mut gate = TerminalInputGate::default();
        gate.prompt_ready();
        assert!(gate.handle(PhysicalInput::bang(), at(0)).is_empty());
        assert_eq!(
            gate.handle(PhysicalInput::other(b"l".to_vec()), at(1)),
            vec![
                TerminalInputAction::Forward(vec![b'!']),
                TerminalInputAction::Forward(vec![b'l']),
            ]
        );
    }

    #[test]
    fn escape_is_forwarded_and_following_bang_switches() {
        let mut gate = TerminalInputGate::default();
        assert_eq!(
            gate.handle(PhysicalInput::escape(), at(10)),
            vec![TerminalInputAction::Forward(vec![0x1b])]
        );
        assert_eq!(
            gate.handle(PhysicalInput::bang(), at(759)),
            vec![TerminalInputAction::SwitchToChat]
        );
    }

    #[test]
    fn escape_bang_window_is_inclusive_at_exactly_750_milliseconds() {
        let mut within = TerminalInputGate::default();
        assert_eq!(
            within.handle(PhysicalInput::escape(), at(10)),
            vec![TerminalInputAction::Forward(vec![0x1b])]
        );
        assert_eq!(
            within.handle(PhysicalInput::bang(), at(760)),
            vec![TerminalInputAction::SwitchToChat]
        );

        let mut expired = TerminalInputGate::default();
        let _ = expired.handle(PhysicalInput::escape(), at(10));
        assert_eq!(
            expired.handle(PhysicalInput::bang(), at(761)),
            vec![TerminalInputAction::Forward(vec![b'!'])]
        );
    }

    #[test]
    fn expired_escape_window_leaves_bang_literal() {
        let mut gate = TerminalInputGate::default();
        let _ = gate.handle(PhysicalInput::escape(), at(10));
        assert_eq!(
            gate.handle(PhysicalInput::bang(), at(761)),
            vec![TerminalInputAction::Forward(vec![b'!'])]
        );
    }

    #[test]
    fn paste_bypasses_both_switch_gates() {
        let mut gate = TerminalInputGate::default();
        gate.prompt_ready();
        assert_eq!(
            gate.handle(PhysicalInput::paste(b"!".to_vec()), at(0)),
            vec![TerminalInputAction::Forward(vec![b'!'])]
        );
        let _ = gate.handle(PhysicalInput::escape(), at(10));
        assert_eq!(
            gate.handle(PhysicalInput::paste(b"!".to_vec()), at(11)),
            vec![TerminalInputAction::Forward(vec![b'!'])]
        );
    }

    #[test]
    fn degraded_integration_disables_prompt_sensitive_switch() {
        let mut gate = TerminalInputGate::default();
        gate.prompt_ready();
        assert!(gate.handle(PhysicalInput::bang(), at(0)).is_empty());
        assert_eq!(
            gate.integration_degraded(),
            vec![TerminalInputAction::Forward(vec![b'!'])]
        );
        gate.prompt_ready();
        assert_eq!(
            gate.handle(PhysicalInput::bang(), at(1)),
            vec![TerminalInputAction::Forward(vec![b'!'])]
        );
    }

    #[test]
    fn raw_gate_preserves_bracketed_paste_and_never_toggles_inside_it() {
        let mut gate = RawTerminalInputGate::default();
        gate.prompt_ready();
        let bytes = b"\x1b[200~!\x1b!\x1b[201~";
        assert_eq!(
            gate.handle_bytes(bytes, at(0)),
            vec![TerminalInputAction::Forward(bytes.to_vec())]
        );
    }

    #[test]
    fn raw_gate_preserves_bracketed_paste_across_every_read_boundary() {
        let bytes = b"\x1b[200~!\r\x1b!\n\x1b[201~";
        for split in 0..=bytes.len() {
            let mut gate = RawTerminalInputGate::default();
            gate.prompt_ready();
            let actions = [
                gate.handle_bytes(&bytes[..split], at(0)),
                gate.handle_bytes(&bytes[split..], at(1)),
            ]
            .concat();
            let mut forwarded = Vec::new();
            for action in actions {
                match action {
                    TerminalInputAction::Forward(chunk) => forwarded.extend_from_slice(&chunk),
                    TerminalInputAction::SwitchToChat => {
                        panic!("paste switched to Chat at split {split}")
                    }
                }
            }
            assert_eq!(forwarded, bytes, "split {split}");
        }
    }

    #[test]
    fn raw_gate_handles_escape_bang_across_read_chunks() {
        let mut gate = RawTerminalInputGate::default();
        assert_eq!(
            gate.handle_bytes(b"\x1b", at(10)),
            vec![TerminalInputAction::Forward(vec![0x1b])]
        );
        assert_eq!(
            gate.handle_bytes(b"!", at(11)),
            vec![TerminalInputAction::SwitchToChat]
        );
    }
}
