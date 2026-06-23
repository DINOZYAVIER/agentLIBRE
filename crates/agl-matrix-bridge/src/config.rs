use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeConfig {
    pub matrix: MatrixConfig,
    #[serde(default)]
    pub access: crate::access::AccessPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatrixConfig {
    pub homeserver_url: String,
    pub user_id: String,
    pub access_token: String,
    #[serde(default = "default_command_prefix")]
    pub command_prefix: String,
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
        };

        assert_eq!(config.command_prefix(), "!agl");
    }
}
