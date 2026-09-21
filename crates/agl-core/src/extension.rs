use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{EffectDefinition, ExtensionId, ToolDefinition};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionDefinition {
    pub id: ExtensionId,
    pub effects: Vec<EffectDefinition>,
    pub tools: Vec<ToolDefinition>,
}

impl ExtensionDefinition {
    pub fn validate(&self) -> Result<(), &'static str> {
        let effect_ids = self
            .effects
            .iter()
            .map(|effect| &effect.id)
            .collect::<BTreeSet<_>>();
        if effect_ids.len() != self.effects.len() {
            return Err("Extension contains duplicate Effect IDs");
        }
        if self.effects.windows(2).any(|pair| pair[0].id >= pair[1].id) {
            return Err("Extension Effects must be sorted");
        }
        let tool_ids = self
            .tools
            .iter()
            .map(|tool| &tool.id)
            .collect::<BTreeSet<_>>();
        if tool_ids.len() != self.tools.len() {
            return Err("Extension contains duplicate Tool IDs");
        }
        if self.tools.windows(2).any(|pair| pair[0].id >= pair[1].id) {
            return Err("Extension Tools must be sorted");
        }
        for tool in &self.tools {
            tool.validate()?;
            if !tool
                .id
                .as_str()
                .starts_with(&format!("{}:", self.id.as_str()))
            {
                return Err("Tool ID is not owned by its Extension");
            }
            if tool
                .required_effects
                .iter()
                .any(|effect| !effect_ids.contains(effect))
            {
                return Err("Tool requires an Effect not declared by its Extension");
            }
        }
        Ok(())
    }
}
