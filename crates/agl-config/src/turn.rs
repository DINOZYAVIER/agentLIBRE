use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::ModelConfig;

pub const MAX_TOOL_CALLS_PER_TURN: usize = 64;
pub const MAX_FINAL_ANSWER_RETRIES: u32 = 8;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct TurnPolicyConfig {
    pub model: ModelConfig,
    pub tools: ToolPolicyConfig,
    pub response: ResponsePolicyConfig,
}

impl TurnPolicyConfig {
    pub fn validate(&self) -> Result<()> {
        self.model.validate()?;
        self.tools.validate()?;
        self.response.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolPolicyConfig {
    pub max_tool_calls: usize,
    pub require_visible_tool: bool,
}

impl Default for ToolPolicyConfig {
    fn default() -> Self {
        Self {
            max_tool_calls: 0,
            require_visible_tool: true,
        }
    }
}

impl ToolPolicyConfig {
    pub fn validate(&self) -> Result<()> {
        if self.max_tool_calls > MAX_TOOL_CALLS_PER_TURN {
            bail!(
                "max_tool_calls {} exceeds maximum {}",
                self.max_tool_calls,
                MAX_TOOL_CALLS_PER_TURN
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResponsePolicyConfig {
    pub reasoning: ReasoningPolicy,
    pub boundary: BoundaryPolicy,
    pub max_final_answer_retries: u32,
}

impl Default for ResponsePolicyConfig {
    fn default() -> Self {
        Self {
            reasoning: ReasoningPolicy::Preserve,
            boundary: BoundaryPolicy::Stop,
            max_final_answer_retries: 0,
        }
    }
}

impl ResponsePolicyConfig {
    pub fn validate(&self) -> Result<()> {
        if self.max_final_answer_retries > MAX_FINAL_ANSWER_RETRIES {
            bail!(
                "max_final_answer_retries {} exceeds maximum {}",
                self.max_final_answer_retries,
                MAX_FINAL_ANSWER_RETRIES
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningPolicy {
    #[default]
    Preserve,
    Strip,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryPolicy {
    Ignore,
    Truncate,
    #[default]
    Stop,
}
