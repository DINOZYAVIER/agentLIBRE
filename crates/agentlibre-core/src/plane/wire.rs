//! A plane's reports, events and ends on the wire (AGL-076, D2): the core's
//! types mapped to the protocol's, which has no dependency on the core. Back
//! come only what a client's command carries (AGL-130, D11).

use agentlibre_proto as wire;

use super::{Done, Text, Sent, SessionView, Told};
use crate::action::{Outcome, Value};
use crate::agent::{Report, TurnEnd};
use crate::backend::ToolCall;
use agentlibre_common::i18n::Lang;
use crate::function::catalog::{Entry, Source as FunctionSource};
use agentlibre_common::media::{Kind, Pending, Source};
use agentlibre_forge::memory::Kind as MemoryKind;
use crate::phase::{Phase, PhaseEvent};
use crate::session::{CommandRun, Event, ForgeKind, SessionInfo, StopReason, TransitionLine};

pub fn sent(sent: Sent) -> Option<wire::Sent> {
    Some(match sent {
        Sent::Report(report) => wire::Sent::Report(self::report(report)),
        Sent::Line(line) => wire::Sent::Line(line),
        Sent::Context(context) => wire::Sent::Context(context),
        Sent::Stored(media) => wire::Sent::Stored(media),
        Sent::Over => wire::Sent::Over,
        Sent::Kept(_) | Sent::Done(_) => return None,
    })
}

/// A command's end; its errors in `lang`.
pub fn done(done: &Done, plane: Option<&str>, lang: Lang) -> wire::Done {
    wire::Done {
        turn: done.turn.as_ref().map(|turn| match turn {
            Ok(end) => Ok(turn_end(end.clone())),
            Err(error) => Err(error.message(lang)),
        }),
        why: done.why.clone(),
        said: done.said.as_ref().map(|said| match said {
            Ok(said) => Ok(self::said(said, lang)),
            Err(error) => Err(error.message(lang)),
        }),
        session: view(&done.session, plane),
    }
}

/// A Function of the list (AGL-126).
pub fn function_entry(entry: Entry) -> wire::FunctionEntry {
    wire::FunctionEntry {
        name: entry.name,
        source: function_source(entry.source),
        shadows: entry.shadows.into_iter().map(function_source).collect(),
        kind: entry.kind.name().to_owned(),
        model: entry.model,
        description: entry.description,
        error: entry.error,
        current: entry.current,
    }
}

fn function_source(source: FunctionSource) -> wire::FunctionSource {
    match source {
        FunctionSource::Plane => wire::FunctionSource::Plane,
        FunctionSource::Workspace => wire::FunctionSource::Workspace,
        FunctionSource::User => wire::FunctionSource::User,
        FunctionSource::BuiltIn => wire::FunctionSource::BuiltIn,
    }
}

/// A phase of a step of the past (AGL-126).
pub fn phase_time(line: TransitionLine) -> wire::PhaseTime {
    wire::PhaseTime {
        turn: line.turn,
        step: line.step,
        from: line.from,
        to: line.to,
        event: line.event,
        ms: line.ms,
        tokens: line.tokens,
        tool: line.tool,
    }
}

/// A past session (AGL-126, D9).
pub fn past(info: SessionInfo) -> wire::PastSession {
    wire::PastSession {
        id: info.id,
        time: info.time,
        name: info.name,
        first: info.first,
        workspace: info.workspace,
        plane: info.plane,
    }
}

pub fn view(view: &SessionView, plane: Option<&str>) -> wire::SessionView {
    wire::SessionView {
        id: view.id.clone(),
        name: view.name.clone(),
        plane: plane.map(str::to_owned),
        outputs: view.outputs.to_string_lossy().into_owned(),
        unfinished: view.unfinished.clone(),
        media: view.media,
    }
}

pub fn turn_end(end: TurnEnd) -> wire::TurnEnd {
    match end {
        TurnEnd::Answer(text) => wire::TurnEnd::Answer(text),
        TurnEnd::Stopped(reason) => wire::TurnEnd::Stopped(stop_reason(reason)),
    }
}

