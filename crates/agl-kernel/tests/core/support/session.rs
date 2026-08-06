#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTransition {
    StartNew,
    Resume,
    PromptForInput,
    ReadUserMessage,
    ReadCommandClear,
    ReadCommandExit,
    RecordUserMessage,
    BeginIncompleteContinuation,
    LinkModelAttempt,
    RecordAssistantAnswer,
    RecordAssistantStop,
    RecordAssistantToolCall,
    RecordToolMessage,
    ClearContext,
    Finish,
    Fail,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionRecordView {
    pub session_id: String,
    pub sequence: u64,
    pub from: String,
    pub to: String,
    pub transition: SessionTransition,
}

/// Test-only wiring point for the kernel-owned Session machine. It contains no
/// transition table; AGL-170 replaces each method with direct production calls.
pub struct ProductionSessionMachine {
    label: String,
    machine: agl_kernel::ChatSessionMachine,
}

impl ProductionSessionMachine {
    pub fn new(session_id: &str) -> Self {
        Self {
            label: session_id.to_string(),
            machine: agl_kernel::ChatSessionMachine::new(agl_ids::SessionId::generate()),
        }
    }

    pub fn state(&self) -> String {
        self.machine.phase().as_str().to_string()
    }

    pub fn sequence(&self) -> u64 {
        self.machine.sequence()
    }

    pub fn apply(&mut self, transition: SessionTransition) -> Result<SessionRecordView, String> {
        use agl_kernel::ChatSessionTransition as T;
        let run_id = agl_ids::RunId::generate();
        let turn_id = agl_ids::TurnId::generate();
        let message_id = agl_ids::MessageId::generate();
        let production = match transition {
            SessionTransition::StartNew => T::StartNewSession,
            SessionTransition::Resume => T::ResumeSession,
            SessionTransition::PromptForInput => T::PromptForInput,
            SessionTransition::ReadUserMessage => T::ReadUserMessage {
                content: agl_content::Content::text("user").unwrap(),
            },
            SessionTransition::ReadCommandClear => T::ReadCommandClear,
            SessionTransition::ReadCommandExit => T::ReadCommandExit,
            SessionTransition::RecordUserMessage => T::RecordUserMessage {
                run_id,
                turn_id,
                message_id,
                content: agl_content::Content::text("user").unwrap(),
            },
            SessionTransition::BeginIncompleteContinuation => T::BeginIncompleteContinuation {
                run_id,
                turn_id,
                source_message_id: message_id,
            },
            SessionTransition::LinkModelAttempt => T::LinkModelAttempt {
                run_id,
                turn_id,
                attempt_id: agl_ids::AttemptId::generate(),
            },
            SessionTransition::RecordAssistantAnswer => T::RecordAssistantAnswer {
                run_id,
                turn_id,
                message_id,
                content: agl_content::Content::text("answer").unwrap(),
            },
            SessionTransition::RecordAssistantStop => T::RecordAssistantStopMarker {
                run_id,
                turn_id,
                message_id,
                content: agl_content::Content::text("stop").unwrap(),
            },
            SessionTransition::RecordAssistantToolCall => T::RecordAssistantToolCall {
                run_id,
                turn_id,
                message_id,
                name: "example:tool".to_string(),
                arguments: serde_json::json!({}),
            },
            SessionTransition::RecordToolMessage => T::RecordToolMessage {
                run_id,
                turn_id,
                message_id,
                name: "example:tool".to_string(),
                data: serde_json::json!({}),
            },
            SessionTransition::ClearContext => T::ClearContext,
            SessionTransition::Finish => T::FinishSession {
                reason: agl_kernel::AgentLibreSessionFinishReason::HostShutdown,
            },
            SessionTransition::Fail => T::FailSession {
                message: "core test failure".to_string(),
            },
        };
        let record = self
            .machine
            .apply(production)
            .map_err(|error| error.to_string())?;
        Ok(SessionRecordView {
            session_id: self.label.clone(),
            sequence: record.sequence,
            from: record.from.as_str().to_string(),
            to: record.to.as_str().to_string(),
            transition,
        })
    }

    pub fn checkpoint_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&(self.label.as_str(), &self.machine)).unwrap()
    }

    pub fn restore(bytes: &[u8]) -> Result<Self, String> {
        let (label, machine): (String, agl_kernel::ChatSessionMachine) =
            serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        Ok(Self { label, machine })
    }
}
