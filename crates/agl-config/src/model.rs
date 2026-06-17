use anyhow::{Result, bail};
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
        match (self.dialect, self.tool_call_format) {
            (ModelDialect::Generic, ToolCallFormat::HermesJson)
            | (ModelDialect::Generic, ToolCallFormat::StructuredToolCalls)
            | (ModelDialect::Qwen3, ToolCallFormat::HermesJson)
            | (ModelDialect::Qwen3, ToolCallFormat::StructuredToolCalls)
            | (ModelDialect::Gemma4, ToolCallFormat::GemmaFunctionCall) => Ok(()),
            (dialect, tool_call_format) => bail!(
                "tool_call_format {tool_call_format:?} is not supported for dialect {dialect:?}"
            ),
        }
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
    StructuredToolCalls,
    #[default]
    HermesJson,
    GemmaFunctionCall,
}
