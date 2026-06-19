use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptConfig {
    #[serde(default)]
    pub system: SystemPrompt,
}

impl PromptConfig {
    pub fn validate(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SystemPrompt {
    #[default]
    #[serde(rename = "builtin:default")]
    BuiltinDefault,
    #[serde(rename = "none")]
    None,
}
