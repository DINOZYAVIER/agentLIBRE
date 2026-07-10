use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use agl_ids::{EventId, RunId};
use anyhow::{Context, Result, anyhow, bail};

use crate::{
    EVENT_SCHEMA, EventDraft, EventEnvelope, RuntimeEvent, RuntimeEventEnvelope, SafeRuntimeEvent,
    SafeRuntimeEventEnvelope,
};

pub trait EventAppender<P> {
    type StoredPayload;

    fn append(&self, draft: EventDraft<P>) -> Result<EventEnvelope<Self::StoredPayload>>;
}

#[derive(Clone, Debug)]
pub struct RuntimeEventWriter {
    events_jsonl: PathBuf,
    state: Arc<Mutex<WriterState>>,
}

impl RuntimeEventWriter {
    pub fn open(events_jsonl: impl Into<PathBuf>) -> Result<Self> {
        let events_jsonl = canonical_event_path(events_jsonl.into())?;
        let mut writers = writer_registry()
            .lock()
            .map_err(|_| anyhow!("runtime event writer registry is poisoned"))?;
        writers.retain(|_, state| state.strong_count() != 0);

        if let Some(state) = writers.get(&events_jsonl).and_then(Weak::upgrade) {
            return Ok(Self {
                events_jsonl,
                state,
            });
        }

        let state = Arc::new(Mutex::new(WriterState::recover(&events_jsonl)?));
        writers.insert(events_jsonl.clone(), Arc::downgrade(&state));
        Ok(Self {
            events_jsonl,
            state,
        })
    }

    pub fn path(&self) -> &Path {
        &self.events_jsonl
    }

    pub fn append(
        &self,
        draft: EventDraft<RuntimeEvent>,
    ) -> Result<EventEnvelope<SafeRuntimeEvent>> {
        <Self as EventAppender<RuntimeEvent>>::append(self, draft)
    }

    pub fn append_with_full(
        &self,
        draft: EventDraft<RuntimeEvent>,
    ) -> Result<(RuntimeEventEnvelope, SafeRuntimeEventEnvelope)> {
        self.append_pair(draft)
    }

    fn append_pair(
        &self,
        draft: EventDraft<RuntimeEvent>,
    ) -> Result<(RuntimeEventEnvelope, SafeRuntimeEventEnvelope)> {
        append_runtime_event(self, draft)
    }
}

impl EventAppender<RuntimeEvent> for RuntimeEventWriter {
    type StoredPayload = SafeRuntimeEvent;

    fn append(&self, draft: EventDraft<RuntimeEvent>) -> Result<SafeRuntimeEventEnvelope> {
        self.append_pair(draft).map(|(_, safe)| safe)
    }
}

fn append_runtime_event(
    writer: &RuntimeEventWriter,
    draft: EventDraft<RuntimeEvent>,
) -> Result<(RuntimeEventEnvelope, SafeRuntimeEventEnvelope)> {
    let mut state = writer
        .state
        .lock()
        .map_err(|_| anyhow!("runtime event writer state is poisoned"))?;
    if state.failed {
        bail!(
            "runtime event writer for {} previously failed; reopen it after inspecting the stream",
            writer.events_jsonl.display()
        );
    }

    let actual_len = file_len(&writer.events_jsonl)?;
    if actual_len != state.file_len {
        state.failed = true;
        bail!(
            "runtime event stream {} changed outside this writer (expected {} bytes, found {})",
            writer.events_jsonl.display(),
            state.file_len,
            actual_len
        );
    }

    let run_id = draft.scope.run_id().clone();
    let sequence = state
        .sequences
        .get(&run_id)
        .copied()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| anyhow!("event sequence overflow for run {run_id}"))?;
    let event_id = next_event_id(&state.event_ids);
    let occurred_at_unix_ms = unix_millis()?;

    let envelope = EventEnvelope {
        schema: EVENT_SCHEMA.to_string(),
        event_id: event_id.clone(),
        sequence,
        occurred_at_unix_ms,
        scope: draft.scope,
        request_id: draft.request_id,
        caused_by: draft.caused_by,
        payload: draft.payload,
    };
    let safe_envelope = envelope.clone().into_redacted();

    let mut encoded = serde_json::to_vec(&safe_envelope)
        .context("failed to serialize safe runtime event envelope")?;
    encoded.push(b'\n');

    if let Err(error) = append_line(&writer.events_jsonl, &encoded) {
        state.failed = true;
        return Err(error);
    }

    let expected_len = state
        .file_len
        .checked_add(encoded.len() as u64)
        .ok_or_else(|| anyhow!("runtime event stream length overflow"))?;
    let actual_len = file_len(&writer.events_jsonl)?;
    if actual_len != expected_len {
        state.failed = true;
        bail!(
            "runtime event stream {} changed during append (expected {} bytes, found {})",
            writer.events_jsonl.display(),
            expected_len,
            actual_len
        );
    }

    state.file_len = expected_len;
    state.sequences.insert(run_id, sequence);
    state.event_ids.insert(event_id);
    Ok((envelope, safe_envelope))
}

