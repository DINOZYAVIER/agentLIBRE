use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agl_content::Content;
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
    pub execution_context: agl_process::ExecutionContextSnapshot,
    pub runtime_selection: SessionRuntimeSelection,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRuntimeSelection {
    pub function_ref: Option<String>,
    pub model_id: Option<String>,
    pub operation_mode: String,
    pub skill_ids: Vec<String>,
    pub revision: u64,
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

#[derive(Debug)]
pub struct ChatSessionReplayRecord {
    pub event: ChatSessionEvent,
    pub start_offset: u64,
    pub end_offset: u64,
    pub transcript_bytes: usize,
}

#[derive(Debug)]
pub enum ChatSessionReverseRead {
    Record(ChatSessionReplayRecord),
    ScanLimitReached,
    End,
}

#[derive(Debug)]
pub struct ChatSessionReverseReader {
    file: File,
    session_id: SessionId,
    transcript_jsonl: PathBuf,
    transcript_len: u64,
    next_offset: u64,
    max_record_bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionCatalogStatus {
    Active,
    Finished,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionCatalogEntry {
    pub metadata: SessionMetadata,
    pub status: SessionCatalogStatus,
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
    metadata: SessionMetadata,
    session_dir: PathBuf,
    transcript_jsonl: PathBuf,
    run_sequences: BTreeMap<RunId, u64>,
    event_ids: BTreeSet<agl_ids::EventId>,
}

const REVERSE_REPLAY_READ_CHUNK_BYTES: usize = 64 * 1024;

impl ChatSessionReverseReader {
    pub fn transcript_len(&self) -> u64 {
        self.transcript_len
    }

    pub fn next_offset(&self) -> u64 {
        self.next_offset
    }

    pub fn set_end_offset(&mut self, end_offset: u64) -> Result<()> {
        ensure!(
            end_offset <= self.transcript_len,
            "chat transcript reverse-read offset exceeds the captured transcript length"
        );
        if end_offset > 0 {
            self.file
                .seek(SeekFrom::Start(end_offset - 1))
                .with_context(|| {
                    format!(
                        "failed to seek chat transcript {}",
                        self.transcript_jsonl.display()
                    )
                })?;
            let mut delimiter = [0_u8; 1];
            self.file.read_exact(&mut delimiter).with_context(|| {
                format!(
                    "failed to read chat transcript {}",
                    self.transcript_jsonl.display()
                )
            })?;
            ensure!(
                delimiter[0] == b'\n',
                "chat transcript reverse-read offset is not a JSONL record boundary"
            );
        }
        self.next_offset = end_offset;
        Ok(())
    }

    pub fn next_record(&mut self, scan_limit_bytes: usize) -> Result<ChatSessionReverseRead> {
        if self.next_offset == 0 {
            return Ok(ChatSessionReverseRead::End);
        }
        if scan_limit_bytes == 0 {
            return Ok(ChatSessionReverseRead::ScanLimitReached);
        }

        let record_end = self.next_offset;
        let line_end = record_end - 1;
        let max_record_search = self.max_record_bytes.saturating_add(1);
        let search_limit = scan_limit_bytes.min(max_record_search);
        let mut search_end = line_end;
        let mut searched_bytes = 0usize;
        let mut record_start = None;
        let mut chunk = [0_u8; REVERSE_REPLAY_READ_CHUNK_BYTES];

        while search_end > 0 && searched_bytes < search_limit {
            let remaining = search_limit - searched_bytes;
            let chunk_len = remaining
                .min(REVERSE_REPLAY_READ_CHUNK_BYTES)
                .min(usize::try_from(search_end).unwrap_or(usize::MAX));
            let chunk_start = search_end - u64::try_from(chunk_len).expect("chunk length fits u64");
            self.file
                .seek(SeekFrom::Start(chunk_start))
                .with_context(|| {
                    format!(
                        "failed to seek chat transcript {}",
                        self.transcript_jsonl.display()
                    )
                })?;
            self.file
                .read_exact(&mut chunk[..chunk_len])
                .with_context(|| {
                    format!(
                        "failed to read chat transcript {}",
                        self.transcript_jsonl.display()
                    )
                })?;
            searched_bytes += chunk_len;
            if let Some(index) = chunk[..chunk_len].iter().rposition(|byte| *byte == b'\n') {
                record_start =
                    Some(chunk_start + u64::try_from(index + 1).expect("chunk position fits u64"));
                break;
            }
            search_end = chunk_start;
        }

        let record_start = match record_start {
            Some(record_start) => record_start,
            None if search_end == 0 => 0,
            None if searched_bytes >= max_record_search => {
                bail!(
                    "chat transcript {} contains a record larger than the {}-byte reverse-read limit",
                    self.transcript_jsonl.display(),
                    self.max_record_bytes
                );
            }
            None => return Ok(ChatSessionReverseRead::ScanLimitReached),
        };
        let transcript_bytes = usize::try_from(record_end - record_start)
            .context("chat transcript reverse-read span does not fit memory limits")?;
        if transcript_bytes > scan_limit_bytes {
            return Ok(ChatSessionReverseRead::ScanLimitReached);
        }
        let line_bytes = usize::try_from(line_end - record_start)
            .context("chat transcript reverse-read record does not fit memory limits")?;
        ensure!(
            line_bytes > 0,
            "chat transcript {} contains an empty record at byte offset {}",
            self.transcript_jsonl.display(),
            record_start
        );
        ensure!(
            line_bytes <= self.max_record_bytes,
            "chat transcript {} contains a record larger than the {}-byte reverse-read limit",
            self.transcript_jsonl.display(),
            self.max_record_bytes
        );

        let mut encoded = vec![0_u8; line_bytes];
        self.file
            .seek(SeekFrom::Start(record_start))
            .with_context(|| {
                format!(
                    "failed to seek chat transcript {}",
                    self.transcript_jsonl.display()
                )
            })?;
        self.file.read_exact(&mut encoded).with_context(|| {
            format!(
                "failed to read chat transcript {}",
                self.transcript_jsonl.display()
            )
        })?;
        let event: ChatSessionEvent = serde_json::from_slice(&encoded).with_context(|| {
            format!(
                "failed to parse chat transcript {} record at byte offset {}",
                self.transcript_jsonl.display(),
                record_start
            )
        })?;
        validate_session_event_shape(&event, &self.session_id).with_context(|| {
            format!(
                "invalid chat transcript {} record at byte offset {}",
                self.transcript_jsonl.display(),
                record_start
            )
        })?;
        self.next_offset = record_start;
        Ok(ChatSessionReverseRead::Record(ChatSessionReplayRecord {
            event,
            start_offset: record_start,
            end_offset: record_end,
            transcript_bytes,
        }))
    }
}

impl ChatSessionStore {
    pub fn catalog(sessions_root: impl AsRef<Path>) -> Result<Vec<SessionCatalogEntry>> {
        let sessions_root = sessions_root.as_ref();
        let entries = match std::fs::read_dir(sessions_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error).context("failed to read chat sessions root"),
        };
        let mut catalog = Vec::new();
        for entry in entries {
            let entry = entry.context("failed to read chat session directory entry")?;
            let file_type = entry
                .file_type()
                .context("failed to inspect chat session directory entry")?;
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let directory_name = entry.file_name();
            let directory_name = directory_name.to_string_lossy();
            if SessionId::parse(&directory_name).is_err() {
                continue;
            }
            let metadata_path = entry.path().join("session.json");
            if !metadata_path.is_file() {
                continue;
            }
            let metadata: SessionMetadata = serde_json::from_slice(
                &std::fs::read(&metadata_path)
                    .with_context(|| format!("failed to read {}", metadata_path.display()))?,
            )
            .with_context(|| format!("failed to parse {}", metadata_path.display()))?;
            ensure!(
                directory_name == metadata.session_id.as_str(),
                "chat session catalog directory does not match metadata identity"
            );
            let status = catalog_status(&entry.path().join("transcript.jsonl"))?;
            catalog.push(SessionCatalogEntry { metadata, status });
        }
        catalog.sort_by(|left, right| {
            left.metadata
                .updated_at_unix_ms
                .cmp(&right.metadata.updated_at_unix_ms)
                .then_with(|| left.metadata.session_id.cmp(&right.metadata.session_id))
        });
        Ok(catalog)
    }

    pub fn exists(sessions_root: impl AsRef<Path>, session_id: &SessionId) -> bool {
        sessions_root
            .as_ref()
            .join(session_id.as_str())
            .join("session.json")
            .exists()
    }

    pub fn open_reverse_replay(
        sessions_root: impl AsRef<Path>,
        session_id: SessionId,
        max_record_bytes: usize,
    ) -> Result<ChatSessionReverseReader> {
        ensure!(
            max_record_bytes > 0,
            "chat transcript reverse-read record limit must be positive"
        );
        let session_dir = sessions_root.as_ref().join(session_id.as_str());
        ensure!(
            session_dir.join("session.json").is_file(),
            "chat session metadata does not exist: {}",
            session_dir.join("session.json").display()
        );
        let transcript_jsonl = session_dir.join("transcript.jsonl");
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&transcript_jsonl).with_context(|| {
            format!(
                "failed to open chat transcript {}",
                transcript_jsonl.display()
            )
        })?;
        let transcript_len = file
            .metadata()
            .with_context(|| format!("failed to inspect {}", transcript_jsonl.display()))?
            .len();
        let mut reader = ChatSessionReverseReader {
            file,
            session_id,
            transcript_jsonl,
            transcript_len,
            next_offset: 0,
            max_record_bytes,
        };
        reader.set_end_offset(transcript_len).with_context(|| {
            format!(
                "chat transcript {} ends with an incomplete JSONL record",
                reader.transcript_jsonl.display()
            )
        })?;
        Ok(reader)
    }

    pub fn start(
        sessions_root: impl AsRef<Path>,
        session_id: SessionId,
        local_inference_config_path: impl Into<PathBuf>,
        backend: impl Into<String>,
        execution_context: agl_process::ExecutionContextSnapshot,
        runtime_selection: SessionRuntimeSelection,
    ) -> Result<Self> {
        execution_context
            .validate()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        validate_runtime_selection(&runtime_selection)?;
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
            execution_context,
            runtime_selection,
        };
        write_new_json(&session_dir.join("session.json"), &metadata)?;

        let mut store = Self {
            machine: ChatSessionMachine::new(session_id),
            metadata,
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
        metadata
            .execution_context
            .validate()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;

        let mut store = Self {
            machine: ChatSessionMachine::new(session_id),
            metadata,
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

    pub fn execution_context(&self) -> &agl_process::ExecutionContextSnapshot {
        &self.metadata.execution_context
    }

    pub fn runtime_selection(&self) -> &SessionRuntimeSelection {
        &self.metadata.runtime_selection
    }

    pub fn update_runtime_selection(
        &mut self,
        expected_revision: u64,
        mut next: SessionRuntimeSelection,
    ) -> Result<&SessionRuntimeSelection> {
        ensure!(
            self.metadata.runtime_selection.revision == expected_revision,
            "session runtime selection revision changed from expected {expected_revision} to {}",
            self.metadata.runtime_selection.revision
        );
        next.revision = expected_revision
            .checked_add(1)
            .context("runtime selection revision overflow")?;
        validate_runtime_selection(&next)?;
        let mut metadata = self.metadata.clone();
        metadata.updated_at_unix_ms = unix_millis();
        metadata.runtime_selection = next;
        replace_json(&self.session_dir.join("session.json"), &metadata)?;
        self.metadata = metadata;
        Ok(&self.metadata.runtime_selection)
    }

    pub fn compare_and_set_execution_context(
        &mut self,
        expected_revision: u64,
        next: agl_process::ExecutionContextSnapshot,
    ) -> Result<&agl_process::ExecutionContextSnapshot> {
        next.validate()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let current = &self.metadata.execution_context;
        ensure!(
            current.revision == expected_revision,
            "session execution context revision changed from expected {expected_revision} to {}",
            current.revision
        );
        ensure!(
            next.revision
                == expected_revision
                    .checked_add(1)
                    .context("execution context revision overflow")?,
            "next session execution context revision is not consecutive"
        );
        ensure!(
            next.workspace_root == current.workspace_root
                && next.private_execution_roots == current.private_execution_roots
                && next.shell == current.shell,
            "session workspace, private roots, and shell admission are immutable during cwd update"
        );
        let mut metadata = self.metadata.clone();
        metadata.updated_at_unix_ms = unix_millis();
        metadata.execution_context = next;
        replace_json(&self.session_dir.join("session.json"), &metadata)?;
        self.metadata = metadata;
        Ok(&self.metadata.execution_context)
    }

    pub fn compare_and_set_execution_context_at(
        sessions_root: impl AsRef<Path>,
        session_id: &SessionId,
        expected_revision: u64,
        next: agl_process::ExecutionContextSnapshot,
    ) -> Result<agl_process::ExecutionContextSnapshot> {
        next.validate()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let metadata_path = sessions_root
            .as_ref()
            .join(session_id.as_str())
            .join("session.json");
        let bytes = std::fs::read(&metadata_path)
            .with_context(|| format!("failed to read {}", metadata_path.display()))?;
        let mut metadata: SessionMetadata = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", metadata_path.display()))?;
        ensure!(
            &metadata.session_id == session_id,
            "chat session metadata ID {} does not match requested session {}",
            metadata.session_id,
            session_id
        );
        let current = &metadata.execution_context;
        ensure!(
            current.revision == expected_revision,
            "session execution context revision changed from expected {expected_revision} to {}",
            current.revision
        );
        ensure!(
            next.revision
                == expected_revision
                    .checked_add(1)
                    .context("execution context revision overflow")?,
            "next session execution context revision is not consecutive"
        );
        ensure!(
            next.workspace_root == current.workspace_root
                && next.private_execution_roots == current.private_execution_roots
                && next.shell == current.shell,
            "session workspace, private roots, and shell admission are immutable during cwd update"
        );
        metadata.updated_at_unix_ms = unix_millis();
        metadata.execution_context = next;
        replace_json(&metadata_path, &metadata)?;
        Ok(metadata.execution_context)
    }

    pub fn reload_execution_context(&mut self) -> Result<&agl_process::ExecutionContextSnapshot> {
        let metadata_path = self.session_dir.join("session.json");
        let bytes = std::fs::read(&metadata_path)
            .with_context(|| format!("failed to read {}", metadata_path.display()))?;
        let metadata: SessionMetadata = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", metadata_path.display()))?;
        ensure!(
            metadata.session_id == *self.session_id(),
            "chat session metadata ID changed while reloading execution context"
        );
        metadata
            .execution_context
            .validate()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        self.metadata = metadata;
        Ok(&self.metadata.execution_context)
    }

    pub fn reset_workspace_execution_context(
        &mut self,
        expected_revision: u64,
        next: agl_process::ExecutionContextSnapshot,
    ) -> Result<&agl_process::ExecutionContextSnapshot> {
        next.validate()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let current = &self.metadata.execution_context;
        ensure!(
            current.revision == expected_revision,
            "session execution context revision changed from expected {expected_revision} to {}",
            current.revision
        );
        ensure!(
            next.revision
                == expected_revision
                    .checked_add(1)
                    .context("execution context revision overflow")?,
            "next session execution context revision is not consecutive"
        );
        ensure!(
            next.working_directory == next.workspace_root
                && next.private_execution_roots.is_empty()
                && next.shell == current.shell,
            "workspace reset must select its root, clear private roots, and retain shell admission"
        );
        let mut metadata = self.metadata.clone();
        metadata.updated_at_unix_ms = unix_millis();
        metadata.execution_context = next;
        replace_json(&self.session_dir.join("session.json"), &metadata)?;
        self.metadata = metadata;
        Ok(&self.metadata.execution_context)
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

    pub fn complete_cancelled_turn(&mut self) -> Result<()> {
        self.apply(ChatSessionTransition::PromptForInput)?;
        Ok(())
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
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&self.transcript_jsonl).with_context(|| {
            format!(
                "failed to open chat transcript {}",
                self.transcript_jsonl.display()
            )
        })?;
        #[cfg(unix)]
        std::fs::set_permissions(
            &self.transcript_jsonl,
            std::os::unix::fs::PermissionsExt::from_mode(0o600),
        )?;
        let line = serde_json::to_string(event).context("failed to serialize chat event")?;
        file.write_all(line.as_bytes())
            .context("failed to write chat event")?;
        file.write_all(b"\n")
            .context("failed to write chat event newline")?;
        file.flush().context("failed to flush chat transcript")
    }
}

fn catalog_status(path: &Path) -> Result<SessionCatalogStatus> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SessionCatalogStatus::Active);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let mut status = SessionCatalogStatus::Active;
    for (index, line) in content.lines().enumerate() {
        let event: ChatSessionEvent = serde_json::from_str(line).with_context(|| {
            format!(
                "failed to parse chat session catalog {} line {}",
                path.display(),
                index + 1
            )
        })?;
        match event {
            ChatSessionEvent::SessionFinished { .. } => status = SessionCatalogStatus::Finished,
            ChatSessionEvent::SessionFailed { .. } => status = SessionCatalogStatus::Failed,
            _ => {}
        }
    }
    Ok(status)
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

fn assistant_message(envelope: &RuntimeEventEnvelope) -> Result<(MessageId, Content)> {
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
    validate_session_event_shape(event, session_id)?;
    match event {
        ChatSessionEvent::Runtime { envelope } => {
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
        ChatSessionEvent::SessionStarted { .. }
        | ChatSessionEvent::ContextCleared { .. }
        | ChatSessionEvent::SessionFinished { .. }
        | ChatSessionEvent::SessionFailed { .. } => {}
    }
    Ok(())
}

fn validate_session_event_shape(event: &ChatSessionEvent, session_id: &SessionId) -> Result<()> {
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
        .with_context(|| format!("failed to create chat session directory {}", path.display()))?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700))
        .with_context(|| {
            format!(
                "failed to restrict chat session directory {}",
                path.display()
            )
        })?;
    Ok(())
}

fn write_new_json<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize + ?Sized,
{
    let bytes = serde_json::to_vec_pretty(value)
        .with_context(|| format!("failed to serialize JSON {}", path.display()))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("failed to write {}", path.display()))
}

