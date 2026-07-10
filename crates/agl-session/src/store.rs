use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agl_events::{EventEnvelope, RuntimeEvent, RuntimeEventEnvelope};
use agl_ids::{MessageId, RunId, SessionId, TurnId};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::fsm::{ChatSessionMachine, ChatSessionTransition, ChatSessionTransitionRecord};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMetadata {
    pub session_id: SessionId,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
    pub local_inference_config_path: PathBuf,
    pub backend: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLibreSessionFinishReason {
    Eof,
    ExitCommand,
    HostShutdown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatSessionReplay {
    pub events: Vec<ChatSessionEvent>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChatSessionEvent {
    Runtime {
        envelope: Box<EventEnvelope<RuntimeEvent>>,
    },
    SessionStarted {
        session_id: SessionId,
    },
    ContextCleared {
        session_id: SessionId,
    },
    SessionFinished {
        session_id: SessionId,
        reason: AgentLibreSessionFinishReason,
    },
    SessionFailed {
        session_id: SessionId,
        message: String,
    },
}

#[derive(Clone, Debug)]
pub struct ChatSessionStore {
    machine: ChatSessionMachine,
    session_dir: PathBuf,
    transcript_jsonl: PathBuf,
    run_sequences: BTreeMap<RunId, u64>,
    event_ids: BTreeSet<agl_ids::EventId>,
}

impl ChatSessionStore {
    pub fn exists(sessions_root: impl AsRef<Path>, session_id: &SessionId) -> bool {
        sessions_root
            .as_ref()
            .join(session_id.as_str())
            .join("session.json")
            .exists()
    }

    pub fn start(
        sessions_root: impl AsRef<Path>,
        session_id: SessionId,
        local_inference_config_path: impl Into<PathBuf>,
        backend: impl Into<String>,
    ) -> Result<Self> {
        let sessions_root = sessions_root.as_ref();
        std::fs::create_dir_all(sessions_root).with_context(|| {
            format!(
                "failed to create chat sessions root {}",
                sessions_root.display()
            )
        })?;
        let session_dir = sessions_root.join(session_id.as_str());
        let transcript_jsonl = session_dir.join("transcript.jsonl");
        create_new_session_dir(&session_dir)?;

        let metadata = SessionMetadata {
            session_id: session_id.clone(),
            created_at_unix_ms: unix_millis(),
            updated_at_unix_ms: unix_millis(),
            local_inference_config_path: local_inference_config_path.into(),
            backend: backend.into(),
        };
        write_new_json(&session_dir.join("session.json"), &metadata)?;

        let mut store = Self {
            machine: ChatSessionMachine::new(session_id),
            session_dir,
            transcript_jsonl,
            run_sequences: BTreeMap::new(),
            event_ids: BTreeSet::new(),
        };
        let record = store.apply(ChatSessionTransition::StartNewSession)?;
        store.append_record_event(&record)?;
        store.apply(ChatSessionTransition::PromptForInput)?;
        Ok(store)
    }

    pub fn open(sessions_root: impl AsRef<Path>, session_id: SessionId) -> Result<Self> {
        let session_dir = sessions_root.as_ref().join(session_id.as_str());
        let metadata_path = session_dir.join("session.json");
        if !metadata_path.exists() {
            bail!(
                "chat session metadata does not exist: {}",
                metadata_path.display()
            );
        }
        let metadata_bytes = std::fs::read(&metadata_path)
            .with_context(|| format!("failed to read {}", metadata_path.display()))?;
        let metadata: SessionMetadata = serde_json::from_slice(&metadata_bytes)
            .with_context(|| format!("failed to parse {}", metadata_path.display()))?;
        ensure!(
            metadata.session_id == session_id,
            "chat session metadata ID {} does not match requested session {}",
            metadata.session_id,
            session_id
        );

        let mut store = Self {
            machine: ChatSessionMachine::new(session_id),
            transcript_jsonl: session_dir.join("transcript.jsonl"),
            session_dir,
            run_sequences: BTreeMap::new(),
            event_ids: BTreeSet::new(),
        };
        store.recover_runtime_state()?;
        store.apply(ChatSessionTransition::ResumeSession)?;
        store.apply(ChatSessionTransition::PromptForInput)?;
        Ok(store)
    }

    pub fn session_id(&self) -> &SessionId {
        self.machine.session_id()
    }

    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    pub fn transcript_jsonl(&self) -> &Path {
        &self.transcript_jsonl
    }

    #[cfg(test)]
    pub(crate) fn machine(&self) -> &ChatSessionMachine {
        &self.machine
    }

    pub fn read_replay(&self) -> Result<ChatSessionReplay> {
        let content = match std::fs::read_to_string(&self.transcript_jsonl) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => Err(err).with_context(|| {
                format!(
                    "failed to read chat transcript {}",
                    self.transcript_jsonl.display()
                )
            })?,
        };
        ensure!(
            content.is_empty() || content.ends_with('\n'),
            "chat transcript {} ends with an incomplete JSONL record",
            self.transcript_jsonl.display()
        );

        let mut events = Vec::new();
        let mut run_sequences = BTreeMap::new();
        let mut event_ids = BTreeSet::new();
        for (line_index, line) in content.lines().enumerate() {
            ensure!(
                !line.trim().is_empty(),
                "chat transcript {} contains an empty record at line {}",
                self.transcript_jsonl.display(),
                line_index + 1
            );
            let event: ChatSessionEvent = serde_json::from_str(line).with_context(|| {
                format!(
                    "failed to parse chat transcript {} line {}",
                    self.transcript_jsonl.display(),
                    line_index + 1
                )
            })?;
            validate_session_event(
                &event,
                self.session_id(),
                &mut run_sequences,
                &mut event_ids,
            )
            .with_context(|| {
                format!(
                    "invalid chat transcript {} line {}",
                    self.transcript_jsonl.display(),
                    line_index + 1
                )
            })?;
            events.push(event);
        }

        Ok(ChatSessionReplay { events })
    }

    fn recover_runtime_state(&mut self) -> Result<()> {
        let replay = self.read_replay()?;
        for event in replay.events {
            let ChatSessionEvent::Runtime { envelope } = event else {
                continue;
            };
            let envelope = *envelope;
            let run_id = envelope.scope.run_id().clone();
            self.run_sequences.insert(run_id, envelope.sequence);
            self.event_ids.insert(envelope.event_id);
        }
        Ok(())
    }

    pub fn append_user_message(&mut self, envelope: RuntimeEventEnvelope) -> Result<()> {
        let (message_id, content) = match &envelope.payload {
            RuntimeEvent::UserMessage {
                message_id,
                content,
            } => (message_id.clone(), content.clone()),
            _ => bail!("expected user_message runtime transcript envelope"),
        };
        let (run_id, turn_id) = self.runtime_identity(&envelope)?;
        self.apply(ChatSessionTransition::ReadUserMessage {
            content: content.clone(),
        })?;
        self.apply(ChatSessionTransition::RecordUserMessage {
            run_id,
            turn_id,
            message_id,
            content,
        })?;
        self.append_runtime_envelope(envelope)
    }

    pub fn append_assistant_message(&mut self, envelope: RuntimeEventEnvelope) -> Result<()> {
        let (message_id, content) = assistant_message(&envelope)?;
        let (run_id, turn_id) = self.runtime_identity(&envelope)?;
        self.apply(ChatSessionTransition::RecordAssistantAnswer {
            run_id,
            turn_id,
            message_id,
            content,
        })?;
        self.append_runtime_envelope(envelope)?;
        self.apply(ChatSessionTransition::PromptForInput)?;
        Ok(())
    }

    pub fn append_assistant_stop_marker(&mut self, envelope: RuntimeEventEnvelope) -> Result<()> {
        let (message_id, content) = assistant_message(&envelope)?;
        let (run_id, turn_id) = self.runtime_identity(&envelope)?;
        self.apply(ChatSessionTransition::RecordAssistantStopMarker {
            run_id,
            turn_id,
            message_id,
            content,
        })?;
        self.append_runtime_envelope(envelope)?;
        self.apply(ChatSessionTransition::PromptForInput)?;
        Ok(())
    }

    pub fn append_assistant_tool_call(&mut self, envelope: RuntimeEventEnvelope) -> Result<()> {
        let (message_id, name, arguments) = match &envelope.payload {
            RuntimeEvent::AssistantToolCall {
                message_id,
                name,
                arguments,
            } => (message_id.clone(), name.clone(), arguments.clone()),
            _ => bail!("expected assistant_tool_call runtime transcript envelope"),
        };
        let (run_id, turn_id) = self.runtime_identity(&envelope)?;
        self.apply(ChatSessionTransition::RecordAssistantToolCall {
            run_id,
            turn_id,
            message_id,
            name,
            arguments,
        })?;
        self.append_runtime_envelope(envelope)
    }

    pub fn append_tool_message(&mut self, envelope: RuntimeEventEnvelope) -> Result<()> {
        let (message_id, name, data) = match &envelope.payload {
            RuntimeEvent::ToolMessage {
                message_id,
                name,
                data,
            } => (message_id.clone(), name.clone(), data.clone()),
            _ => bail!("expected tool_message runtime transcript envelope"),
        };
        let (run_id, turn_id) = self.runtime_identity(&envelope)?;
        self.apply(ChatSessionTransition::RecordToolMessage {
            run_id,
            turn_id,
            message_id,
            name,
            data,
        })?;
        self.append_runtime_envelope(envelope)
    }

    pub fn link_attempt(&mut self, envelope: RuntimeEventEnvelope) -> Result<()> {
        ensure!(
            matches!(envelope.payload, RuntimeEvent::ModelAttemptLinked),
            "expected model_attempt_linked runtime transcript envelope"
        );
        let (run_id, turn_id) = self.runtime_identity(&envelope)?;
        let attempt_id = envelope
            .scope
            .attempt_id()
            .cloned()
            .context("model attempt transcript envelope is missing attempt ID")?;
        self.apply(ChatSessionTransition::LinkModelAttempt {
            run_id,
            turn_id,
            attempt_id,
        })?;
        self.append_runtime_envelope(envelope)
    }

    pub fn append_context_cleared(&mut self) -> Result<()> {
        self.apply(ChatSessionTransition::ReadCommandClear)?;
        self.append_transition_event_and_prompt(ChatSessionTransition::ClearContext)
    }

    pub fn finish(&mut self) -> Result<()> {
        self.finish_with_reason(AgentLibreSessionFinishReason::HostShutdown)
    }

    pub fn finish_eof(&mut self) -> Result<()> {
        self.finish_with_reason(AgentLibreSessionFinishReason::Eof)
    }

    pub fn request_exit(&mut self) -> Result<()> {
        self.append_transition_event(ChatSessionTransition::ReadCommandExit)
    }

    pub fn fail(&mut self, message: impl Into<String>) -> Result<()> {
        self.append_transition_event(ChatSessionTransition::FailSession {
            message: message.into(),
        })
    }

    fn finish_with_reason(&mut self, reason: AgentLibreSessionFinishReason) -> Result<()> {
        self.append_transition_event(ChatSessionTransition::FinishSession { reason })
    }

    fn apply(&mut self, transition: ChatSessionTransition) -> Result<ChatSessionTransitionRecord> {
        Ok(self.machine.apply(transition)?)
    }

    fn append_transition_event(&mut self, transition: ChatSessionTransition) -> Result<()> {
        let record = self.apply(transition)?;
        self.append_record_event(&record)
    }

    fn append_transition_event_and_prompt(
        &mut self,
        transition: ChatSessionTransition,
    ) -> Result<()> {
        self.append_transition_event(transition)?;
        self.apply(ChatSessionTransition::PromptForInput)?;
        Ok(())
    }

    fn append_record_event(&mut self, record: &ChatSessionTransitionRecord) -> Result<()> {
        let Some(event) = control_event_from_transition(record) else {
            return Ok(());
        };
        self.append(&event)
    }

    fn append_runtime_envelope(&mut self, envelope: RuntimeEventEnvelope) -> Result<()> {
        let event = ChatSessionEvent::Runtime {
            envelope: Box::new(envelope),
        };
        let mut run_sequences = self.run_sequences.clone();
        let mut event_ids = self.event_ids.clone();
        validate_session_event(
            &event,
            self.session_id(),
            &mut run_sequences,
            &mut event_ids,
        )?;
        self.append(&event)?;
        self.run_sequences = run_sequences;
        self.event_ids = event_ids;
        Ok(())
    }

    fn runtime_identity(&self, envelope: &RuntimeEventEnvelope) -> Result<(RunId, TurnId)> {
        ensure!(
            envelope.scope.session_id() == Some(self.session_id()),
            "runtime transcript envelope belongs to a different session"
        );
        let turn_id = envelope
            .scope
            .turn_id()
            .cloned()
            .context("runtime transcript envelope is missing turn ID")?;
        Ok((envelope.scope.run_id().clone(), turn_id))
    }

    fn append(&self, event: &ChatSessionEvent) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.transcript_jsonl)
            .with_context(|| {
                format!(
                    "failed to open chat transcript {}",
                    self.transcript_jsonl.display()
                )
            })?;
        let line = serde_json::to_string(event).context("failed to serialize chat event")?;
        file.write_all(line.as_bytes())
            .context("failed to write chat event")?;
        file.write_all(b"\n")
            .context("failed to write chat event newline")?;
        file.flush().context("failed to flush chat transcript")
    }
}