pub fn said(said: &Text, lang: Lang) -> wire::Text {
    wire::Text {
        lines: said
            .lines
            .iter()
            .map(|told| match told.clone() {
                Told::Output(text) => wire::Told::Output(text),
                Told::Notice(text) => wire::Told::Notice(text),
                Told::Refusal(text) => wire::Told::Refusal(text),
                Told::Fault(text) => wire::Told::Fault(text),
                Told::Pending(text) => wire::Told::Pending(text),
            })
            .collect(),
        memory: said.memory.as_ref().map(|write| wire::MemoryWrite {
            name: write.name.clone(),
            kind: memory_kind(write.kind),
            text: write.text.clone(),
        }),
        content: said.content.clone(),
        failed: said.failed,
        interrupted: said.interrupted,
        record_error: said.record_error.as_ref().map(|error| error.message(lang)),
    }
}

pub fn report(report: Report) -> wire::Report {
    match report {
        Report::Event(event) => wire::Report::Event(self::event(event)),
        Report::Prefill => wire::Report::Prefill,
        Report::Thinking(text) => wire::Report::Thinking(text),
        Report::ThinkingDone => wire::Report::ThinkingDone,
        Report::ActionStarted => wire::Report::ActionStarted,
        Report::ActionText(text) => wire::Report::ActionText(text),
        Report::Stirred { attempt, max, error } => wire::Report::Stirred { attempt, max, error },
        Report::Compacting => wire::Report::Compacting,
        Report::Compacted => wire::Report::Compacted,
        Report::CompactFailed => wire::Report::CompactFailed,
        Report::ContextUsed(n) => wire::Report::ContextUsed(n),
        Report::Decoded(n) => wire::Report::Decoded(n),
        Report::PromptProgress(p) => wire::Report::PromptProgress(wire::PromptProgress {
            total: p.total,
            cache: p.cache,
            processed: p.processed,
            time_ms: p.time_ms,
        }),
        Report::PromptRead(p) => wire::Report::PromptRead(wire::Prefill {
            tokens: p.tokens,
            ms: p.ms,
        }),
        Report::StepStarted(s) => wire::Report::StepStarted(wire::StepRecord {
            state: s.state,
            prompt_sha256: s.prompt_sha256,
            prompt_tokens: s.prompt_tokens,
            think: s.think,
            think_budget: s.think_budget,
            action_budget: s.action_budget,
            context: s.context,
            stirred: s.stirred,
            reasoning: s.reasoning,
        }),
        Report::Transition(t) => wire::Report::Transition(wire::Transition {
            from: phase(t.from),
            to: phase(t.to),
            event: phase_event(t.event),
            micros: t.time.as_micros() as u64,
            violation: t.violation,
        }),
    }
}

pub fn event(event: Event) -> wire::Event {
    match event {
        Event::User(text) => wire::Event::User(text),
        Event::Wake { flow, text } => wire::Event::Wake { flow, text },
        Event::State(name) => wire::Event::State(name),
        Event::Thinking(text) => wire::Event::Thinking(text),
        Event::Call { text, call } => {
            // AGL-121, D52: the summary rides the wire; the session's
            // file does not keep it.
            let summary = crate::tools::spec::call_summary(&call);
            wire::Event::Call {
                text,
                summary: Some(summary),
                call: tool_call(call),
            }
        }
        Event::Result { id, outcome, content } => wire::Event::Result {
            id,
            outcome: self::outcome(outcome),
            content,
        },
        Event::Forge { name, kind, section } => wire::Event::Forge {
            name,
            kind: forge_kind(kind),
            section,
        },
        Event::Answer(text) => wire::Event::Answer(text),
        Event::CutOff(cut) => wire::Event::CutOff(wire::CutOff {
            limit: cut.limit,
            thinking: cut.thinking,
            discarded: cut.discarded,
            call: cut.call,
        }),
        Event::Stir { attempt, error } => wire::Event::Stir { attempt, error },
        Event::Stop(reason) => wire::Event::Stop(stop_reason(reason)),
        Event::Memory { name, kind, text } => wire::Event::Memory {
            name,
            kind: memory_kind(kind),
            text,
        },
        Event::Media(media) => wire::Event::Media(wire::Media {
            kind: media_kind(media.kind),
            number: media.number,
            sha256: media.sha256,
            ext: media.ext,
            name: media.name,
            transcript: media.transcript,
            frames: media
                .frames
                .into_iter()
                .map(|frame| wire::Frame {
                    at: frame.at,
                    sha256: frame.sha256,
                })
                .collect(),
        }),
        Event::Compact { summary, question } => wire::Event::Compact { summary, question },
        Event::Command(run) => wire::Event::Command(command(run)),
        Event::System { text, failed } => wire::Event::System { text, failed },
    }
}

