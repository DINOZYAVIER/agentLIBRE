use std::collections::BTreeSet;

use agl_extension::SkillId;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptConfig {
    #[serde(default)]
    pub system: SystemPrompt,
    #[serde(default)]
    pub skills: Vec<String>,
}

impl PromptConfig {
    pub fn validate(&self) -> Result<()> {
        let mut seen = BTreeSet::new();
        for skill in &self.skills {
            if let Err(err) = SkillId::new(skill.clone()) {
                bail!("prompt skill id is invalid: {err}");
            }
            if !seen.insert(skill) {
                bail!("prompt skill id is duplicated: {skill}");
            }
        }
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