#[derive(Debug)]
struct WriterState {
    sequences: HashMap<RunId, u64>,
    event_ids: HashSet<EventId>,
    file_len: u64,
    failed: bool,
}

impl WriterState {
    fn recover(path: &Path) -> Result<Self> {
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read runtime event stream {}", path.display())
                });
            }
        };

        if !content.is_empty() && !content.ends_with('\n') {
            bail!(
                "runtime event stream {} ends with an incomplete JSONL record",
                path.display()
            );
        }

        let mut state = Self {
            sequences: HashMap::new(),
            event_ids: HashSet::new(),
            file_len: content.len() as u64,
            failed: false,
        };

        for (line_index, line) in content.lines().enumerate() {
            if line.is_empty() {
                bail!(
                    "runtime event stream {} contains an empty record at line {}",
                    path.display(),
                    line_index + 1
                );
            }
            let envelope: SafeRuntimeEventEnvelope =
                serde_json::from_str(line).with_context(|| {
                    format!(
                        "failed to parse runtime event stream {} line {}",
                        path.display(),
                        line_index + 1
                    )
                })?;

            if !state.event_ids.insert(envelope.event_id.clone()) {
                bail!(
                    "runtime event stream {} contains duplicate event ID {}",
                    path.display(),
                    envelope.event_id
                );
            }

            let run_id = envelope.scope.run_id().clone();
            let expected = state
                .sequences
                .get(&run_id)
                .copied()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| anyhow!("event sequence overflow for run {run_id}"))?;
            if envelope.sequence != expected {
                bail!(
                    "runtime event stream {} has sequence {} for run {}; expected {}",
                    path.display(),
                    envelope.sequence,
                    run_id,
                    expected
                );
            }
            state.sequences.insert(run_id, envelope.sequence);
        }

        Ok(state)
    }
}

type SharedWriterState = Weak<Mutex<WriterState>>;

fn writer_registry() -> &'static Mutex<HashMap<PathBuf, SharedWriterState>> {
    static WRITERS: OnceLock<Mutex<HashMap<PathBuf, SharedWriterState>>> = OnceLock::new();
    WRITERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn canonical_event_path(path: PathBuf) -> Result<PathBuf> {
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("runtime event stream path must name a file"))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create runtime event directory {}",
            parent.display()
        )
    })?;
    let parent = parent.canonicalize().with_context(|| {
        format!(
            "failed to resolve runtime event directory {}",
            parent.display()
        )
    })?;
    Ok(parent.join(file_name))
}

fn append_line(path: &Path, encoded: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open runtime event stream {}", path.display()))?;
    file.write_all(encoded)
        .with_context(|| format!("failed to write runtime event stream {}", path.display()))?;
    file.flush()
        .with_context(|| format!("failed to flush runtime event stream {}", path.display()))
}

fn file_len(path: &Path) -> Result<u64> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect runtime event stream {}", path.display())),
    }
}

fn next_event_id(existing: &HashSet<EventId>) -> EventId {
    loop {
        let event_id = EventId::generate();
        if !existing.contains(&event_id) {
            return event_id;
        }
    }
}

fn unix_millis() -> Result<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_millis();
    u64::try_from(millis).context("Unix millisecond timestamp does not fit in u64")
}