pub fn command(run: CommandRun) -> wire::CommandLine {
    wire::CommandLine {
        line: run.line,
        failed: run.failed,
        output: run.output,
    }
}

pub fn command_of(line: wire::CommandLine) -> CommandRun {
    CommandRun {
        line: line.line,
        failed: line.failed,
        output: line.output,
    }
}

/// The medium back; `None` when its base64 is not.
pub fn attached_of(attached: wire::Attached) -> Option<Pending> {
    Some(Pending {
        number: attached.number,
        source: Source {
            kind: media_kind_of(attached.kind),
            bytes: wire::base64::decode(&attached.base64)?,
            ext: attached.ext,
            name: attached.name,
        },
    })
}

/// A call on the wire.
pub fn tool_call(call: ToolCall) -> wire::ToolCall {
    wire::ToolCall {
        id: call.id,
        name: call.name,
        args: call
            .args
            .into_iter()
            .map(|(name, value)| (name, self::value(value)))
            .collect(),
    }
}

fn value(value: Value) -> wire::Value {
    match value {
        Value::Text(text) => wire::Value::Text(text),
        Value::Int(n) => wire::Value::Int(n),
    }
}

pub fn outcome(outcome: Outcome) -> wire::Outcome {
    match outcome {
        Outcome::Ok => wire::Outcome::Ok,
        Outcome::Fail => wire::Outcome::Fail,
        Outcome::Timeout => wire::Outcome::Timeout,
        Outcome::Interrupted => wire::Outcome::Interrupted,
    }
}

pub fn stop_reason(reason: StopReason) -> wire::StopReason {
    match reason {
        StopReason::StepLimit => wire::StopReason::StepLimit,
        StopReason::ContextFull => wire::StopReason::ContextFull,
        StopReason::ServerError => wire::StopReason::ServerError,
        StopReason::Interrupted => wire::StopReason::Interrupted,
        StopReason::StirLimit => wire::StopReason::StirLimit,
    }
}

fn forge_kind(kind: ForgeKind) -> wire::ForgeKind {
    match kind {
        ForgeKind::Created => wire::ForgeKind::Created,
        ForgeKind::Edited => wire::ForgeKind::Edited,
        ForgeKind::Appended => wire::ForgeKind::Appended,
        ForgeKind::Removed => wire::ForgeKind::Removed,
        ForgeKind::StatusChanged => wire::ForgeKind::StatusChanged,
        ForgeKind::Renamed => wire::ForgeKind::Renamed,
    }
}

fn memory_kind(kind: MemoryKind) -> wire::MemoryKind {
    match kind {
        MemoryKind::Added => wire::MemoryKind::Added,
        MemoryKind::Edited => wire::MemoryKind::Edited,
        MemoryKind::Removed => wire::MemoryKind::Removed,
    }
}

pub fn media_kind(kind: Kind) -> wire::MediaKind {
    match kind {
        Kind::Image => wire::MediaKind::Image,
        Kind::Video => wire::MediaKind::Video,
        Kind::Audio => wire::MediaKind::Audio,
    }
}

