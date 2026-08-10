use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{SkillHarness, SkillSource, SkillTrustState};

const TRUST_SCHEMA: &str = "agentlibre.skill-trust/v1";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillTrustStore {
    schema: String,
    #[serde(default)]
    trusted: BTreeSet<String>,
    #[serde(default)]
    revoked: BTreeSet<String>,
}

impl Default for SkillTrustStore {
    fn default() -> Self {
        Self {
            schema: TRUST_SCHEMA.to_owned(),
            trusted: BTreeSet::new(),
            revoked: BTreeSet::new(),
        }
    }
}

impl SkillTrustStore {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, SkillTrustStoreError> {
        let path = path.as_ref();
        let value = match fs::read_to_string(path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(SkillTrustStoreError::Io(error.to_string())),
        };
        let store: Self = toml::from_str(&value)
            .map_err(|error| SkillTrustStoreError::Invalid(error.to_string()))?;
        if store.schema != TRUST_SCHEMA {
            return Err(SkillTrustStoreError::Invalid(format!(
                "unsupported Skill trust schema `{}`",
                store.schema
            )));
        }
        Ok(store)
    }

    pub fn state(&self, skill: &SkillHarness) -> SkillTrustState {
        if skill.source == SkillSource::Core {
            return SkillTrustState::TrustedByBinary;
        }
        let identity = skill_identity(skill);
        if self.revoked.contains(&identity) {
            SkillTrustState::Revoked
        } else if self.trusted.contains(&identity) {
            SkillTrustState::TrustedLocal
        } else {
            SkillTrustState::Unknown
        }
    }

    pub fn trust(&mut self, skill: &SkillHarness) {
        let identity = skill_identity(skill);
        self.revoked.remove(&identity);
        self.trusted.insert(identity);
    }

    pub fn revoke(&mut self, skill: &SkillHarness) {
        let identity = skill_identity(skill);
        self.trusted.remove(&identity);
        self.revoked.insert(identity);
    }

    pub fn write_atomic(&self, path: impl AsRef<Path>) -> Result<(), SkillTrustStoreError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| SkillTrustStoreError::Io(error.to_string()))?;
        }
        let value = toml::to_string(self)
            .map_err(|error| SkillTrustStoreError::Invalid(error.to_string()))?;
        let temporary = path.with_extension("toml.tmp");
        let result = (|| {
            let mut file = fs::File::create(&temporary)?;
            file.write_all(value.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            Ok::<(), std::io::Error>(())
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary);
            return Err(SkillTrustStoreError::Io(error.to_string()));
        }
        Ok(())
    }
}

pub fn skill_identity(skill: &SkillHarness) -> String {
    format!(
        "{}:{}@{}#{}",
        skill.package.type_id, skill.package.id, skill.package.version, skill.tree_sha256
    )
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum SkillTrustStoreError {
    #[error("invalid Skill trust store: {0}")]
    Invalid(String),
    #[error("Skill trust store I/O failed: {0}")]
    Io(String),
}
