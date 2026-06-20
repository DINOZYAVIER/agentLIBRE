use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{ChatSessionMachine, ChatSessionTransition, ChatSessionTransitionRecord};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct AgentLibreSessionId(String);

impl AgentLibreSessionId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        ensure_path_segment(&value, "session_id")?;
        Ok(Self(value))
    }

    pub fn generate() -> Self {
        let counter = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
        Self(format!(
            "session-{}-{}-{counter}",
            unix_millis(),
            std::process::id()
        ))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AgentLibreSessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct AgentLibreMessageId(String);

impl AgentLibreMessageId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        ensure_path_segment(&value, "message_id")?;
        Ok(Self(value))
    }

    pub fn indexed(index: usize) -> Self {
        Self(format!("message-{index:04}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AgentLibreMessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub session_id: AgentLibreSessionId,
    pub created_at_unix_ms: u128,
    pub updated_at_unix_ms: u128,
    pub model_config_path: PathBuf,
    pub backend: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatSessionReplay {
    pub events: Vec<ChatSessionEvent>,
    pub next_message_index: usize,
    pub next_attempt_index: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatSessionEvent {
    SessionStarted {
        session_id: AgentLibreSessionId,
        run_id: String,
    },
    UserMessage {
        session_id: AgentLibreSessionId,
        message_id: AgentLibreMessageId,
        content: String,
    },
    AssistantMessage {
        session_id: AgentLibreSessionId,
        message_id: AgentLibreMessageId,
        content: String,
    },
    ToolMessage {
        session_id: AgentLibreSessionId,
        message_id: AgentLibreMessageId,
        name: String,
        content: String,
    },
    ModelAttemptLinked {
        session_id: AgentLibreSessionId,
        run_id: String,
        attempt_id: String,
    },
    ContextCleared {
        session_id: AgentLibreSessionId,
    },
    SessionFinished {
        session_id: AgentLibreSessionId,
    },
}

#[derive(Clone, Debug)]
pub struct ChatSessionStore {
    machine: ChatSessionMachine,
    run_id: String,
    session_dir: PathBuf,
    transcript_jsonl: PathBuf,
}

impl ChatSessionStore {
    pub fn exists(sessions_root: impl AsRef<Path>, session_id: &AgentLibreSessionId) -> bool {
        sessions_root
            .as_ref()
            .join(session_id.as_str())
            .join("session.json")
            .exists()
    }

    pub fn start(
        sessions_root: impl AsRef<Path>,
        session_id: AgentLibreSessionId,
        run_id: impl Into<String>,
        model_config_path: impl Into<PathBuf>,
        backend: impl Into<String>,
    ) -> Result<Self> {
        let run_id = run_id.into();
        ensure_path_segment(&run_id, "run_id")?;
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
            model_config_path: model_config_path.into(),
            backend: backend.into(),
        };
        write_new_json(&session_dir.join("session.json"), &metadata)?;

        let mut store = Self {
            machine: ChatSessionMachine::new(session_id),
            run_id,
            session_dir,
            transcript_jsonl,
        };
        let record = store.apply(ChatSessionTransition::StartNewSession {
            run_id: store.run_id.clone(),
        })?;
        store.append_record_event(&record)?;
        store.apply(ChatSessionTransition::PromptForInput)?;
        Ok(store)
    }

    pub fn open(
        sessions_root: impl AsRef<Path>,
        session_id: AgentLibreSessionId,
        run_id: impl Into<String>,
    ) -> Result<Self> {
        let run_id = run_id.into();
        ensure_path_segment(&run_id, "run_id")?;
        let session_dir = sessions_root.as_ref().join(session_id.as_str());
        let metadata_path = session_dir.join("session.json");
        if !metadata_path.exists() {
            bail!(
                "chat session metadata does not exist: {}",
                metadata_path.display()
            );
        }

        let mut store = Self {
            machine: ChatSessionMachine::new(session_id),
            run_id,
            transcript_jsonl: session_dir.join("transcript.jsonl"),
            session_dir,
        };
        store.apply(ChatSessionTransition::ResumeSession {
            run_id: store.run_id.clone(),
        })?;
        store.apply(ChatSessionTransition::PromptForInput)?;
        Ok(store)
    }

    pub fn session_id(&self) -> &AgentLibreSessionId {
        self.machine.session_id()
    }

    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    pub fn transcript_jsonl(&self) -> &Path {
        &self.transcript_jsonl
    }

    pub fn machine(&self) -> &ChatSessionMachine {
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

        let mut events = Vec::new();
        let mut max_message_index = 0;
        let mut max_attempt_index = 0;
        for (line_index, line) in content.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let event: ChatSessionEvent = serde_json::from_str(line).with_context(|| {
                format!(
                    "failed to parse chat transcript {} line {}",
                    self.transcript_jsonl.display(),
                    line_index + 1
                )
            })?;
            if let Some(index) = event.message_index() {
                max_message_index = max_message_index.max(index);
            }
            if let Some(index) = event.attempt_index() {
                max_attempt_index = max_attempt_index.max(index);
            }
            events.push(event);
        }

        Ok(ChatSessionReplay {
            events,
            next_message_index: max_message_index + 1,
            next_attempt_index: max_attempt_index + 1,
        })
    }

    pub fn append_user_message(
        &mut self,
        message_id: AgentLibreMessageId,
        content: String,
    ) -> Result<()> {
        self.apply(ChatSessionTransition::ReadUserMessage {
            content: content.clone(),
        })?;
        let record = self.apply(ChatSessionTransition::RecordUserMessage {
            message_id,
            content,
        })?;
        self.append_record_event(&record)
    }

    pub fn note_empty_input(&mut self) -> Result<()> {
        self.apply(ChatSessionTransition::ReadEmptyInput)?;
        Ok(())
    }

    pub fn note_help_command(&mut self) -> Result<()> {
        self.apply(ChatSessionTransition::ReadCommandHelp)?;
        self.apply(ChatSessionTransition::PromptForInput)?;
        Ok(())
    }

    pub fn note_session_command(&mut self) -> Result<()> {
        self.apply(ChatSessionTransition::ReadCommandSession)?;
        self.apply(ChatSessionTransition::PromptForInput)?;
        Ok(())
    }

    pub fn note_unknown_command(&mut self, command: impl Into<String>) -> Result<()> {
        self.apply(ChatSessionTransition::ReadUnknownCommand {
            command: command.into(),
        })?;
        self.apply(ChatSessionTransition::PromptForInput)?;
        Ok(())
    }

    pub fn append_assistant_message(
        &mut self,
        message_id: AgentLibreMessageId,
        content: String,
    ) -> Result<()> {
        let record = self.apply(ChatSessionTransition::RecordAssistantAnswer {
            message_id,
            content,
        })?;
        self.append_record_event(&record)?;
        self.apply(ChatSessionTransition::PromptForInput)?;
        Ok(())
    }

    pub fn append_assistant_stop_marker(
        &mut self,
        message_id: AgentLibreMessageId,
        content: String,
    ) -> Result<()> {
        let record = self.apply(ChatSessionTransition::RecordAssistantStopMarker {
            message_id,
            content,
        })?;
        self.append_record_event(&record)?;
        self.apply(ChatSessionTransition::PromptForInput)?;
        Ok(())
    }

    pub fn link_attempt(&mut self, attempt_id: impl Into<String>) -> Result<()> {
        let record = self.apply(ChatSessionTransition::LinkModelAttempt {
            run_id: self.run_id.clone(),
            attempt_id: attempt_id.into(),
        })?;
        self.append_record_event(&record)
    }

    pub fn append_context_cleared(&mut self) -> Result<()> {
        self.apply(ChatSessionTransition::ReadCommandClear)?;
        let record = self.apply(ChatSessionTransition::ClearContext)?;
        self.append_record_event(&record)?;
        self.apply(ChatSessionTransition::PromptForInput)?;
        Ok(())
    }

    pub fn finish(&mut self) -> Result<()> {
        let record = self.apply(ChatSessionTransition::FinishSession)?;
        self.append_record_event(&record)
    }

    fn apply(&mut self, transition: ChatSessionTransition) -> Result<ChatSessionTransitionRecord> {
        Ok(self.machine.apply(transition)?)
    }

    fn append_record_event(&self, record: &ChatSessionTransitionRecord) -> Result<()> {
        let Some(event) = ChatSessionEvent::from_transition(record) else {
            return Ok(());
        };
        self.append(&event)
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

impl ChatSessionEvent {
    fn from_transition(record: &ChatSessionTransitionRecord) -> Option<Self> {
        match &record.transition {
            ChatSessionTransition::StartNewSession { run_id } => Some(Self::SessionStarted {
                session_id: record.session_id.clone(),
                run_id: run_id.clone(),
            }),
            ChatSessionTransition::RecordUserMessage {
                message_id,
                content,
            } => Some(Self::UserMessage {
                session_id: record.session_id.clone(),
                message_id: message_id.clone(),
                content: content.clone(),
            }),
            ChatSessionTransition::RecordAssistantAnswer {
                message_id,
                content,
            }
            | ChatSessionTransition::RecordAssistantStopMarker {
                message_id,
                content,
            } => Some(Self::AssistantMessage {
                session_id: record.session_id.clone(),
                message_id: message_id.clone(),
                content: content.clone(),
            }),
            ChatSessionTransition::LinkModelAttempt { run_id, attempt_id } => {
                Some(Self::ModelAttemptLinked {
                    session_id: record.session_id.clone(),
                    run_id: run_id.clone(),
                    attempt_id: attempt_id.clone(),
                })
            }
            ChatSessionTransition::ClearContext => Some(Self::ContextCleared {
                session_id: record.session_id.clone(),
            }),
            ChatSessionTransition::FinishSession => Some(Self::SessionFinished {
                session_id: record.session_id.clone(),
            }),
            _ => None,
        }
    }

    fn message_index(&self) -> Option<usize> {
        match self {
            Self::UserMessage { message_id, .. }
            | Self::AssistantMessage { message_id, .. }
            | Self::ToolMessage { message_id, .. } => {
                indexed_suffix(message_id.as_str(), "message-")
            }
            _ => None,
        }
    }

    fn attempt_index(&self) -> Option<usize> {
        match self {
            Self::ModelAttemptLinked { attempt_id, .. } => indexed_suffix(attempt_id, "attempt-"),
            _ => None,
        }
    }
}

fn create_new_session_dir(path: &Path) -> Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("chat session already exists: {}", path.display())
        }
        Err(err) => Err(err)
            .with_context(|| format!("failed to create chat session directory {}", path.display())),
    }
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

fn ensure_path_segment(value: &str, name: &str) -> Result<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
    {
        bail!("{name} must be a path segment");
    }
    Ok(())
}

fn indexed_suffix(value: &str, prefix: &str) -> Option<usize> {
    value.strip_prefix(prefix)?.parse().ok()
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}
