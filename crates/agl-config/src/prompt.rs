use std::collections::BTreeSet;

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
            validate_prompt_skill_id(skill)?;
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

fn validate_prompt_skill_id(value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.' | b':')
        })
        && value.matches(':').count() <= 1
        && !value.starts_with(':')
        && !value.ends_with(':');
    if !valid {
        bail!(
            "prompt skill id is invalid: skill id must use lowercase ASCII letters, digits, hyphens, underscores, dots, or one namespace colon: {value}"
        );
    }
    Ok(())
}
