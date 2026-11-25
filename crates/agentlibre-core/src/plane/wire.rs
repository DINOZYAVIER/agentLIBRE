//! A plane's reports, events and ends on the wire and back (AGL-076, D2):
//! the core's types mapped to the protocol's, which has no dependency on
//! the core, and the protocol's back to the core's for a surface that
//! draws them with the core.

use std::path::PathBuf;
use std::time::Duration;

use agentlibre_proto as wire;

use super::{Done, Text, Sent, SessionView, Told};
use crate::action::{Outcome, Value};
use crate::agent::{Report, TurnEnd};
use crate::attach::Pending;
use crate::backend::{Prefill, PromptProgress, ToolCall};
use agentlibre_common::i18n::Lang;
use crate::function::catalog::{Entry, Source as FunctionSource};
use crate::media::{Frame, Kind, Media, Source};
use agentlibre_forge::memory::{Kind as MemoryKind, Write};
use crate::phase::{Phase, PhaseEvent, StepRecord, Transition};
use crate::session::{CommandRun, CutOff, Event, ForgeKind, SessionInfo, StopReason};
use agentlibre_common::settings::ServerKind;

pub fn sent(sent: Sent) -> Option<wire::Sent> {
    Some(match sent {
        Sent::Report(report) => wire::Sent::Report(self::report(report)),
        Sent::Line(line) => wire::Sent::Line(line),
        Sent::Context(context) => wire::Sent::Context(context),
        Sent::Stored(media) => wire::Sent::Stored(media),
        Sent::Over => wire::Sent::Over,
        Sent::Done(_) => return None,
    })
}

