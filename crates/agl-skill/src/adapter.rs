use std::sync::Arc;

use agl_artifact::{
    ArtifactAdapter, ArtifactAdapterDescriptor, ArtifactCandidate, ArtifactEntrypoint,
    ArtifactEnvelope, ArtifactError, ArtifactPackageView, ArtifactSource, ArtifactSourceKind,
    ArtifactSourceTier, ArtifactTypeId, DirectoryArtifactSource, ErasedArtifactPayload,
    InMemoryPackageView, StaticArtifactSource,
};
use anyhow::Result;
use serde::Deserialize;

use crate::manifest::{SkillHarness, SkillSource, split_frontmatter};

pub const SKILL_FILE_NAME: &str = "SKILL.md";
pub const SKILL_PAYLOAD_SCHEMA: &str = "agentlibre.skill/v2";

#[derive(Clone, Debug)]
pub struct SkillArtifactAdapter {
    descriptor: ArtifactAdapterDescriptor,
}

impl SkillArtifactAdapter {
    pub fn new() -> Result<Self, ArtifactError> {
        Ok(Self {
            descriptor: ArtifactAdapterDescriptor::new(
                ArtifactTypeId::skill(),
                "skills",
                ArtifactEntrypoint::new(SKILL_FILE_NAME)?,
            )?,
        })
    }
}

impl Default for SkillArtifactAdapter {
    fn default() -> Self {
        Self::new().expect("skill adapter descriptor is valid")
    }
}

impl ArtifactAdapter for SkillArtifactAdapter {
    fn descriptor(&self) -> &ArtifactAdapterDescriptor {
        &self.descriptor
    }

    fn extract_envelope(
        &self,
        package: &dyn ArtifactPackageView,
    ) -> Result<ArtifactEnvelope, ArtifactError> {
        let entrypoint = SKILL_FILE_NAME.parse()?;
        let content = package.read_file(&entrypoint)?;
        let content =
            std::str::from_utf8(&content).map_err(|error| ArtifactError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: format!("SKILL.md is not UTF-8: {error}"),
            })?;
        parse_skill_envelope(content).map_err(|error| ArtifactError::AdapterPayload {
            type_id: self.descriptor.type_id.to_string(),
            reason: error.to_string(),
        })
    }

    fn validate_payload(
        &self,
        package: &dyn ArtifactPackageView,
        envelope: &ArtifactEnvelope,
    ) -> Result<ErasedArtifactPayload, ArtifactError> {
        let extracted = self.extract_envelope(package)?;
        if &extracted != envelope {
            return Err(ArtifactError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: "SKILL.md envelope changed during validation".to_owned(),
            });
        }
        let harness =
            SkillHarness::parse_package_view(package, envelope.id.as_str(), SkillSource::Local)
                .map_err(|error| ArtifactError::AdapterPayload {
                    type_id: self.descriptor.type_id.to_string(),
                    reason: error.to_string(),
                })?;
        if harness.artifact != *envelope {
            return Err(ArtifactError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: "SKILL.md envelope changed during payload validation".to_owned(),
            });
        }
        Ok(Box::new(harness))
    }
}

#[derive(Deserialize)]
struct SkillEnvelopeDocument {
    artifact: ArtifactEnvelope,
}

pub fn parse_skill_envelope(content: &str) -> Result<ArtifactEnvelope> {
    let (frontmatter, _) = split_frontmatter("SKILL.md", content)?;
    let document: SkillEnvelopeDocument = serde_yaml::from_str(frontmatter)
        .map_err(|error| anyhow::anyhow!("failed to parse Skill artifact envelope: {error}"))?;
    if document.artifact.type_id != ArtifactTypeId::skill() {
        anyhow::bail!(
            "skill artifact has type `{}`; expected `skill`",
            document.artifact.type_id
        );
    }
    if document.artifact.payload_schema.as_str() != SKILL_PAYLOAD_SCHEMA {
        anyhow::bail!(
            "unsupported skill payload schema `{}`; expected `{SKILL_PAYLOAD_SCHEMA}`",
            document.artifact.payload_schema
        );
    }
    document.artifact.validate().map_err(anyhow::Error::from)?;
    Ok(document.artifact)
}

pub fn skill_adapter_registry() -> Result<Arc<agl_artifact::ArtifactAdapterRegistry>> {
    Ok(Arc::new(agl_artifact::ArtifactAdapterRegistry::new([
        SkillArtifactAdapter::default(),
    ])?))
}

pub fn builtin_source() -> Result<Arc<dyn ArtifactSource>> {
    let source_id: agl_artifact::ArtifactSourceId = "builtin".parse()?;
    let mut candidates = Vec::new();
    for package in agl_assets::BUILTIN_ARTIFACT_PACKAGES
        .iter()
        .filter(|package| package.type_id == "skill")
    {
        let files = package
            .files
            .iter()
            .map(|file| Ok::<_, ArtifactError>((file.path.parse()?, file.bytes.to_vec())))
            .collect::<Result<Vec<_>, _>>()?;
        candidates.push(ArtifactCandidate::new(
            package.type_id.parse()?,
            package.id.parse()?,
            package.version.parse()?,
            source_id.clone(),
            ArtifactSourceTier::Builtin,
            ArtifactSourceKind::Embedded,
            Arc::new(InMemoryPackageView::new(files)?),
        ));
    }
    Ok(Arc::new(StaticArtifactSource::new(
        source_id,
        ArtifactSourceTier::Builtin,
        ArtifactSourceKind::Embedded,
        candidates,
    )?))
}

pub fn directory_skill_source(
    source_id: agl_artifact::ArtifactSourceId,
    tier: ArtifactSourceTier,
    root: impl Into<std::path::PathBuf>,
    registry: Arc<agl_artifact::ArtifactAdapterRegistry>,
) -> Arc<dyn ArtifactSource> {
    Arc::new(DirectoryArtifactSource::new(
        source_id,
        tier,
        ArtifactSourceKind::Directory,
        root,
        registry,
    ))
}
