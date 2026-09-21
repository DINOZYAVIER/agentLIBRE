use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum BridgeConfigError {
    InvalidFunctionPath,
    InvalidWorkspacePath,
    MissingAccessPolicy,
    MissingFunctionPath,
    MissingHomeserverUrl,
    MissingStorePathForEncryptedRooms,
    MissingUserId,
    MissingWorkspacePath,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeConfig {
    pub matrix: MatrixConfig,
    pub agl: AglConfig,
    #[serde(default)]
    pub verification: VerificationConfig,
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
    #[serde(default)]
    pub encrypted_rooms: EncryptedRoomPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AglConfig {
    #[serde(default)]
    pub socket_path: Option<String>,
    pub function_path: String,
    pub workspace_path: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationConfig {
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub device_id: Option<String>,
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
        if self.agl.function_path.trim().is_empty() {
            return Err(BridgeConfigError::MissingFunctionPath);
        }
        if self.agl.workspace_path.trim().is_empty() {
            return Err(BridgeConfigError::MissingWorkspacePath);
        }
        if !Path::new(&self.agl.function_path).is_absolute() {
            return Err(BridgeConfigError::InvalidFunctionPath);
        }
        if !Path::new(&self.agl.workspace_path).is_absolute() {
            return Err(BridgeConfigError::InvalidWorkspacePath);
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
    fn bridge_config_defaults_to_rejecting_encrypted_rooms() {
        let config = BridgeConfig {
            matrix: MatrixConfig {
                homeserver_url: "https://matrix.example".to_owned(),
                user_id: "@agent:example".to_owned(),
                access_token: Some("token".to_owned()),
                device_id: None,
                session_path: None,
                store_path: None,
                encrypted_rooms: EncryptedRoomPolicy::Reject,
            },
            agl: AglConfig {
                socket_path: None,
                function_path: "/functions/matrix".into(),
                workspace_path: "/workspace".into(),
            },
            verification: VerificationConfig::default(),
            access: crate::access::AccessPolicy {
                allowed_rooms: vec!["!room:example".to_owned()],
                allowed_users: vec![],
            },
            bindings: BindingConfig::default(),
        };

        assert_eq!(config.matrix.encrypted_rooms, EncryptedRoomPolicy::Reject);
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
                encrypted_rooms: EncryptedRoomPolicy::AllowDecrypted,
            },
            agl: AglConfig {
                socket_path: None,
                function_path: "/functions/matrix".into(),
                workspace_path: "/workspace".into(),
            },
            verification: VerificationConfig::default(),
            access: crate::access::AccessPolicy::default(),
            bindings: BindingConfig::default(),
        };

        assert_eq!(
            config.validate(),
            Err(BridgeConfigError::MissingStorePathForEncryptedRooms)
        );
    }

    #[test]
    fn function_and_workspace_paths_must_be_absolute() {
        let mut config = BridgeConfig {
            matrix: MatrixConfig {
                homeserver_url: "https://matrix.example".to_owned(),
                user_id: "@agent:example".to_owned(),
                access_token: Some("token".to_owned()),
                device_id: None,
                session_path: None,
                store_path: None,
                encrypted_rooms: EncryptedRoomPolicy::Reject,
            },
            agl: AglConfig {
                socket_path: None,
                function_path: "functions/matrix".into(),
                workspace_path: "/workspace".into(),
            },
            verification: VerificationConfig::default(),
            access: crate::access::AccessPolicy {
                allowed_rooms: vec!["!room:example".to_owned()],
                allowed_users: vec![],
            },
            bindings: BindingConfig::default(),
        };
        assert_eq!(
            config.validate(),
            Err(BridgeConfigError::InvalidFunctionPath)
        );
        config.agl.function_path = "/functions/matrix".into();
        config.agl.workspace_path = "workspace".into();
        assert_eq!(
            config.validate(),
            Err(BridgeConfigError::InvalidWorkspacePath)
        );
    }
}