pub fn sent_of(sent: wire::Sent) -> Sent {
    match sent {
        wire::Sent::Report(report) => Sent::Report(report_of(report)),
        wire::Sent::Line(line) => Sent::Line(line),
        wire::Sent::Context(context) => Sent::Context(context),
        wire::Sent::Stored(media) => Sent::Stored(media),
        wire::Sent::Over => Sent::Over,
    }
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

/// None for a kind this agl does not know.
pub fn function_entry_of(entry: wire::FunctionEntry) -> Option<Entry> {
    Some(Entry {
        kind: ServerKind::parse(&entry.kind)?,
        name: entry.name,
        source: function_source_of(entry.source),
        shadows: entry.shadows.into_iter().map(function_source_of).collect(),
        model: entry.model,
        description: entry.description,
        error: entry.error,
        current: entry.current,
    })
}

fn function_source(source: FunctionSource) -> wire::FunctionSource {
    match source {
        FunctionSource::Plane => wire::FunctionSource::Plane,
        FunctionSource::Workspace => wire::FunctionSource::Workspace,
        FunctionSource::User => wire::FunctionSource::User,
        FunctionSource::BuiltIn => wire::FunctionSource::BuiltIn,
    }
}

fn function_source_of(source: wire::FunctionSource) -> FunctionSource {
    match source {
        wire::FunctionSource::Plane => FunctionSource::Plane,
        wire::FunctionSource::Workspace => FunctionSource::Workspace,
        wire::FunctionSource::User => FunctionSource::User,
        wire::FunctionSource::BuiltIn => FunctionSource::BuiltIn,
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

pub fn view_of(view: wire::SessionView) -> SessionView {
    SessionView {
        id: view.id,
        name: view.name,
        events: Vec::new(),
        outputs: PathBuf::from(view.outputs),
        unfinished: view.unfinished,
        media: view.media,
    }
}

pub fn turn_end(end: TurnEnd) -> wire::TurnEnd {
    match end {
        TurnEnd::Answer(text) => wire::TurnEnd::Answer(text),
        TurnEnd::Stopped(reason) => wire::TurnEnd::Stopped(stop_reason(reason)),
    }
}

pub fn turn_end_of(end: wire::TurnEnd) -> TurnEnd {
    match end {
        wire::TurnEnd::Answer(text) => TurnEnd::Answer(text),
        wire::TurnEnd::Stopped(reason) => TurnEnd::Stopped(stop_reason_of(reason)),
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

/// The told lines of `said` back; its record's error stays text.
pub fn told_of(told: wire::Told) -> Told {
    match told {
        wire::Told::Output(text) => Told::Output(text),
        wire::Told::Notice(text) => Told::Notice(text),
        wire::Told::Refusal(text) => Told::Refusal(text),
        wire::Told::Fault(text) => Told::Fault(text),
        wire::Told::Pending(text) => Told::Pending(text),
    }
}

pub fn write_of(write: wire::MemoryWrite) -> Write {
    Write {
        name: write.name,
        kind: memory_kind_of(write.kind),
        text: write.text,
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

pub fn report_of(report: wire::Report) -> Report {
    match report {
        wire::Report::Event(event) => Report::Event(event_of(event)),
        wire::Report::Prefill => Report::Prefill,
        wire::Report::Thinking(text) => Report::Thinking(text),
        wire::Report::ThinkingDone => Report::ThinkingDone,
        wire::Report::ActionStarted => Report::ActionStarted,
        wire::Report::ActionText(text) => Report::ActionText(text),
        wire::Report::Stirred { attempt, max, error } => Report::Stirred { attempt, max, error },
        wire::Report::Compacting => Report::Compacting,
        wire::Report::Compacted => Report::Compacted,
        wire::Report::CompactFailed => Report::CompactFailed,
        wire::Report::ContextUsed(n) => Report::ContextUsed(n),
        wire::Report::Decoded(n) => Report::Decoded(n),
        wire::Report::PromptProgress(p) => Report::PromptProgress(PromptProgress {
            total: p.total,
            cache: p.cache,
            processed: p.processed,
            time_ms: p.time_ms,
        }),
        wire::Report::PromptRead(p) => Report::PromptRead(Prefill {
            tokens: p.tokens,
            ms: p.ms,
        }),
        wire::Report::StepStarted(s) => Report::StepStarted(StepRecord {
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
        wire::Report::Transition(t) => Report::Transition(Transition {
            from: phase_of(t.from),
            to: phase_of(t.to),
            event: phase_event_of(t.event),
            time: Duration::from_micros(t.micros),
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
                call: wire::ToolCall {
                    id: call.id,
                    name: call.name,
                    args: call.args
                        .into_iter()
                        .map(|(name, value)| (name, self::value(value)))
                        .collect(),
                },
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

pub fn event_of(event: wire::Event) -> Event {
    match event {
        wire::Event::User(text) => Event::User(text),
        wire::Event::Wake { flow, text } => Event::Wake { flow, text },
        wire::Event::State(name) => Event::State(name),
        wire::Event::Thinking(text) => Event::Thinking(text),
        wire::Event::Call { text, call, .. } => Event::Call {
            text,
            call: ToolCall {
                id: call.id,
                name: call.name,
                args: call.args.into_iter().map(|(name, value)| (name, value_of(value))).collect(),
            },
        },
        wire::Event::Result { id, outcome, content } => Event::Result {
            id,
            outcome: outcome_of(outcome),
            content,
        },
        wire::Event::Forge { name, kind, section } => Event::Forge {
            name,
            kind: forge_kind_of(kind),
            section,
        },
        wire::Event::Answer(text) => Event::Answer(text),
        wire::Event::CutOff(cut) => Event::CutOff(CutOff {
            limit: cut.limit,
            thinking: cut.thinking,
            discarded: cut.discarded,
            call: cut.call,
        }),
        wire::Event::Stir { attempt, error } => Event::Stir { attempt, error },
        wire::Event::Stop(reason) => Event::Stop(stop_reason_of(reason)),
        wire::Event::Memory { name, kind, text } => Event::Memory {
            name,
            kind: memory_kind_of(kind),
            text,
        },
        wire::Event::Media(media) => Event::Media(Media {
            kind: media_kind_of(media.kind),
            number: media.number,
            sha256: media.sha256,
            ext: media.ext,
            name: media.name,
            transcript: media.transcript,
            frames: media
                .frames
                .into_iter()
                .map(|frame| Frame {
                    at: frame.at,
                    sha256: frame.sha256,
                })
                .collect(),
        }),
        wire::Event::Compact { summary, question } => Event::Compact { summary, question },
        wire::Event::Command(line) => Event::Command(command_of(line)),
        wire::Event::System { text, failed } => Event::System { text, failed },
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

/// A medium waiting to be sent, its bytes in base64 (D14).
pub fn attached(pending: &Pending) -> wire::Attached {
    wire::Attached {
        number: pending.number,
        kind: media_kind(pending.source.kind),
        name: pending.source.name.clone(),
        ext: pending.source.ext.clone(),
        base64: wire::base64::encode(&pending.source.bytes),
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

fn value(value: Value) -> wire::Value {
    match value {
        Value::Text(text) => wire::Value::Text(text),
        Value::Int(n) => wire::Value::Int(n),
    }
}

fn value_of(value: wire::Value) -> Value {
    match value {
        wire::Value::Text(text) => Value::Text(text),
        wire::Value::Int(n) => Value::Int(n),
    }
}

fn outcome(outcome: Outcome) -> wire::Outcome {
    match outcome {
        Outcome::Ok => wire::Outcome::Ok,
        Outcome::Fail => wire::Outcome::Fail,
        Outcome::Timeout => wire::Outcome::Timeout,
        Outcome::Interrupted => wire::Outcome::Interrupted,
    }
}

fn outcome_of(outcome: wire::Outcome) -> Outcome {
    match outcome {
        wire::Outcome::Ok => Outcome::Ok,
        wire::Outcome::Fail => Outcome::Fail,
        wire::Outcome::Timeout => Outcome::Timeout,
        wire::Outcome::Interrupted => Outcome::Interrupted,
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

pub fn stop_reason_of(reason: wire::StopReason) -> StopReason {
    match reason {
        wire::StopReason::StepLimit => StopReason::StepLimit,
        wire::StopReason::ContextFull => StopReason::ContextFull,
        wire::StopReason::ServerError => StopReason::ServerError,
        wire::StopReason::Interrupted => StopReason::Interrupted,
        wire::StopReason::StirLimit => StopReason::StirLimit,
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

fn forge_kind_of(kind: wire::ForgeKind) -> ForgeKind {
    match kind {
        wire::ForgeKind::Created => ForgeKind::Created,
        wire::ForgeKind::Edited => ForgeKind::Edited,
        wire::ForgeKind::Appended => ForgeKind::Appended,
        wire::ForgeKind::Removed => ForgeKind::Removed,
        wire::ForgeKind::StatusChanged => ForgeKind::StatusChanged,
        wire::ForgeKind::Renamed => ForgeKind::Renamed,
    }
}

fn memory_kind(kind: MemoryKind) -> wire::MemoryKind {
    match kind {
        MemoryKind::Added => wire::MemoryKind::Added,
        MemoryKind::Edited => wire::MemoryKind::Edited,
        MemoryKind::Removed => wire::MemoryKind::Removed,
    }
}

fn memory_kind_of(kind: wire::MemoryKind) -> MemoryKind {
    match kind {
        wire::MemoryKind::Added => MemoryKind::Added,
        wire::MemoryKind::Edited => MemoryKind::Edited,
        wire::MemoryKind::Removed => MemoryKind::Removed,
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

fn phase_of(phase: wire::Phase) -> Phase {
    match phase {
        wire::Phase::Ready => Phase::Ready,
        wire::Phase::StartingServer => Phase::StartingServer,
        wire::Phase::Transcribing => Phase::Transcribing,
        wire::Phase::Prefill => Phase::Prefill,
        wire::Phase::Thinking => Phase::Thinking,
        wire::Phase::Writing => Phase::Writing,
        wire::Phase::Running(tool) => Phase::Running(tool),
        wire::Phase::Compacting => Phase::Compacting,
        wire::Phase::Retry { attempt, max } => Phase::Retry { attempt, max },
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

fn phase_event_of(event: wire::PhaseEvent) -> PhaseEvent {
    match event {
        wire::PhaseEvent::ServerStart => PhaseEvent::ServerStart,
        wire::PhaseEvent::Transcribe => PhaseEvent::Transcribe,
        wire::PhaseEvent::StepStart => PhaseEvent::StepStart,
        wire::PhaseEvent::FirstThinking => PhaseEvent::FirstThinking,
        wire::PhaseEvent::FirstText => PhaseEvent::FirstText,
        wire::PhaseEvent::ToolStart { tool } => PhaseEvent::ToolStart { tool },
        wire::PhaseEvent::ToolEnd(outcome) => PhaseEvent::ToolEnd(outcome_of(outcome)),
        wire::PhaseEvent::CompactStart => PhaseEvent::CompactStart,
        wire::PhaseEvent::Compacted => PhaseEvent::Compacted,
        wire::PhaseEvent::CompactFailed => PhaseEvent::CompactFailed,
        wire::PhaseEvent::CutOff => PhaseEvent::CutOff,
        wire::PhaseEvent::Stir { attempt, max } => PhaseEvent::Stir { attempt, max },
        wire::PhaseEvent::Answer => PhaseEvent::Answer,
        wire::PhaseEvent::Stop(reason) => PhaseEvent::Stop(stop_reason_of(reason)),
        wire::PhaseEvent::Failed => PhaseEvent::Failed,
        wire::PhaseEvent::Idle => PhaseEvent::Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_goes_to_the_wire_and_back_as_it_was() {
        let reports = [
            Report::Event(Event::Call {
                text: Some("look".into()),
                call: ToolCall {
                    id: "c1".into(),
                    name: "bash".into(),
                    args: vec![("command".into(), Value::Text("ls".into())), ("timeout".into(), Value::Int(9))],
                },
            }),
            Report::Event(Event::Result {
                id: "c1".into(),
                outcome: Outcome::Timeout,
                content: "…".into(),
            }),
            Report::Event(Event::Stop(StopReason::StirLimit)),
            Report::Event(Event::Command(CommandRun {
                line: "/status".into(),
                failed: false,
                output: None,
            })),
            Report::Stirred {
                attempt: 1,
                max: 3,
                error: "bad".into(),
            },
            Report::Transition(Transition {
                from: Phase::Running("bash".into()),
                to: Phase::Retry { attempt: 2, max: 3 },
                event: PhaseEvent::ToolEnd(Outcome::Fail),
                time: Duration::from_micros(1234),
                violation: true,
            }),
            Report::PromptRead(Prefill { tokens: 10, ms: 2.5 }),
        ];
        for report in reports {
            assert_eq!(report_of(self::report(report.clone())), report);
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
    fn a_medium_goes_as_base64_and_back() {
        let pending = Pending {
            number: 2,
            source: Source {
                kind: Kind::Audio,
                bytes: vec![0, 1, 2, 255],
                ext: "wav".into(),
                name: "said.wav".into(),
            },
        };
        assert_eq!(attached_of(attached(&pending)), Some(pending));
    }
}