fn control_event_from_transition(record: &ChatSessionTransitionRecord) -> Option<ChatSessionEvent> {
    match &record.transition {
        ChatSessionTransition::StartNewSession => Some(ChatSessionEvent::SessionStarted {
            session_id: record.session_id.clone(),
        }),
        ChatSessionTransition::ClearContext => Some(ChatSessionEvent::ContextCleared {
            session_id: record.session_id.clone(),
        }),
        ChatSessionTransition::ReadCommandExit => Some(ChatSessionEvent::SessionFinished {
            session_id: record.session_id.clone(),
            reason: AgentLibreSessionFinishReason::ExitCommand,
        }),
        ChatSessionTransition::FinishSession { reason } => {
            Some(ChatSessionEvent::SessionFinished {
                session_id: record.session_id.clone(),
                reason: *reason,
            })
        }
        ChatSessionTransition::FailSession { message } => Some(ChatSessionEvent::SessionFailed {
            session_id: record.session_id.clone(),
            message: message.clone(),
        }),
        _ => None,
    }
}

fn assistant_message(envelope: &RuntimeEventEnvelope) -> Result<(MessageId, String)> {
    match &envelope.payload {
        RuntimeEvent::AssistantMessage {
            message_id,
            content,
        } => Ok((message_id.clone(), content.clone())),
        _ => bail!("expected assistant_message runtime transcript envelope"),
    }
}

