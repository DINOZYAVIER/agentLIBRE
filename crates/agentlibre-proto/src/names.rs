//! The names of the wire's values (AGL-130, D3): small helpers of plain
//! data a surface needs to draw what the protocol says, with no
//! translation and no dependency. Each is the core's method of the same
//! name; the core keeps its own, which name the session's file.

use std::time::Duration;

use crate::{MediaKind, MemoryKind, Outcome, Phase, PhaseEvent, StopReason, ToolCall, Transition, Value};

impl Outcome {
    /// The name in transition keys such as `bash.ok`; the wire's tag.
    pub fn name(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Fail => "fail",
            Outcome::Timeout => "timeout",
            Outcome::Interrupted => "interrupted",
        }
    }
}

impl StopReason {
    /// The name in the session file; the wire's tag.
    pub fn name(self) -> &'static str {
        match self {
            StopReason::StepLimit => "step_limit",
            StopReason::ContextFull => "context_full",
            StopReason::ServerError => "server_error",
            StopReason::Interrupted => "interrupted",
            StopReason::StirLimit => "stir_limit",
        }
    }
}

impl ToolCall {
    /// The text argument `name`; `None` when the call has no such text
    /// argument.
    pub fn text(&self, name: &str) -> Option<&str> {
        self.args.iter().find_map(|(n, value)| match value {
            Value::Text(text) if n == name => Some(text.as_str()),
            _ => None,
        })
    }

    /// The integer argument `name`; `None` when the call has no such
    /// integer argument.
    pub fn int(&self, name: &str) -> Option<i64> {
        self.args.iter().find_map(|(n, value)| match value {
            Value::Int(number) if n == name => Some(*number),
            _ => None,
        })
    }
}

impl MemoryKind {
    /// The sign in the marker, `[memory: +name]`.
    pub fn sign(self) -> char {
        match self {
            MemoryKind::Added => '+',
            MemoryKind::Edited => '~',
            MemoryKind::Removed => '-',
        }
    }
}

impl Phase {
    /// The name in the session file.
    pub fn name(&self) -> &'static str {
        match self {
            Phase::Ready => "Ready",
            Phase::StartingServer => "StartingServer",
            Phase::Transcribing => "Transcribing",
            Phase::Prefill => "Prefill",
            Phase::Thinking => "Thinking",
            Phase::Writing => "Writing",
            Phase::Running(_) => "Running",
            Phase::Compacting => "Compacting",
            Phase::Retry { .. } => "Retry",
        }
    }
}

impl PhaseEvent {
    /// The name in the session file.
    pub fn name(&self) -> String {
        match self {
            PhaseEvent::ServerStart => "server_start".into(),
            PhaseEvent::Transcribe => "transcribe".into(),
            PhaseEvent::StepStart => "step_start".into(),
            PhaseEvent::FirstThinking => "first_thinking".into(),
            PhaseEvent::FirstText => "first_text".into(),
            PhaseEvent::ToolStart { .. } => "tool_start".into(),
            PhaseEvent::ToolEnd(outcome) => format!("tool_{}", outcome.name()),
            PhaseEvent::CompactStart => "compact_start".into(),
            PhaseEvent::Compacted => "compacted".into(),
            PhaseEvent::CompactFailed => "compact_failed".into(),
            PhaseEvent::CutOff => "cut_off".into(),
            PhaseEvent::Stir { .. } => "stir".into(),
            PhaseEvent::Answer => "answer".into(),
            PhaseEvent::Stop(reason) => format!("stop_{}", reason.name()),
            PhaseEvent::Failed => "failed".into(),
            PhaseEvent::Idle => "idle".into(),
        }
    }
}

impl Transition {
    /// The time spent in `from`.
    pub fn time(&self) -> Duration {
        Duration::from_micros(self.micros)
    }
}

impl MediaKind {
    /// Every kind, in the order of a session's media counts.
    pub const ALL: [MediaKind; 3] = [MediaKind::Image, MediaKind::Video, MediaKind::Audio];

