use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

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
    session_id: AgentLibreSessionId,
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

        let store = Self {
            session_id,
            run_id,
            session_dir,
            transcript_jsonl,
        };
        store.append(&ChatSessionEvent::SessionStarted {
            session_id: store.session_id.clone(),
            run_id: store.run_id.clone(),
        })?;
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

        Ok(Self {
            session_id,
            run_id,
            transcript_jsonl: session_dir.join("transcript.jsonl"),
            session_dir,
        })
    }

    pub fn session_id(&self) -> &AgentLibreSessionId {
        &self.session_id
    }

    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    pub fn transcript_jsonl(&self) -> &Path {
        &self.transcript_jsonl
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
        &self,
        message_id: AgentLibreMessageId,
        content: String,
    ) -> Result<()> {
        self.append(&ChatSessionEvent::UserMessage {
            session_id: self.session_id.clone(),
            message_id,
            content,
        })
    }

    pub fn append_assistant_message(
        &self,
        message_id: AgentLibreMessageId,
        content: String,
    ) -> Result<()> {
        self.append(&ChatSessionEvent::AssistantMessage {
            session_id: self.session_id.clone(),
            message_id,
            content,
        })
    }

    pub fn link_attempt(&self, attempt_id: impl Into<String>) -> Result<()> {
        self.append(&ChatSessionEvent::ModelAttemptLinked {
            session_id: self.session_id.clone(),
            run_id: self.run_id.clone(),
            attempt_id: attempt_id.into(),
        })
    }

    pub fn append_context_cleared(&self) -> Result<()> {
        self.append(&ChatSessionEvent::ContextCleared {
            session_id: self.session_id.clone(),
        })
    }

    pub fn finish(&self) -> Result<()> {
        self.append(&ChatSessionEvent::SessionFinished {
            session_id: self.session_id.clone(),
        })
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

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn temp_root(name: &str) -> PathBuf {
        let id = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("agl-runtime-{name}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn generated_session_ids_are_unique_path_segments() {
        let first = AgentLibreSessionId::generate();
        let second = AgentLibreSessionId::generate();

        assert_ne!(first, second);
        AgentLibreSessionId::new(first.as_str()).unwrap();
        AgentLibreSessionId::new(second.as_str()).unwrap();
    }

    #[test]
    fn writes_chat_session_metadata_and_transcript() {
        let root = temp_root("session");
        let store = ChatSessionStore::start(
            &root,
            AgentLibreSessionId::new("session-001").unwrap(),
            "run-001",
            "/tmp/local.toml",
            "llama_cpp",
        )
        .unwrap();

        store
            .append_user_message(AgentLibreMessageId::indexed(1), "hello".to_string())
            .unwrap();
        store
            .append_assistant_message(AgentLibreMessageId::indexed(2), "hi".to_string())
            .unwrap();
        store.link_attempt("attempt-0001").unwrap();
        store.append_context_cleared().unwrap();
        store.finish().unwrap();

        assert!(store.session_dir().join("session.json").exists());
        let transcript = std::fs::read_to_string(store.transcript_jsonl()).unwrap();
        assert!(transcript.contains("\"kind\":\"session_started\""));
        assert!(transcript.contains("\"kind\":\"user_message\""));
        assert!(transcript.contains("\"kind\":\"assistant_message\""));
        assert!(transcript.contains("\"kind\":\"model_attempt_linked\""));
        assert!(transcript.contains("\"kind\":\"context_cleared\""));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn start_refuses_existing_chat_session() {
        let root = temp_root("session-collision");
        let session_id = AgentLibreSessionId::new("session-001").unwrap();
        let _store = ChatSessionStore::start(
            &root,
            session_id.clone(),
            "run-001",
            "/tmp/local.toml",
            "llama_cpp",
        )
        .unwrap();

        let err =
            ChatSessionStore::start(&root, session_id, "run-002", "/tmp/local.toml", "llama_cpp")
                .unwrap_err();

        assert!(format!("{err:#}").contains("chat session already exists"));

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opens_existing_session_and_reads_replay_without_appending_start() {
        let root = temp_root("session-replay");
        let session_id = AgentLibreSessionId::new("session-001").unwrap();
        let store = ChatSessionStore::start(
            &root,
            session_id.clone(),
            "run-001",
            "/tmp/local.toml",
            "llama_cpp",
        )
        .unwrap();
        store
            .append_user_message(AgentLibreMessageId::indexed(1), "hello".to_string())
            .unwrap();
        store.link_attempt("attempt-0001").unwrap();
        store
            .append_assistant_message(AgentLibreMessageId::indexed(2), "hi".to_string())
            .unwrap();
        let before = std::fs::read_to_string(store.transcript_jsonl()).unwrap();

        let opened = ChatSessionStore::open(&root, session_id, "run-002").unwrap();
        let replay = opened.read_replay().unwrap();
        let after = std::fs::read_to_string(opened.transcript_jsonl()).unwrap();

        assert_eq!(after, before);
        assert_eq!(replay.events.len(), 4);
        assert_eq!(replay.next_message_index, 3);
        assert_eq!(replay.next_attempt_index, 2);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_transcript_reports_line_number() {
        let root = temp_root("session-malformed");
        let session_id = AgentLibreSessionId::new("session-001").unwrap();
        let store = ChatSessionStore::start(
            &root,
            session_id.clone(),
            "run-001",
            "/tmp/local.toml",
            "llama_cpp",
        )
        .unwrap();
        let mut transcript = std::fs::read_to_string(store.transcript_jsonl()).unwrap();
        transcript.push_str("not-json\n");
        std::fs::write(store.transcript_jsonl(), transcript).unwrap();
        let opened = ChatSessionStore::open(&root, session_id, "run-002").unwrap();

        let err = opened.read_replay().unwrap_err();

        assert!(format!("{err:#}").contains("line 2"));

        std::fs::remove_dir_all(root).unwrap();
    }
}
