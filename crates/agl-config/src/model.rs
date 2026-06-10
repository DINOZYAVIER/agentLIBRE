use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelConfig {
    pub dialect: ModelDialect,
    pub tool_call_format: ToolCallFormat,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            dialect: ModelDialect::Generic,
            tool_call_format: ToolCallFormat::HermesJson,
        }
    }
}

impl ModelConfig {
    pub fn validate(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelDialect {
    #[default]
    Generic,
    Qwen3,
    Gemma4,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallFormat {
    #[default]
    HermesJson,
    XmlTag,
    Gemma4,
}
