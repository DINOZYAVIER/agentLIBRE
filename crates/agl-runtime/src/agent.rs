use crate::package::{PackageId, PackageRelativePath, PackageVersion, PackageView};
use agl_core::ToolId;
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const AGENT_FILE_NAME: &str = "AGENT.md";
pub const AGENT_INSTRUCTIONS_FILE_NAME: &str = "SYSTEM.md";
pub const AGENT_PAYLOAD_SCHEMA: &str = "agentlibre.agent/v1";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManifest {
    pub schema: String,
    pub id: PackageId,
    pub version: PackageVersion,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub required_tools: Vec<ToolId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentPackage {
    pub manifest: AgentManifest,
    pub instructions: String,
}

impl AgentPackage {
    pub fn parse(manifest: &str, instructions: &str) -> Result<Self> {
        let manifest = manifest
            .strip_prefix("---\n")
            .and_then(|value| value.strip_suffix("\n---\n"))
            .ok_or_else(|| anyhow::anyhow!("AGENT.md must contain only YAML front matter"))?;
        let package = Self {
            manifest: serde_yaml::from_str(manifest)?,
            instructions: instructions.trim().to_owned(),
        };
        package.validate()?;
        Ok(package)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.manifest.schema == AGENT_PAYLOAD_SCHEMA,
            "unsupported Agent payload schema"
        );
        if let Some(description) = &self.manifest.description {
            ensure!(
                !description.trim().is_empty(),
                "Agent description cannot be empty when present"
            );
        }
        ensure!(
            !self.instructions.is_empty() && self.instructions.len() <= 1024 * 1024,
            "Agent instructions are empty or exceed 1 MiB"
        );
        let mut tools = self.manifest.required_tools.clone();
        tools.sort();
        tools.dedup();
        ensure!(
            tools.len() == self.manifest.required_tools.len(),
            "Agent required_tools contain duplicates"
        );
        Ok(())
    }
}

pub fn parse_package_view(package: &impl PackageView) -> Result<AgentPackage> {
    let manifest = package.read_file(&PackageRelativePath::new(AGENT_FILE_NAME)?)?;
    let instructions =
        package.read_file(&PackageRelativePath::new(AGENT_INSTRUCTIONS_FILE_NAME)?)?;
    AgentPackage::parse(
        std::str::from_utf8(&manifest)?,
        std::str::from_utf8(&instructions)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thin_exact_schema_parses_without_a_builtin_catalog() {
        let package = AgentPackage::parse(
            "---\nschema: agentlibre.agent/v1\nid: test-agent\nversion: 1.0.0\ndescription: Test Agent\nrequired_tools: []\n---\n",
            "Answer precisely.",
        )
        .unwrap();
        assert_eq!(package.manifest.version.to_string(), "1.0.0");
        assert_eq!(package.manifest.id.to_string(), "test-agent");
        assert!(
            AgentPackage::parse(
                "---\npackage:\n  schema: agentlibre.package/v1\n---\n",
                "legacy"
            )
            .is_err()
        );
        assert!(
            AgentPackage::parse(
                "---\nschema: agentlibre.agent/v1\nid: test-agent\nversion: 1.0.0\ntitle: legacy\n---\n",
                "legacy"
            )
            .is_err()
        );
    }
}
