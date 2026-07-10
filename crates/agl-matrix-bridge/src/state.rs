use std::collections::BTreeSet;
use std::path::Path;

use agl_ids::SessionId;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{BindingKey, BridgeProcessedEvents, ThreadBinding, ThreadBindingStore};

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeState {
    #[serde(default)]
    pub bindings: Vec<ThreadBinding>,
    #[serde(default)]
    pub processed_event_ids: BTreeSet<String>,
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
        serde_json::from_str(&content)
            .with_context(|| format!("failed to parse bridge state {}", path.display()))
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create bridge state dir {}", parent.display())
            })?;
        }
        let bytes = serde_json::to_vec_pretty(self).context("failed to serialize bridge state")?;
        std::fs::write(path, bytes)
            .with_context(|| format!("failed to write bridge state {}", path.display()))
    }

    pub fn from_parts(bindings: &ThreadBindingStore, processed: &BridgeProcessedEvents) -> Self {
        Self {
            bindings: bindings.bindings().collect(),
            processed_event_ids: processed.iter().cloned().collect(),
        }
    }

    pub fn into_parts(self) -> (ThreadBindingStore, BridgeProcessedEvents) {
        let mut bindings = ThreadBindingStore::default();
        for binding in self.bindings {
            bindings.bind(binding.key, binding.session_id);
        }
        let mut processed = BridgeProcessedEvents::default();
        for event_id in self.processed_event_ids {
            processed.mark(event_id);
        }
        (bindings, processed)
    }

    pub fn bind(&mut self, key: BindingKey, session_id: SessionId) {
        if let Some(binding) = self.bindings.iter_mut().find(|binding| binding.key == key) {
            binding.session_id = session_id;
        } else {
            self.bindings.push(ThreadBinding { key, session_id });
        }
        self.bindings
            .sort_by(|left, right| left.key.cmp(&right.key));
    }

    pub fn unbind(&mut self, key: &BindingKey) -> Option<SessionId> {
        self.bindings
            .iter()
            .position(|binding| &binding.key == key)
            .map(|index| self.bindings.remove(index).session_id)
    }

    pub fn mark_processed(&mut self, event_id: impl Into<String>) {
        self.processed_event_ids.insert(event_id.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_ID: &str = "ses_01890f17-4a00-7000-8000-000000000001";

    #[test]
    fn state_round_trips_bindings_and_processed_events() {
        let path = std::env::temp_dir().join(format!(
            "agl-matrix-bridge-state-{}-{}.json",
            std::process::id(),
            "round-trip"
        ));
        let _ = std::fs::remove_file(&path);
        let mut state = BridgeState::default();
        state.bind(
            BindingKey::new("!room:example", Some("$thread".to_string())),
            SessionId::parse(SESSION_ID).unwrap(),
        );
        state.mark_processed("$event");

        state.save(&path).unwrap();
        let loaded = BridgeState::load(&path).unwrap();

        assert_eq!(loaded, state);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn state_rejects_non_canonical_session_id() {
        let json = r#"{
            "bindings": [{
                "key": { "room_id": "!room:example" },
                "session_id": "session-1"
            }]
        }"#;

        assert!(serde_json::from_str::<BridgeState>(json).is_err());
    }

    #[test]
    fn missing_state_file_loads_empty() {
        let path = std::env::temp_dir().join(format!(
            "agl-matrix-bridge-state-{}-missing.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let loaded = BridgeState::load(path).unwrap();

        assert_eq!(loaded, BridgeState::default());
    }
}
