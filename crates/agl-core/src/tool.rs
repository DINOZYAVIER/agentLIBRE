use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::agent::DeliveryClass;
use crate::{EffectId, JsonSchema, ToolId};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDefinition {
    pub id: ToolId,
    pub description: String,
    pub input_schema: JsonSchema,
    pub required_effects: Vec<EffectId>,
    pub delivery: DeliveryClass,
}

impl ToolDefinition {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.description.trim().is_empty() {
            return Err("Tool description cannot be empty");
        }
        let effects = self.required_effects.iter().collect::<BTreeSet<_>>();
        if effects.len() != self.required_effects.len()
            || self
                .required_effects
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err("Tool required Effects must be sorted and duplicate-free");
        }
        Ok(())
    }
}
