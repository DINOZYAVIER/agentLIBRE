use std::path::{Component, Path};

use crate::package::{PackageId, PackageRelativePath, PackageVersion, PackageView};
use agl_core::Content;
use agl_core::ToolId;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const SKILL_FILE_NAME: &str = "SKILL.md";
pub const SKILL_PAYLOAD_SCHEMA: &str = "agentlibre.skill/v1";
const MAX_INSTRUCTIONS_BYTES: usize = 1024 * 1024;

pub type SkillId = crate::package::PackageId;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillManifest {
    pub schema: String,
    pub id: PackageId,
    pub version: PackageVersion,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required_tools: Vec<ToolId>,
    #[serde(default)]
    pub references: Vec<SkillReference>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillReference {
    pub path: String,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillDefinition {
    pub id: SkillId,
    pub description: Option<String>,
    pub instructions: Content,
    pub required_tools: Vec<ToolId>,
    pub references: Vec<SkillReference>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillPackage {
    pub manifest: SkillManifest,
    pub instructions: String,
}

impl SkillPackage {
    pub fn parse(document: &str) -> Result<Self> {
        let (front_matter, instructions) = split_document(document)?;
        let manifest: SkillManifest = serde_yaml::from_str(front_matter)?;
        let package = Self {
            manifest,
            instructions: instructions.trim().to_owned(),
        };
        package.validate()?;
        Ok(package)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.manifest.schema == SKILL_PAYLOAD_SCHEMA,
            "unsupported Skill payload schema"
        );
        if let Some(description) = &self.manifest.description {
            ensure!(
                !description.trim().is_empty(),
                "Skill description cannot be empty when present"
            );
        }
        ensure!(
            !self.instructions.is_empty() && self.instructions.len() <= MAX_INSTRUCTIONS_BYTES,
            "Skill instructions are empty or exceed 1 MiB"
        );
        let mut tools = self.manifest.required_tools.clone();
        tools.sort();
        tools.dedup();
        ensure!(
            tools.len() == self.manifest.required_tools.len(),
            "Skill required_tools must be duplicate-free"
        );
        for reference in &self.manifest.references {
            validate_relative_path(&reference.path)?;
            ensure!(
                !reference.description.trim().is_empty(),
                "Skill reference description cannot be empty"
            );
        }
        let unique_references = self
            .manifest
            .references
            .iter()
            .map(|reference| reference.path.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        ensure!(
            unique_references.len() == self.manifest.references.len(),
            "Skill reference paths must be duplicate-free"
        );
        Ok(())
    }

    pub fn definition(&self) -> Result<SkillDefinition> {
        self.validate()?;
        Ok(SkillDefinition {
            id: self.manifest.id.clone(),
            description: self.manifest.description.clone(),
            instructions: Content::text(self.instructions.clone())?,
            required_tools: self.manifest.required_tools.clone(),
            references: self.manifest.references.clone(),
        })
    }
}

pub fn parse_package_view(package: &impl PackageView) -> Result<SkillPackage> {
    let path = PackageRelativePath::new(SKILL_FILE_NAME)?;
    let bytes = package.read_file(&path)?;
    let skill = SkillPackage::parse(std::str::from_utf8(&bytes)?)?;
    for reference in &skill.manifest.references {
        package.read_file(&PackageRelativePath::new(reference.path.clone())?)?;
    }
    Ok(skill)
}

fn split_document(document: &str) -> Result<(&str, &str)> {
    let rest = document
        .strip_prefix("---\n")
        .ok_or_else(|| anyhow::anyhow!("SKILL.md must start with YAML front matter"))?;
    let (front_matter, body) = rest
        .split_once("\n---\n")
        .ok_or_else(|| anyhow::anyhow!("SKILL.md front matter is not terminated"))?;
    Ok((front_matter, body))
}

fn validate_relative_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    ensure!(
        !path.as_os_str().is_empty() && !path.is_absolute(),
        "invalid Skill reference path"
    );
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "Skill reference path must stay inside the package"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::package::{InMemoryPackageView, PackageRelativePath};

    use super::*;

    #[test]
    fn thin_skill_parses_without_an_embedded_catalog() {
        let package = SkillPackage::parse(
            "---\nschema: agentlibre.skill/v1\nid: test-skill\nversion: 1.0.0\ndescription: Test skill\nrequired_tools: [test.extension:read]\nreferences: []\n---\nUse the declared read Tool.",
        )
        .unwrap();
        assert_eq!(package.manifest.version.to_string(), "1.0.0");
        assert!(!package.manifest.required_tools.is_empty());
        assert!(package.manifest.references.is_empty());
        let definition = package.definition().unwrap();
        assert_eq!(definition.id, package.manifest.id);
        assert_eq!(definition.instructions.as_text(), package.instructions);
        assert!(
            SkillPackage::parse("---\npackage:\n  schema: agentlibre.package/v1\n---\nlegacy")
                .is_err()
        );
    }

    #[test]
    fn declared_reference_must_be_unique_and_present() {
        let document = "---\nschema: agentlibre.skill/v1\nid: refs\nversion: 1.0.0\nreferences:\n  - path: references/guide.md\n    description: Guide\n---\nUse the guide.";
        let view = InMemoryPackageView::new([(
            PackageRelativePath::new(SKILL_FILE_NAME).unwrap(),
            document.as_bytes().to_vec(),
        )])
        .unwrap();
        assert!(parse_package_view(&view).is_err());
        assert!(
            SkillPackage::parse(&document.replace(
                "---\nUse the guide.",
                "  - path: references/guide.md\n    description: Duplicate\n---\nUse the guide."
            ))
            .is_err()
        );
    }
}
