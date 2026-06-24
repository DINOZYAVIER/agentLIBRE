use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum BridgeConfigError {
    EmptyCommandPrefix,
    MissingAccessPolicy,
    MissingHomeserverUrl,
    MissingStorePathForEncryptedRooms,
    MissingUserId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeConfig {
    pub matrix: MatrixConfig,
    #[serde(default)]
    pub agl: AglConfig,
    #[serde(default)]
    pub access: crate::access::AccessPolicy,
    #[serde(default)]
    pub bindings: BindingConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatrixConfig {
    pub homeserver_url: String,
    pub user_id: String,
    #[serde(default)]
    pub access_token: Option<String>,
    #[serde(default)]
    pub device_id: Option<String>,
    #[serde(default)]
    pub session_path: Option<String>,
    #[serde(default)]
    pub store_path: Option<String>,
    #[serde(default = "default_command_prefix")]
    pub command_prefix: String,
    #[serde(default)]
    pub normal_chat: bool,
    #[serde(default)]
    pub encrypted_rooms: EncryptedRoomPolicy,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AglConfig {
    #[serde(default)]
    pub socket_path: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingConfig {
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EncryptedRoomPolicy {
    #[default]
    Reject,
    AllowDecrypted,
}

fn default_command_prefix() -> String {
    "!agl".to_owned()
}

impl MatrixConfig {
    pub fn command_prefix(&self) -> &str {
        self.command_prefix.as_str()
    }
}

impl BridgeConfig {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path).map_err(|err| {
            anyhow::anyhow!("failed to read bridge config {}: {err}", path.display())
        })?;
        toml::from_str(&content).map_err(|err| {
            anyhow::anyhow!("failed to parse bridge config {}: {err}", path.display())
        })
    }

    pub fn validate(&self) -> Result<(), BridgeConfigError> {
        if self.matrix.homeserver_url.trim().is_empty() {
            return Err(BridgeConfigError::MissingHomeserverUrl);
        }
        if self.matrix.user_id.trim().is_empty() {
            return Err(BridgeConfigError::MissingUserId);
        }
        if self.matrix.command_prefix.trim().is_empty() {
            return Err(BridgeConfigError::EmptyCommandPrefix);
        }
        if self.matrix.encrypted_rooms == EncryptedRoomPolicy::AllowDecrypted
            && self
                .matrix
                .store_path
                .as_deref()
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .is_none()
        {
            return Err(BridgeConfigError::MissingStorePathForEncryptedRooms);
        }
        if self.access.allowed_rooms.is_empty() && self.access.allowed_users.is_empty() {
            return Err(BridgeConfigError::MissingAccessPolicy);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_config_exposes_default_prefix() {
        let config = MatrixConfig {
            homeserver_url: "https://matrix.example".to_owned(),
            user_id: "@agent:example".to_owned(),
            access_token: Some("token".to_owned()),
            device_id: None,
            session_path: None,
            store_path: None,
            command_prefix: default_command_prefix(),
            normal_chat: false,
            encrypted_rooms: EncryptedRoomPolicy::Reject,
        };

        assert_eq!(config.command_prefix(), "!agl");
    }

    #[test]
    fn bridge_config_defaults_to_rejecting_ambient_chat_and_encryption() {
        let config = BridgeConfig {
            matrix: MatrixConfig {
                homeserver_url: "https://matrix.example".to_owned(),
                user_id: "@agent:example".to_owned(),
                access_token: Some("token".to_owned()),
                device_id: None,
                session_path: None,
                store_path: None,
                command_prefix: default_command_prefix(),
                normal_chat: false,
                encrypted_rooms: EncryptedRoomPolicy::Reject,
            },
            agl: AglConfig::default(),
            access: crate::access::AccessPolicy {
                allowed_rooms: vec!["!room:example".to_owned()],
                allowed_users: vec![],
            },
            bindings: BindingConfig::default(),
        };

        assert!(!config.matrix.normal_chat);
        assert_eq!(config.matrix.encrypted_rooms, EncryptedRoomPolicy::Reject);
        assert_eq!(config.matrix.command_prefix(), "!agl");
    }

    #[test]
    fn encrypted_room_allow_policy_requires_store_path() {
        let config = BridgeConfig {
            matrix: MatrixConfig {
                homeserver_url: "https://matrix.example".to_owned(),
                user_id: "@agent:example".to_owned(),
                access_token: Some("token".to_owned()),
                device_id: None,
                session_path: None,
                store_path: None,
                command_prefix: default_command_prefix(),
                normal_chat: false,
                encrypted_rooms: EncryptedRoomPolicy::AllowDecrypted,
            },
            agl: AglConfig::default(),
            access: crate::access::AccessPolicy::default(),
            bindings: BindingConfig::default(),
        };

        assert_eq!(
            config.validate(),
            Err(BridgeConfigError::MissingStorePathForEncryptedRooms)
        );
    }
}