fn replace_json<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize + ?Sized,
{
    let bytes = serde_json::to_vec_pretty(value)
        .with_context(|| format!("failed to serialize JSON {}", path.display()))?;
    let temporary = path.with_extension(format!("json.tmp-{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("failed to create {}", temporary.display()))?;
    file.write_all(&bytes)
        .with_context(|| format!("failed to write {}", temporary.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| {
        format!(
            "failed to atomically replace {} from {}",
            path.display(),
            temporary.display()
        )
    })?;
    if let Some(parent) = path.parent() {
        let directory = std::fs::File::open(parent)
            .with_context(|| format!("failed to open {} for sync", parent.display()))?;
        directory
            .sync_all()
            .with_context(|| format!("failed to sync {}", parent.display()))?;
    }
    Ok(())
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn validate_runtime_selection(selection: &SessionRuntimeSelection) -> Result<()> {
    ensure!(
        selection.revision > 0,
        "runtime selection revision must be positive"
    );
    ensure!(
        matches!(
            selection.operation_mode.as_str(),
            "read-only" | "write" | "execute" | "approve" | "admin"
        ),
        "invalid session operation mode {}",
        selection.operation_mode
    );
    ensure!(selection.skill_ids.len() <= 256, "too many selected skills");
    for skill_id in &selection.skill_ids {
        ensure!(
            !skill_id.is_empty() && skill_id.len() <= 256 && skill_id.trim() == skill_id,
            "invalid selected skill ID"
        );
    }
    if let Some(model_id) = &selection.model_id {
        ensure!(
            !model_id.is_empty() && model_id.len() <= 256 && model_id.trim() == model_id,
            "invalid selected model ID"
        );
    }
    if let Some(function_ref) = &selection.function_ref {
        ensure!(
            !function_ref.is_empty()
                && function_ref.len() <= 1024
                && function_ref.trim() == function_ref,
            "invalid function reference"
        );
    }
    Ok(())
}
