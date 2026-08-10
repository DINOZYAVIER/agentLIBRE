use std::sync::Arc;

use agl_package::{
    DirectoryPackageSource, ErasedPackagePayload, InMemoryPackageView, PackageAdapter,
    PackageAdapterDescriptor, PackageCandidate, PackageEntrypoint, PackageEnvelope, PackageError,
    PackageSource, PackageSourceKind, PackageSourceTier, PackageTypeId, PackageView,
    StaticPackageSource,
};
use anyhow::Result;
use serde::Deserialize;

use crate::manifest::{SkillHarness, SkillSource, split_frontmatter};

pub const SKILL_FILE_NAME: &str = "SKILL.md";
pub const SKILL_PAYLOAD_SCHEMA: &str = "agentlibre.skill/v2";

#[derive(Clone, Debug)]
pub struct SkillPackageAdapter {
    descriptor: PackageAdapterDescriptor,
}

impl SkillPackageAdapter {
    pub fn new() -> Result<Self, PackageError> {
        Ok(Self {
            descriptor: PackageAdapterDescriptor::new(
                PackageTypeId::skill(),
                "skills",
                PackageEntrypoint::new(SKILL_FILE_NAME)?,
            )?,
        })
    }
}

impl Default for SkillPackageAdapter {
    fn default() -> Self {
        Self::new().expect("skill adapter descriptor is valid")
    }
}

impl PackageAdapter for SkillPackageAdapter {
    fn descriptor(&self) -> &PackageAdapterDescriptor {
        &self.descriptor
    }

    fn extract_envelope(&self, package: &dyn PackageView) -> Result<PackageEnvelope, PackageError> {
        let entrypoint = SKILL_FILE_NAME.parse()?;
        let content = package.read_file(&entrypoint)?;
        let content =
            std::str::from_utf8(&content).map_err(|error| PackageError::AdapterEnvelope {
                type_id: self.descriptor.type_id.to_string(),
                reason: format!("SKILL.md is not UTF-8: {error}"),
            })?;
        parse_skill_envelope(content).map_err(|error| PackageError::AdapterEnvelope {
            type_id: self.descriptor.type_id.to_string(),
            reason: error.to_string(),
        })
    }

    fn validate_payload(
        &self,
        package: &dyn PackageView,
        envelope: &PackageEnvelope,
    ) -> Result<ErasedPackagePayload, PackageError> {
        let extracted = self.extract_envelope(package)?;
        if &extracted != envelope {
            return Err(PackageError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: "SKILL.md envelope changed during validation".to_owned(),
            });
        }
        let harness =
            SkillHarness::parse_package_view(package, envelope.id.as_str(), SkillSource::Local)
                .map_err(|error| PackageError::AdapterPayload {
                    type_id: self.descriptor.type_id.to_string(),
                    reason: error.to_string(),
                })?;
        if harness.package != *envelope {
            return Err(PackageError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: "SKILL.md envelope changed during payload validation".to_owned(),
            });
        }
        Ok(Box::new(harness))
    }
}

#[derive(Deserialize)]
struct SkillEnvelopeDocument {
    package: PackageEnvelope,
}

pub fn parse_skill_envelope(content: &str) -> Result<PackageEnvelope> {
    let (frontmatter, _) = split_frontmatter("SKILL.md", content)?;
    let document: SkillEnvelopeDocument = serde_yaml::from_str(frontmatter)
        .map_err(|error| anyhow::anyhow!("failed to parse Skill package envelope: {error}"))?;
    if document.package.type_id != PackageTypeId::skill() {
        anyhow::bail!(
            "skill package has type `{}`; expected `skill`",
            document.package.type_id
        );
    }
    if document.package.payload_schema.as_str() != SKILL_PAYLOAD_SCHEMA {
        anyhow::bail!(
            "unsupported skill payload schema `{}`; expected `{SKILL_PAYLOAD_SCHEMA}`",
            document.package.payload_schema
        );
    }
    document.package.validate().map_err(anyhow::Error::from)?;
    Ok(document.package)
}

pub fn builtin_source() -> Result<Arc<dyn PackageSource>> {
    let source_id: agl_package::PackageSourceId = "builtin".parse()?;
    let mut candidates = Vec::new();
    for package in agl_assets::BUILTIN_PACKAGES
        .iter()
        .filter(|package| package.type_id == "skill")
    {
        let files = package
            .files
            .iter()
            .map(|file| Ok::<_, PackageError>((file.path.parse()?, file.bytes.to_vec())))
            .collect::<Result<Vec<_>, _>>()?;
        candidates.push(
            PackageCandidate::new(
                package.type_id.parse()?,
                package.id.parse()?,
                package.version.parse()?,
                source_id.clone(),
                PackageSourceTier::Builtin,
                PackageSourceKind::Embedded,
                Arc::new(InMemoryPackageView::new(files)?),
            )
            .with_package_root(format!("builtin:{}/{}", package.type_id, package.id)),
        );
    }
    Ok(Arc::new(StaticPackageSource::new(
        source_id,
        PackageSourceTier::Builtin,
        PackageSourceKind::Embedded,
        candidates,
    )?))
}

pub fn directory_skill_source(
    source_id: agl_package::PackageSourceId,
    tier: PackageSourceTier,
    root: impl Into<std::path::PathBuf>,
    registry: Arc<agl_package::PackageAdapterRegistry>,
) -> Arc<dyn PackageSource> {
    Arc::new(DirectoryPackageSource::new(
        source_id,
        tier,
        PackageSourceKind::Directory,
        root,
        registry,
    ))
}
