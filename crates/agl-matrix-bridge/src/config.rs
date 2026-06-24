use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
pub struct MatrixConfig {
    pub homeserver_url: String,
    pub user_id: String,
    pub access_token: String,
    #[serde(default = "default_command_prefix")]
    pub command_prefix: String,
    #[serde(default)]
    pub normal_chat: bool,
    #[serde(default)]
    pub encrypted_rooms: EncryptedRoomPolicy,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AglConfig {
    #[serde(default)]
    pub socket_path: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_config_exposes_default_prefix() {
        let config = MatrixConfig {
            homeserver_url: "https://matrix.example".to_owned(),
            user_id: "@agent:example".to_owned(),
            access_token: "token".to_owned(),
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
                access_token: "token".to_owned(),
                command_prefix: default_command_prefix(),
                normal_chat: false,
                encrypted_rooms: EncryptedRoomPolicy::Reject,
            },
            agl: AglConfig::default(),
            access: crate::access::AccessPolicy::default(),
            bindings: BindingConfig::default(),
        };

        assert!(!config.matrix.normal_chat);
        assert_eq!(config.matrix.encrypted_rooms, EncryptedRoomPolicy::Reject);
        assert_eq!(config.matrix.command_prefix(), "!agl");
    }
}
