use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct AgentLibreSessionId(String);

impl AgentLibreSessionId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        ensure_path_segment(&value, "session_id")?;
        Ok(Self(value))
    }

    pub fn generate() -> Self {
        Self(format!("session-{}", unix_millis()))
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
    pub fn start(
        sessions_root: impl AsRef<Path>,
        session_id: AgentLibreSessionId,
        run_id: impl Into<String>,
        model_config_path: impl Into<PathBuf>,
        backend: impl Into<String>,
    ) -> Result<Self> {
        let run_id = run_id.into();
        ensure_path_segment(&run_id, "run_id")?;
        let session_dir = sessions_root.as_ref().join(session_id.as_str());
        let transcript_jsonl = session_dir.join("transcript.jsonl");
        std::fs::create_dir_all(&session_dir).with_context(|| {
            format!(
                "failed to create chat session directory {}",
                session_dir.display()
            )
        })?;

        let metadata = SessionMetadata {
            session_id: session_id.clone(),
            created_at_unix_ms: unix_millis(),
            updated_at_unix_ms: unix_millis(),
            model_config_path: model_config_path.into(),
            backend: backend.into(),
        };
        write_json(&session_dir.join("session.json"), &metadata)?;

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

    pub fn session_id(&self) -> &AgentLibreSessionId {
        &self.session_id
    }

    pub fn session_dir(&self) -> &Path {
        &self.session_dir
    }

    pub fn transcript_jsonl(&self) -> &Path {
        &self.transcript_jsonl
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

fn write_json<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize + ?Sized,
{
    let bytes = serde_json::to_vec_pretty(value)
        .with_context(|| format!("failed to serialize JSON {}", path.display()))?;
    std::fs::write(path, bytes).with_context(|| format!("failed to write {}", path.display()))
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
        store.finish().unwrap();

        assert!(store.session_dir().join("session.json").exists());
        let transcript = std::fs::read_to_string(store.transcript_jsonl()).unwrap();
        assert!(transcript.contains("\"kind\":\"session_started\""));
        assert!(transcript.contains("\"kind\":\"user_message\""));
        assert!(transcript.contains("\"kind\":\"assistant_message\""));
        assert!(transcript.contains("\"kind\":\"model_attempt_linked\""));

        std::fs::remove_dir_all(root).unwrap();
    }
}