    /// The word of the placeholder and of the session file.
    pub fn label(self) -> &'static str {
        match self {
            MediaKind::Image => "Image",
            MediaKind::Video => "Video",
            MediaKind::Audio => "Audio",
        }
    }

    pub fn from_label(label: &str) -> Option<MediaKind> {
        MediaKind::ALL.into_iter().find(|kind| kind.label() == label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tag serde writes for a value that is a bare string.
    fn tag<T: serde::Serialize>(value: &T) -> String {
        serde_json::to_value(value)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn an_outcome_is_named_by_its_tag() {
        for outcome in [
            Outcome::Ok,
            Outcome::Fail,
            Outcome::Timeout,
            Outcome::Interrupted,
        ] {
            assert_eq!(outcome.name(), tag(&outcome));
        }
    }

    #[test]
    fn a_stop_reason_is_named_by_its_tag() {
        for reason in [
            StopReason::StepLimit,
            StopReason::ContextFull,
            StopReason::ServerError,
            StopReason::Interrupted,
            StopReason::StirLimit,
        ] {
            assert_eq!(reason.name(), tag(&reason));
        }
    }

    #[test]
    fn a_memory_kind_has_its_sign() {
        let signs: Vec<char> = [MemoryKind::Added, MemoryKind::Edited, MemoryKind::Removed]
            .into_iter()
            .map(MemoryKind::sign)
            .collect();
        assert_eq!(signs, ['+', '~', '-']);
    }

    #[test]
    fn a_phase_is_named_as_the_session_file_names_it() {
        let names: Vec<&str> = [
            Phase::Ready,
            Phase::StartingServer,
            Phase::Transcribing,
            Phase::Prefill,
            Phase::Thinking,
            Phase::Writing,
            Phase::Running("bash".into()),
            Phase::Compacting,
            Phase::Retry { attempt: 1, max: 3 },
        ]
        .iter()
        .map(Phase::name)
        .collect();
        assert_eq!(
            names,
            [
                "Ready",
                "StartingServer",
                "Transcribing",
                "Prefill",
                "Thinking",
                "Writing",
                "Running",
                "Compacting",
                "Retry"
            ]
        );
    }

    #[test]
    fn a_phase_event_is_named_as_the_session_file_names_it() {
        // A unit variant is named by its tag; the others by the tag of
        // their variant, or by it and what it holds.
        for event in [
            PhaseEvent::ServerStart,
            PhaseEvent::Transcribe,
            PhaseEvent::StepStart,
            PhaseEvent::FirstThinking,
            PhaseEvent::FirstText,
            PhaseEvent::CompactStart,
            PhaseEvent::Compacted,
            PhaseEvent::CompactFailed,
            PhaseEvent::CutOff,
            PhaseEvent::Answer,
            PhaseEvent::Failed,
            PhaseEvent::Idle,
        ] {
            assert_eq!(event.name(), tag(&event));
        }
        let key = |event: &PhaseEvent| {
            serde_json::to_value(event)
                .unwrap()
                .as_object()
                .unwrap()
                .keys()
                .next()
                .unwrap()
                .clone()
        };
        let start = PhaseEvent::ToolStart { tool: "bash".into() };
        assert_eq!(start.name(), key(&start));
        let stir = PhaseEvent::Stir { attempt: 1, max: 3 };
        assert_eq!(stir.name(), key(&stir));
        assert_eq!(PhaseEvent::ToolEnd(Outcome::Ok).name(), "tool_ok");
        assert_eq!(PhaseEvent::ToolEnd(Outcome::Interrupted).name(), "tool_interrupted");
        assert_eq!(PhaseEvent::Stop(StopReason::StepLimit).name(), "stop_step_limit");
    }

    #[test]
    fn a_transition_has_its_time_from_its_micros() {
        let transition = Transition {
            from: Phase::Ready,
            to: Phase::Prefill,
            event: PhaseEvent::StepStart,
            micros: 1_500_250,
            violation: false,
        };
        assert_eq!(transition.time(), Duration::new(1, 500_250_000));
    }

    #[test]
    fn a_call_gives_its_text_and_integer_arguments() {
        let call = ToolCall {
            id: "1".into(),
            name: "read".into(),
            args: vec![
                ("path".into(), Value::Text("a".into())),
                ("line".into(), Value::Int(7)),
            ],
        };
        assert_eq!(call.text("path"), Some("a"));
        assert_eq!(call.text("line"), None);
        assert_eq!(call.int("line"), Some(7));
        assert_eq!(call.int("path"), None);
        assert_eq!(call.text("none"), None);
    }

    #[test]
    fn a_media_kind_has_its_label_and_is_found_by_it() {
        let labels: Vec<&str> = MediaKind::ALL.into_iter().map(MediaKind::label).collect();
        assert_eq!(labels, ["Image", "Video", "Audio"]);
        for kind in MediaKind::ALL {
            assert_eq!(MediaKind::from_label(kind.label()), Some(kind));
        }
        assert_eq!(MediaKind::from_label("image"), None);
    }
}
