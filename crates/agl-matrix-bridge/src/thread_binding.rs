use std::collections::BTreeMap;

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
    pub session_id: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadBindingStore {
    bindings: BTreeMap<BindingKey, String>,
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
    pub fn bind(&mut self, key: BindingKey, session_id: impl Into<String>) -> Option<String> {
        self.bindings.insert(key, session_id.into())
    }

    pub fn unbind(&mut self, key: &BindingKey) -> Option<String> {
        self.bindings.remove(key)
    }

    pub fn session_for(&self, key: &BindingKey) -> Option<&str> {
        self.bindings.get(key).map(String::as_str)
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

    #[test]
    fn binds_matrix_thread_to_session() {
        let key = BindingKey::new("!room:example", Some("$thread".to_owned()));
        let mut store = ThreadBindingStore::default();

        assert_eq!(store.bind(key.clone(), "session-1"), None);
        assert_eq!(store.session_for(&key), Some("session-1"));
    }

    #[test]
    fn unbinds_matrix_thread_from_session() {
        let key = BindingKey::new("!room:example", Some("$thread".to_owned()));
        let mut store = ThreadBindingStore::default();
        store.bind(key.clone(), "session-1");

        assert_eq!(store.unbind(&key), Some("session-1".to_owned()));
        assert_eq!(store.session_for(&key), None);
    }
}
