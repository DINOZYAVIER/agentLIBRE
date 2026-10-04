use std::path::PathBuf;

use crate::package::{PackageId, PackageRelativePath, PackageVersion, PackageView};
use agl_core::agent::ReasoningEffort;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::model::{
    ModelArtifactDigest, ModelConfig, ModelDialect, ModelFetchRequest, ToolCallFormat,
};

pub const MODEL_FILE_NAME: &str = "MODEL.toml";
pub const MODEL_SCHEMA: &str = "agentlibre.model/v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelArtifactKind {
    Gguf,
    LoraAdapter,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelArtifact {
    pub kind: ModelArtifactKind,
    pub url: String,
    pub sha256: ModelArtifactDigest,
    pub bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MmprojArtifact {
    pub url: String,
    pub sha256: ModelArtifactDigest,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelReasoning {
    pub efforts: Vec<ReasoningEffort>,
    pub preserve: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelManifest {
    pub schema: String,
    pub id: PackageId,
    pub version: PackageVersion,
    #[serde(default)]
    pub description: Option<String>,
    pub artifact: ModelArtifact,
    pub mmproj: Option<MmprojArtifact>,
    pub dialect: ModelDialect,
    pub tool_call_format: ToolCallFormat,
    #[serde(default)]
    pub reasoning: ModelReasoning,
}

impl ModelManifest {
    pub fn parse(document: &str) -> Result<Self> {
        let manifest: Self = toml::from_str(document).context("invalid MODEL.toml")?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema == MODEL_SCHEMA, "unsupported Model schema");
        if let Some(description) = &self.description {
            ensure!(
                !description.trim().is_empty(),
                "Model description cannot be empty when present"
            );
        }
        validate_artifact(&self.artifact.url, self.artifact.bytes)?;
        if let Some(mmproj) = &self.mmproj {
            validate_artifact(&mmproj.url, mmproj.bytes)?;
        }
        ModelConfig {
            dialect: self.dialect,
            tool_call_format: self.tool_call_format,
        }
        .validate()?;
        ensure!(
            self.reasoning
                .efforts
                .iter()
                .enumerate()
                .all(|(index, effort)| !self.reasoning.efforts[..index].contains(effort)),
            "Model reasoning efforts contain duplicates"
        );
        Ok(())
    }

    pub fn fetch_request(&self, destination: PathBuf) -> Result<ModelFetchRequest> {
        self.validate()?;
        Ok(ModelFetchRequest {
            url: Url::parse(&self.artifact.url)?,
            destination,
            expected_digest: self.artifact.sha256,
            expected_bytes: self.artifact.bytes,
        })
    }
}

fn validate_artifact(artifact_url: &str, bytes: u64) -> Result<()> {
    ensure!(
        bytes > 0 && bytes <= 1024_u64.pow(4),
        "Model artifact bytes must be between 1 byte and 1 TiB"
    );
    let url = Url::parse(artifact_url).context("invalid Model artifact URL")?;
    ensure!(url.scheme() == "https", "Model artifact URL requires HTTPS");
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "Model artifact URL cannot contain credentials"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "Model artifact URL cannot contain query parameters or fragments"
    );
    Ok(())
}

pub fn parse_package_view(package: &impl PackageView) -> Result<ModelManifest> {
    let bytes = package.read_file(&PackageRelativePath::new(MODEL_FILE_NAME)?)?;
    ModelManifest::parse(std::str::from_utf8(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mmproj_metadata_is_optional_and_validated_like_model_artifact() {
        let base = format!(
            "schema = \"agentlibre.model/v1\"\nid = \"vision\"\nversion = \"1.0.0\"\ndialect = \"generic\"\ntool_call_format = \"hermes_json\"\n[artifact]\nkind = \"gguf\"\nurl = \"https://example.invalid/model.gguf\"\nsha256 = \"sha256:{}\"\nbytes = 42\n",
            "01".repeat(32)
        );
        assert!(ModelManifest::parse(&base).unwrap().mmproj.is_none());
        let projector = format!(
            "\n[mmproj]\nurl = \"https://example.invalid/projector.gguf\"\nsha256 = \"sha256:{}\"\nbytes = 17\n",
            "02".repeat(32)
        );
        let document = format!("{base}{projector}");
        let manifest = ModelManifest::parse(&document).unwrap();
        assert_eq!(manifest.mmproj.unwrap().bytes, 17);
        for invalid in [
            document.replace("projector.gguf", "projector.gguf?secret=yes"),
            document.replace(
                "https://example.invalid/projector",
                "http://example.invalid/projector",
            ),
            document.replace("bytes = 17", "bytes = 0"),
            format!("{document}kind = \"gguf\"\n"),
        ] {
            assert!(ModelManifest::parse(&invalid).is_err());
        }
    }

    #[test]
    fn direct_model_manifest_owns_only_artifact_and_format() {
        let digest = format!("sha256:{}", "01".repeat(32));
        let manifest = ModelManifest::parse(&format!(
            "schema = \"agentlibre.model/v1\"\nid = \"test-model\"\nversion = \"1.0.0\"\ndialect = \"gemma4\"\ntool_call_format = \"gemma_agent_call\"\n\n[artifact]\nkind = \"gguf\"\nurl = \"https://example.invalid/model.gguf\"\nsha256 = \"{digest}\"\nbytes = 42\n"
        ))
        .unwrap();
        assert_eq!(manifest.id.to_string(), "test-model");
        assert_eq!(manifest.artifact.kind, ModelArtifactKind::Gguf);
        assert!(manifest.reasoning.efforts.is_empty());
        assert!(ModelManifest::parse(
            "schema = \"agentlibre.model/v1\"\nid = \"bad\"\nversion = \"1.0.0\"\ndialect = \"generic\"\ntool_call_format = \"hermes_json\"\nphysical_profile = {}"
        )
        .is_err());
        let capable = ModelManifest::parse(&format!(
            "schema = \"agentlibre.model/v1\"\nid = \"thinking\"\nversion = \"1.0.0\"\ndialect = \"qwen3\"\ntool_call_format = \"structured_tool_calls\"\n\n[reasoning]\nefforts = [\"xhigh\", \"medium\", \"low\"]\npreserve = true\n\n[artifact]\nkind = \"gguf\"\nurl = \"https://example.invalid/model.gguf\"\nsha256 = \"{digest}\"\nbytes = 42\n"
        ))
        .unwrap();
        assert_eq!(capable.reasoning.efforts.len(), 3);
        assert!(capable.reasoning.preserve);
        assert!(ModelManifest::parse(&format!(
            "schema = \"agentlibre.model/v1\"\nid = \"bad\"\nversion = \"1.0.0\"\ndialect = \"generic\"\ntool_call_format = \"hermes_json\"\n\n[artifact]\nkind = \"gguf\"\nurl = \"http://example.invalid/model.gguf\"\nsha256 = \"{digest}\"\nbytes = 42\n"
        ))
        .is_err());
        assert!(ModelManifest::parse(&format!(
            "schema = \"agentlibre.model/v1\"\nid = \"bad\"\nversion = \"1.0.0\"\ndialect = \"generic\"\ntool_call_format = \"hermes_json\"\n\n[artifact]\nkind = \"gguf\"\nurl = \"https://example.invalid/model.gguf?token=secret\"\nsha256 = \"{digest}\"\nbytes = 42\n"
        ))
        .is_err());
    }
}
