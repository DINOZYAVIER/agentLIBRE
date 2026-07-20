use agl_events::{EventEnvelope, RuntimeEvent};
use agl_protocol::{SessionFinishReason, TranscriptEvent};
use agl_session::ChatSessionEvent;

pub(crate) fn transcript_event(
    event: ChatSessionEvent,
    include_content: bool,
) -> Option<TranscriptEvent> {
    match event {
        ChatSessionEvent::SessionStarted { .. } => None,
        ChatSessionEvent::Runtime { envelope } => {
            runtime_transcript_event(*envelope, include_content)
        }
        ChatSessionEvent::ContextCleared { .. } => Some(TranscriptEvent::ContextCleared),
        ChatSessionEvent::SessionFinished { reason, .. } => {
            Some(TranscriptEvent::SessionFinished {
                reason: match reason {
                    agl_session::AgentLibreSessionFinishReason::Eof => SessionFinishReason::Eof,
                    agl_session::AgentLibreSessionFinishReason::ExitCommand => {
                        SessionFinishReason::ExitCommand
                    }
                    agl_session::AgentLibreSessionFinishReason::HostShutdown => {
                        SessionFinishReason::HostShutdown
                    }
                },
            })
        }
        ChatSessionEvent::SessionFailed { message, .. } => {
            Some(TranscriptEvent::SessionFailed { message })
        }
    }
}

fn runtime_transcript_event(
    envelope: EventEnvelope<RuntimeEvent>,
    include_content: bool,
) -> Option<TranscriptEvent> {
    let run_id = envelope.scope.run_id().clone();
    let turn_id = envelope.scope.turn_id()?.clone();
    let attempt_id = envelope.scope.attempt_id().cloned();
    match envelope.payload {
        RuntimeEvent::UserMessage {
            message_id,
            content,
        } => Some(TranscriptEvent::UserMessage {
            run_id,
            turn_id,
            message_id,
            content: include_content.then_some(content),
        }),
        RuntimeEvent::AssistantMessage {
            message_id,
            content,
        } => Some(TranscriptEvent::AssistantMessage {
            run_id,
            turn_id,
            message_id,
            content: include_content.then_some(content),
        }),
        RuntimeEvent::AssistantToolCall {
            message_id,
            name,
            arguments,
        } => Some(TranscriptEvent::AssistantToolCall {
            run_id,
            turn_id,
            message_id,
            name,
            arguments: include_content.then_some(arguments),
        }),
        RuntimeEvent::ToolMessage {
            message_id,
            name,
            data,
        } => Some(TranscriptEvent::ToolMessage {
            run_id,
            turn_id,
            message_id,
            name,
            data: include_content.then_some(data),
        }),
        RuntimeEvent::ModelAttemptLinked => Some(TranscriptEvent::ModelAttemptLinked {
            run_id,
            turn_id,
            attempt_id: attempt_id?,
        }),
        _ => None,
    }
}
