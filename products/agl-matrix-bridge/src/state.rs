use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

use agl_core::{ConversationId, MessageId};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const REPLAY_JOURNAL_LIMIT: usize = 4_096;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingKey {
    pub room_id: String,
    pub thread_root_event_id: String,
}

impl BindingKey {
    pub fn new(room_id: impl Into<String>, thread_root_event_id: impl Into<String>) -> Self {
        Self {
            room_id: room_id.into(),
            thread_root_event_id: thread_root_event_id.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadBinding {
    pub key: BindingKey,
    pub conversation_id: ConversationId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeReplayEntry {
    pub event_id: String,
    pub message_id: MessageId,
    pub processed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplayReservation {
    Reserved(MessageId),
    AlreadyProcessed,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeState {
    pub bindings: Vec<ThreadBinding>,
    pub replay: Vec<BridgeReplayEntry>,
}

impl BridgeState {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = match std::fs::read_to_string(path) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to read bridge state {}", path.display()));
            }
        };
        let state: Self = serde_json::from_str(&content)
            .with_context(|| format!("failed to parse bridge state {}", path.display()))?;
        state
            .validate()
            .with_context(|| format!("invalid bridge state {}", path.display()))?;
        Ok(state)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        self.validate().context("invalid bridge state")?;
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create bridge state dir {}", parent.display())
            })?;
        }
        let bytes = serde_json::to_vec_pretty(self).context("failed to serialize bridge state")?;
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .context("bridge state path has no UTF-8 file name")?;
        let staged = path.with_file_name(format!(".{name}.{}.tmp", MessageId::generate()));
        let result = (|| -> Result<()> {
            let mut options = std::fs::OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&staged)
                .with_context(|| format!("failed to stage bridge state {}", staged.display()))?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&staged, path)
                .with_context(|| format!("failed to publish bridge state {}", path.display()))?;
            std::fs::File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&staged);
        }
        result
    }

    pub fn conversation_for(&self, key: &BindingKey) -> Option<ConversationId> {
        self.bindings
            .iter()
            .find(|binding| &binding.key == key)
            .map(|binding| binding.conversation_id)
    }

    pub fn reserve_event(&mut self, event_id: &str) -> Result<ReplayReservation> {
        if let Some(entry) = self.replay.iter().find(|entry| entry.event_id == event_id) {
            return Ok(if entry.processed {
                ReplayReservation::AlreadyProcessed
            } else {
                ReplayReservation::Reserved(entry.message_id.clone())
            });
        }

        if self.replay.len() == REPLAY_JOURNAL_LIMIT {
            let Some(index) = self.replay.iter().position(|entry| entry.processed) else {
                bail!(
                    "Matrix replay journal is full with {REPLAY_JOURNAL_LIMIT} unprocessed reservations"
                );
            };
            self.replay.remove(index);
        }

        let message_id = MessageId::generate();
        self.replay.push(BridgeReplayEntry {
            event_id: event_id.to_owned(),
            message_id: message_id.clone(),
            processed: false,
        });
        Ok(ReplayReservation::Reserved(message_id))
    }

    fn validate(&self) -> Result<()> {
        if self.replay.len() > REPLAY_JOURNAL_LIMIT {
            bail!(
                "Matrix replay journal has {} entries; maximum is {REPLAY_JOURNAL_LIMIT}",
                self.replay.len()
            );
        }

        let mut binding_keys = BTreeSet::new();
        for binding in &self.bindings {
            if !binding_keys.insert(&binding.key) {
                bail!(
                    "duplicate Matrix thread binding for room {} and root {}",
                    binding.key.room_id,
                    binding.key.thread_root_event_id
                );
            }
        }

        let mut event_ids = BTreeSet::new();
        for entry in &self.replay {
            if !event_ids.insert(entry.event_id.as_str()) {
                bail!("duplicate Matrix replay event {}", entry.event_id);
            }
        }
        Ok(())
    }

    pub fn mark_processed(&mut self, event_id: &str) -> Result<()> {
        let Some(entry) = self
            .replay
            .iter_mut()
            .find(|entry| entry.event_id == event_id)
        else {
            bail!("Matrix event {event_id} has no replay reservation");
        };
        entry.processed = true;
        Ok(())
    }

    pub fn bind(&mut self, key: BindingKey, conversation_id: ConversationId) {
        if let Some(binding) = self.bindings.iter_mut().find(|binding| binding.key == key) {
            binding.conversation_id = conversation_id;
        } else {
            self.bindings.push(ThreadBinding {
                key,
                conversation_id,
            });
        }
        self.bindings
            .sort_by(|left, right| left.key.cmp(&right.key));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONVERSATION_ID: &str = "conv_01890f17-4a00-7000-8000-000000000001";

    #[test]
    fn state_round_trips_thread_binding_and_replay_entry() {
        let path = std::env::temp_dir().join(format!(
            "agl-matrix-bridge-state-{}-round-trip.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut state = BridgeState::default();
        state.bind(
            BindingKey::new("!room:example", "$thread"),
            ConversationId::parse(CONVERSATION_ID).unwrap(),
        );
        let reservation = state.reserve_event("$event").unwrap();
        assert!(matches!(reservation, ReplayReservation::Reserved(_)));
        state.mark_processed("$event").unwrap();

        state.save(&path).unwrap();
        let loaded = BridgeState::load(&path).unwrap();

        assert_eq!(loaded, state);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn retry_reuses_message_id_and_completed_event_is_ignored() {
        let mut state = BridgeState::default();
        let ReplayReservation::Reserved(first) = state.reserve_event("$event").unwrap() else {
            panic!("first event must reserve an origin");
        };
        let ReplayReservation::Reserved(retry) = state.reserve_event("$event").unwrap() else {
            panic!("unprocessed retry must reserve the existing origin");
        };
        assert_eq!(retry, first);

        state.mark_processed("$event").unwrap();
        assert_eq!(
            state.reserve_event("$event").unwrap(),
            ReplayReservation::AlreadyProcessed
        );
    }

    #[test]
    fn replay_journal_evicts_oldest_processed_entry_at_exact_bound() {
        let mut state = BridgeState::default();
        for index in 0..REPLAY_JOURNAL_LIMIT {
            let event_id = format!("$event-{index}");
            state.reserve_event(&event_id).unwrap();
            state.mark_processed(&event_id).unwrap();
        }

        state.reserve_event("$new").unwrap();

        assert_eq!(state.replay.len(), REPLAY_JOURNAL_LIMIT);
        assert!(
            !state
                .replay
                .iter()
                .any(|entry| entry.event_id == "$event-0")
        );
        assert_eq!(state.replay.last().unwrap().event_id, "$new");
    }

    #[test]
    fn replay_journal_never_discards_unprocessed_reservation() {
        let mut state = BridgeState::default();
        for index in 0..REPLAY_JOURNAL_LIMIT {
            state.reserve_event(&format!("$event-{index}")).unwrap();
        }

        let error = state.reserve_event("$overflow").unwrap_err();

        assert!(error.to_string().contains("unprocessed reservations"));
        assert_eq!(state.replay.len(), REPLAY_JOURNAL_LIMIT);
    }

    #[test]
    fn persisted_state_rejects_entries_beyond_the_replay_bound() {
        let path = std::env::temp_dir().join(format!(
            "agl-matrix-bridge-state-{}-oversized.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut state = BridgeState::default();
        for index in 0..=REPLAY_JOURNAL_LIMIT {
            state.replay.push(BridgeReplayEntry {
                event_id: format!("$event-{index}"),
                message_id: MessageId::generate(),
                processed: true,
            });
        }

        let error = state.save(&path).unwrap_err();

        assert!(error.to_string().contains("invalid bridge state"));
        assert!(!path.exists());
    }

    #[test]
    fn missing_state_file_loads_empty() {
        let path = std::env::temp_dir().join(format!(
            "agl-matrix-bridge-state-{}-missing.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        assert_eq!(BridgeState::load(path).unwrap(), BridgeState::default());
    }
}
