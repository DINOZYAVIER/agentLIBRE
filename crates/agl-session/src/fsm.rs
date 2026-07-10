use std::error::Error;
use std::fmt;

use agl_ids::{AttemptId, MessageId, RunId, SessionId, TurnId};

use crate::AgentLibreSessionFinishReason;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChatSessionPhase {
    Uninitialized,
    Started,
    AwaitingInput,
    HandlingCommand,
    RecordingUserMessage,
    RunningTurn,
    RecordingAssistantMessage,
    ContextCleared,
    Finished,
    Failed,
}

impl ChatSessionPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            ChatSessionPhase::Uninitialized => "uninitialized",
            ChatSessionPhase::Started => "started",
            ChatSessionPhase::AwaitingInput => "awaiting_input",
            ChatSessionPhase::HandlingCommand => "handling_command",
            ChatSessionPhase::RecordingUserMessage => "recording_user_message",
            ChatSessionPhase::RunningTurn => "running_turn",
            ChatSessionPhase::RecordingAssistantMessage => "recording_assistant_message",
            ChatSessionPhase::ContextCleared => "context_cleared",
            ChatSessionPhase::Finished => "finished",
            ChatSessionPhase::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChatSessionTransition {
    StartNewSession,
    ResumeSession,
    PromptForInput,
    ReadUserMessage {
        content: String,
    },
    ReadCommandClear,
    ReadCommandExit,
    RecordUserMessage {
        run_id: RunId,
        turn_id: TurnId,
        message_id: MessageId,
        content: String,
    },
    LinkModelAttempt {
        run_id: RunId,
        turn_id: TurnId,
        attempt_id: AttemptId,
    },
    RecordAssistantAnswer {
        run_id: RunId,
        turn_id: TurnId,
        message_id: MessageId,
        content: String,
    },
    RecordAssistantStopMarker {
        run_id: RunId,
        turn_id: TurnId,
        message_id: MessageId,
        content: String,
    },
    RecordAssistantToolCall {
        run_id: RunId,
        turn_id: TurnId,
        message_id: MessageId,
        name: String,
        arguments: serde_json::Value,
    },
    RecordToolMessage {
        run_id: RunId,
        turn_id: TurnId,
        message_id: MessageId,
        name: String,
        data: serde_json::Value,
    },
    ClearContext,
    FinishSession {
        reason: AgentLibreSessionFinishReason,
    },
    FailSession {
        message: String,
    },
}

impl ChatSessionTransition {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChatSessionTransition::StartNewSession => "start_new_session",
            ChatSessionTransition::ResumeSession => "resume_session",
            ChatSessionTransition::PromptForInput => "prompt_for_input",
            ChatSessionTransition::ReadUserMessage { .. } => "read_user_message",
            ChatSessionTransition::ReadCommandClear => "read_command_clear",
            ChatSessionTransition::ReadCommandExit => "read_command_exit",
            ChatSessionTransition::RecordUserMessage { .. } => "record_user_message",
            ChatSessionTransition::LinkModelAttempt { .. } => "link_model_attempt",
            ChatSessionTransition::RecordAssistantAnswer { .. } => "record_assistant_answer",
            ChatSessionTransition::RecordAssistantStopMarker { .. } => {
                "record_assistant_stop_marker"
            }
            ChatSessionTransition::RecordAssistantToolCall { .. } => "record_assistant_tool_call",
            ChatSessionTransition::RecordToolMessage { .. } => "record_tool_message",
            ChatSessionTransition::ClearContext => "clear_context",
            ChatSessionTransition::FinishSession { .. } => "finish_session",
            ChatSessionTransition::FailSession { .. } => "fail_session",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatSessionTransitionRecord {
    pub session_id: SessionId,
    pub sequence: usize,
    pub from: ChatSessionPhase,
    pub to: ChatSessionPhase,
    pub transition: ChatSessionTransition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatSessionMachine {
    session_id: SessionId,
    phase: ChatSessionPhase,
    sequence: usize,
}

impl ChatSessionMachine {
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            phase: ChatSessionPhase::Uninitialized,
            sequence: 0,
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    #[cfg(test)]
    pub(crate) fn phase(&self) -> ChatSessionPhase {
        self.phase
    }

    pub fn apply(
        &mut self,
        transition: ChatSessionTransition,
    ) -> Result<ChatSessionTransitionRecord, ChatSessionTransitionError> {
        let from = self.phase;
        let Some(to) = next_phase(from, &transition) else {
            return Err(ChatSessionTransitionError {
                phase: from,
                transition: transition.as_str(),
            });
        };

        self.sequence += 1;
        self.phase = to;
        Ok(ChatSessionTransitionRecord {
            session_id: self.session_id.clone(),
            sequence: self.sequence,
            from,
            to,
            transition,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatSessionTransitionError {
    pub phase: ChatSessionPhase,
    pub transition: &'static str,
}

impl fmt::Display for ChatSessionTransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "illegal chat session transition `{}` from phase `{}`",
            self.transition,
            self.phase.as_str()
        )
    }
}

impl Error for ChatSessionTransitionError {}

fn next_phase(
    from: ChatSessionPhase,
    transition: &ChatSessionTransition,
) -> Option<ChatSessionPhase> {
    use ChatSessionPhase::*;
    use ChatSessionTransition::*;

    match (from, transition) {
        (Uninitialized, StartNewSession) => Some(Started),
        (Uninitialized, ResumeSession) => Some(Started),
        (Started | ContextCleared, PromptForInput) => Some(AwaitingInput),
        (AwaitingInput, ReadUserMessage { .. }) => Some(RecordingUserMessage),
        (AwaitingInput, ReadCommandClear) => Some(HandlingCommand),
        (HandlingCommand, ClearContext) => Some(ContextCleared),
        (RecordingUserMessage, RecordUserMessage { .. }) => Some(RunningTurn),
        (RunningTurn, LinkModelAttempt { .. }) => Some(RunningTurn),
        (RunningTurn, RecordAssistantToolCall { .. }) => Some(RunningTurn),
        (RunningTurn, RecordToolMessage { .. }) => Some(RunningTurn),
        (RunningTurn, RecordAssistantAnswer { .. }) => Some(RecordingAssistantMessage),
        (RunningTurn, RecordAssistantStopMarker { .. }) => Some(RecordingAssistantMessage),
        (RecordingAssistantMessage, PromptForInput) => Some(AwaitingInput),
        (AwaitingInput, ReadCommandExit) => Some(Finished),
        (AwaitingInput, FinishSession { .. }) => Some(Finished),
        (Started, FinishSession { .. }) => Some(Finished),
        (_, FailSession { .. }) if !matches!(from, Finished | Failed) => Some(Failed),
        _ => None,
    }
}