fn validate_session_event(
    event: &ChatSessionEvent,
    session_id: &SessionId,
    run_sequences: &mut BTreeMap<RunId, u64>,
    event_ids: &mut BTreeSet<agl_ids::EventId>,
) -> Result<()> {
    match event {
        ChatSessionEvent::Runtime { envelope } => {
            ensure!(
                envelope.scope.session_id() == Some(session_id),
                "runtime transcript event belongs to a different session"
            );
            ensure!(
                envelope.scope.turn_id().is_some(),
                "runtime transcript event is missing its turn ID"
            );
            let attempt_linked = matches!(envelope.payload, RuntimeEvent::ModelAttemptLinked);
            ensure!(
                attempt_linked == envelope.scope.attempt_id().is_some(),
                "runtime transcript attempt scope does not match its payload"
            );
            ensure!(
                matches!(
                    envelope.payload,
                    RuntimeEvent::UserMessage { .. }
                        | RuntimeEvent::AssistantMessage { .. }
                        | RuntimeEvent::AssistantToolCall { .. }
                        | RuntimeEvent::ToolMessage { .. }
                        | RuntimeEvent::ModelAttemptLinked
                ),
                "runtime transcript contains a non-transcript payload"
            );
            ensure!(
                event_ids.insert(envelope.event_id.clone()),
                "runtime transcript contains duplicate event ID {}",
                envelope.event_id
            );
            let run_id = envelope.scope.run_id().clone();
            let previous = run_sequences.get(&run_id).copied().unwrap_or(0);
            ensure!(
                envelope.sequence > previous,
                "runtime transcript run {} sequence {} does not follow {}",
                run_id,
                envelope.sequence,
                previous
            );
            run_sequences.insert(run_id, envelope.sequence);
        }
        ChatSessionEvent::SessionStarted { session_id: actual }
        | ChatSessionEvent::ContextCleared { session_id: actual }
        | ChatSessionEvent::SessionFinished {
            session_id: actual, ..
        }
        | ChatSessionEvent::SessionFailed {
            session_id: actual, ..
        } => ensure!(
            actual == session_id,
            "session transcript control record belongs to a different session"
        ),
    }
    Ok(())
}

fn create_new_session_dir(path: &Path) -> Result<()> {
    if path.join("session.json").exists() {
        bail!("chat session already exists: {}", path.display());
    }
    std::fs::create_dir_all(path)
        .with_context(|| format!("failed to create chat session directory {}", path.display()))
}

fn write_new_json<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize + ?Sized,
{
    let bytes = serde_json::to_vec_pretty(value)
        .with_context(|| format!("failed to serialize JSON {}", path.display()))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}
