use std::sync::Arc;

use agl_artifact::{
    ArtifactAdapter, ArtifactAdapterDescriptor, ArtifactCandidate, ArtifactEntrypoint,
    ArtifactEnvelope, ArtifactError, ArtifactPackageView, ArtifactSource, ArtifactSourceKind,
    ArtifactSourceTier, ArtifactTypeId, DirectoryArtifactSource, ErasedArtifactPayload,
    InMemoryPackageView, StaticArtifactSource,
};
use anyhow::{Context, Result};

use crate::loader::parse_function_document;
use crate::manifest::{AgentFunctionFrontMatter, FUNCTION_FILE_NAME};

#[derive(Clone, Debug)]
pub struct FunctionArtifactAdapter {
    descriptor: ArtifactAdapterDescriptor,
}

impl FunctionArtifactAdapter {
    pub fn new() -> Result<Self, ArtifactError> {
        Ok(Self {
            descriptor: ArtifactAdapterDescriptor::new(
                ArtifactTypeId::new("function")?,
                "functions",
                ArtifactEntrypoint::new(FUNCTION_FILE_NAME)?,
            )?,
        })
    }
}

impl Default for FunctionArtifactAdapter {
    fn default() -> Self {
        Self::new().expect("function adapter descriptor is valid")
    }
}

impl ArtifactAdapter for FunctionArtifactAdapter {
    fn descriptor(&self) -> &ArtifactAdapterDescriptor {
        &self.descriptor
    }

    fn extract_envelope(
        &self,
        package: &dyn ArtifactPackageView,
    ) -> Result<ArtifactEnvelope, ArtifactError> {
        let entrypoint: agl_artifact::ArtifactRelativePath = FUNCTION_FILE_NAME.parse()?;
        let content = package.read_file(&entrypoint)?;
        let content =
            std::str::from_utf8(&content).map_err(|error| ArtifactError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: format!("FUNCTION.md is not UTF-8: {error}"),
            })?;
        let (front_matter, body) =
            parse_function_document(content).map_err(|error| ArtifactError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: error.to_string(),
            })?;
        if !body.trim().is_empty() {
            return Err(ArtifactError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: "FUNCTION.md body is not supported".to_owned(),
            });
        }
        front_matter
            .validate()
            .map_err(|error| ArtifactError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: error.to_string(),
            })?;
        Ok(front_matter.artifact)
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
                reason: "FUNCTION.md envelope changed during validation".to_owned(),
            });
        }
        let entrypoint: agl_artifact::ArtifactRelativePath = FUNCTION_FILE_NAME.parse()?;
        let content = package.read_file(&entrypoint)?;
        let content =
            std::str::from_utf8(&content).map_err(|error| ArtifactError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: format!("FUNCTION.md is not UTF-8: {error}"),
            })?;
        let (front_matter, _) =
            parse_function_document(content).map_err(|error| ArtifactError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: error.to_string(),
            })?;
        front_matter
            .validate()
            .map_err(|error| ArtifactError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: error.to_string(),
            })?;
        Ok(Box::new(front_matter))
    }
}

pub fn function_adapter_registry() -> Result<Arc<agl_artifact::ArtifactAdapterRegistry>> {
    Ok(Arc::new(agl_artifact::ArtifactAdapterRegistry::new([
        FunctionArtifactAdapter::default(),
    ])?))
}

pub fn builtin_source() -> Result<Arc<dyn ArtifactSource>> {
    let source_id: agl_artifact::ArtifactSourceId = "builtin".parse()?;
    let mut candidates = Vec::new();
    for package in agl_assets::BUILTIN_ARTIFACT_PACKAGES {
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

pub fn directory_function_source(
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

pub fn parse_function_envelope(content: &str) -> Result<AgentFunctionFrontMatter> {
    let (front_matter, body) =
        parse_function_document(content).context("failed to parse function")?;
    if !body.trim().is_empty() {
        anyhow::bail!("FUNCTION.md body is not supported");
    }
    front_matter.validate()?;
    Ok(front_matter)
}
