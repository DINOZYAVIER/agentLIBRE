use agl_protocol::{SessionFinishReason, TranscriptEvent};
use agl_session::ChatSessionEvent;

pub(crate) fn transcript_event(
    event: ChatSessionEvent,
    include_content: bool,
) -> Option<TranscriptEvent> {
    match event {
        ChatSessionEvent::SessionStarted { .. } => None,
        ChatSessionEvent::UserMessage {
            message_id,
            content,
            ..
        } => Some(TranscriptEvent::UserMessage {
            message_id: message_id.to_string(),
            content: include_content.then_some(content),
        }),
        ChatSessionEvent::AssistantMessage {
            message_id,
            content,
            ..
        } => Some(TranscriptEvent::AssistantMessage {
            message_id: message_id.to_string(),
            content: include_content.then_some(content),
        }),
        ChatSessionEvent::AssistantToolCall {
            message_id,
            name,
            arguments,
            ..
        } => Some(TranscriptEvent::AssistantToolCall {
            message_id: message_id.to_string(),
            name,
            arguments: include_content.then_some(arguments),
        }),
        ChatSessionEvent::ToolMessage {
            message_id,
            name,
            content,
            ..
        } => Some(TranscriptEvent::ToolMessage {
            message_id: message_id.to_string(),
            name,
            content: include_content.then_some(content),
        }),
        ChatSessionEvent::ModelAttemptLinked {
            run_id, attempt_id, ..
        } => Some(TranscriptEvent::ModelAttemptLinked { run_id, attempt_id }),
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