pub fn media_kind_of(kind: wire::MediaKind) -> Kind {
    match kind {
        wire::MediaKind::Image => Kind::Image,
        wire::MediaKind::Video => Kind::Video,
        wire::MediaKind::Audio => Kind::Audio,
    }
}

fn phase(phase: Phase) -> wire::Phase {
    match phase {
        Phase::Ready => wire::Phase::Ready,
        Phase::StartingServer => wire::Phase::StartingServer,
        Phase::Transcribing => wire::Phase::Transcribing,
        Phase::Prefill => wire::Phase::Prefill,
        Phase::Thinking => wire::Phase::Thinking,
        Phase::Writing => wire::Phase::Writing,
        Phase::Running(tool) => wire::Phase::Running(tool),
        Phase::Compacting => wire::Phase::Compacting,
        Phase::Retry { attempt, max } => wire::Phase::Retry { attempt, max },
    }
}

fn phase_event(event: PhaseEvent) -> wire::PhaseEvent {
    match event {
        PhaseEvent::ServerStart => wire::PhaseEvent::ServerStart,
        PhaseEvent::Transcribe => wire::PhaseEvent::Transcribe,
        PhaseEvent::StepStart => wire::PhaseEvent::StepStart,
        PhaseEvent::FirstThinking => wire::PhaseEvent::FirstThinking,
        PhaseEvent::FirstText => wire::PhaseEvent::FirstText,
        PhaseEvent::ToolStart { tool } => wire::PhaseEvent::ToolStart { tool },
        PhaseEvent::ToolEnd(outcome) => wire::PhaseEvent::ToolEnd(self::outcome(outcome)),
        PhaseEvent::CompactStart => wire::PhaseEvent::CompactStart,
        PhaseEvent::Compacted => wire::PhaseEvent::Compacted,
        PhaseEvent::CompactFailed => wire::PhaseEvent::CompactFailed,
        PhaseEvent::CutOff => wire::PhaseEvent::CutOff,
        PhaseEvent::Stir { attempt, max } => wire::PhaseEvent::Stir { attempt, max },
        PhaseEvent::Answer => wire::PhaseEvent::Answer,
        PhaseEvent::Stop(reason) => wire::PhaseEvent::Stop(stop_reason(reason)),
        PhaseEvent::Failed => wire::PhaseEvent::Failed,
        PhaseEvent::Idle => wire::PhaseEvent::Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_goes_to_the_wire_as_it_was() {
        // The mapping back is gone (AGL-130, D11): what the run reports is
        // what the wire says, value for value.
        let bash = ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: vec![("command".into(), Value::Text("ls".into())), ("timeout".into(), Value::Int(9))],
        };
        let reports = [
            (
                Report::Event(Event::Call {
                    text: Some("look".into()),
                    call: bash,
                }),
                wire::Report::Event(wire::Event::Call {
                    text: Some("look".into()),
                    call: wire::ToolCall {
                        id: "c1".into(),
                        name: "bash".into(),
                        args: vec![
                            ("command".into(), wire::Value::Text("ls".into())),
                            ("timeout".into(), wire::Value::Int(9)),
                        ],
                    },
                    summary: Some("ls".into()),
                }),
            ),
            (
                Report::Event(Event::Result {
                    id: "c1".into(),
                    outcome: Outcome::Timeout,
                    content: "…".into(),
                }),
                wire::Report::Event(wire::Event::Result {
                    id: "c1".into(),
                    outcome: wire::Outcome::Timeout,
                    content: "…".into(),
                }),
            ),
            (
                Report::Event(Event::Stop(StopReason::StirLimit)),
                wire::Report::Event(wire::Event::Stop(wire::StopReason::StirLimit)),
            ),
            (
                Report::Event(Event::Command(CommandRun {
                    line: "/status".into(),
                    failed: false,
                    output: None,
                })),
                wire::Report::Event(wire::Event::Command(wire::CommandLine {
                    line: "/status".into(),
                    failed: false,
                    output: None,
                })),
            ),
            (
                Report::Stirred {
                    attempt: 1,
                    max: 3,
                    error: "bad".into(),
                },
                wire::Report::Stirred {
                    attempt: 1,
                    max: 3,
                    error: "bad".into(),
                },
            ),
            (
                Report::Transition(crate::phase::Transition {
                    from: Phase::Running("bash".into()),
                    to: Phase::Retry { attempt: 2, max: 3 },
                    event: PhaseEvent::ToolEnd(Outcome::Fail),
                    time: std::time::Duration::from_micros(1234),
                    violation: true,
                }),
                wire::Report::Transition(wire::Transition {
                    from: wire::Phase::Running("bash".into()),
                    to: wire::Phase::Retry { attempt: 2, max: 3 },
                    event: wire::PhaseEvent::ToolEnd(wire::Outcome::Fail),
                    micros: 1234,
                    violation: true,
                }),
            ),
            (
                Report::PromptRead(crate::backend::Prefill { tokens: 10, ms: 2.5 }),
                wire::Report::PromptRead(wire::Prefill { tokens: 10, ms: 2.5 }),
            ),
        ];
        for (report, on_the_wire) in reports {
            assert_eq!(self::report(report), on_the_wire);
        }
    }

    #[test]
    fn a_phase_event_has_the_same_name_on_the_wire() {
        // `PhaseEvent::name` is the core's and the protocol's (AGL-130, D3).
        let events = [
            PhaseEvent::ServerStart,
            PhaseEvent::Transcribe,
            PhaseEvent::StepStart,
            PhaseEvent::FirstThinking,
            PhaseEvent::FirstText,
            PhaseEvent::ToolStart { tool: "bash".into() },
            PhaseEvent::ToolEnd(Outcome::Ok),
            PhaseEvent::ToolEnd(Outcome::Interrupted),
            PhaseEvent::CompactStart,
            PhaseEvent::Compacted,
            PhaseEvent::CompactFailed,
            PhaseEvent::CutOff,
            PhaseEvent::Stir { attempt: 1, max: 2 },
            PhaseEvent::Answer,
            PhaseEvent::Stop(StopReason::StepLimit),
            PhaseEvent::Stop(StopReason::StirLimit),
            PhaseEvent::Failed,
            PhaseEvent::Idle,
        ];
        for event in events {
            assert_eq!(phase_event(event.clone()).name(), event.name());
        }
    }

    #[test]
    fn the_call_on_the_wire_carries_the_summary_of_the_table() {
        // AGL-121, D52: the daemon computes it, the wire carries it.
        let event = Event::Call {
            text: None,
            call: ToolCall {
                id: "c1".into(),
                name: "forge".into(),
                args: vec![
                    ("action".into(), Value::Text("rename".into())),
                    ("name".into(), Value::Text("agl-066".into())),
                    ("new".into(), Value::Text("agl-067".into())),
                ],
            },
        };
        let wire::Event::Call { summary, .. } = self::event(event) else {
            panic!("not a call");
        };
        assert_eq!(summary.as_deref(), Some("rename agl-066 → agl-067"));
    }

    #[test]
    fn a_medium_comes_from_base64() {
        // The twin of `attached` is the surface's (AGL-127, D2): its test
        // holds that the same bytes go out as base64.
        let pending = Pending {
            number: 2,
            source: Source {
                kind: Kind::Audio,
                bytes: vec![0, 1, 2, 255],
                ext: "wav".into(),
                name: "said.wav".into(),
            },
        };
        let attached = wire::Attached {
            number: 2,
            kind: wire::MediaKind::Audio,
            name: "said.wav".into(),
            ext: "wav".into(),
            base64: "AAEC/w==".into(),
        };
        assert_eq!(attached_of(attached), Some(pending));
        let bad = wire::Attached {
            number: 1,
            kind: wire::MediaKind::Image,
            name: "x.png".into(),
            ext: "png".into(),
            base64: "not base64!".into(),
        };
        assert_eq!(attached_of(bad), None);
    }
}
