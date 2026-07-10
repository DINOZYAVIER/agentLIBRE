use std::collections::BTreeMap;

use agl_ids::SessionId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingKey {
    pub room_id: String,
    #[serde(default)]
    pub thread_root_event_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadBinding {
    pub key: BindingKey,
    pub session_id: SessionId,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadBindingStore {
    bindings: BTreeMap<BindingKey, SessionId>,
}

impl BindingKey {
    pub fn new(room_id: impl Into<String>, thread_root_event_id: Option<String>) -> Self {
        Self {
            room_id: room_id.into(),
            thread_root_event_id,
        }
    }
}

impl ThreadBindingStore {
    pub fn bind(&mut self, key: BindingKey, session_id: SessionId) -> Option<SessionId> {
        self.bindings.insert(key, session_id)
    }

    pub fn unbind(&mut self, key: &BindingKey) -> Option<SessionId> {
        self.bindings.remove(key)
    }

    pub fn session_for(&self, key: &BindingKey) -> Option<&SessionId> {
        self.bindings.get(key)
    }

    pub fn bindings(&self) -> impl Iterator<Item = ThreadBinding> + '_ {
        self.bindings.iter().map(|(key, session_id)| ThreadBinding {
            key: key.clone(),
            session_id: session_id.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_ID: &str = "ses_01890f17-4a00-7000-8000-000000000001";

    fn session_id() -> SessionId {
        SessionId::parse(SESSION_ID).unwrap()
    }

    #[test]
    fn binds_matrix_thread_to_session() {
        let key = BindingKey::new("!room:example", Some("$thread".to_owned()));
        let mut store = ThreadBindingStore::default();

        assert_eq!(store.bind(key.clone(), session_id()), None);
        assert_eq!(store.session_for(&key), Some(&session_id()));
    }

    #[test]
    fn unbinds_matrix_thread_from_session() {
        let key = BindingKey::new("!room:example", Some("$thread".to_owned()));
        let mut store = ThreadBindingStore::default();
        store.bind(key.clone(), session_id());

        assert_eq!(store.unbind(&key), Some(session_id()));
        assert_eq!(store.session_for(&key), None);
    }
}
