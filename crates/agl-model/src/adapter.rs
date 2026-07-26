use std::sync::Arc;

use agl_artifact::{
    ArtifactAdapter, ArtifactAdapterDescriptor, ArtifactCandidate, ArtifactEntrypoint,
    ArtifactEnvelope, ArtifactError, ArtifactPackageRef, ArtifactPackageView, ArtifactResolver,
    ArtifactSource, ArtifactSourceId, ArtifactSourceKind, ArtifactSourceTier, ArtifactTypeId,
    ErasedArtifactPayload, InMemoryPackageView, StaticArtifactSource,
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    CatalogCapability, CatalogRuntimeProfile, ModelArtifact, ModelPackage, ModelPackageId,
};

pub const MODEL_FILE_NAME: &str = "MODEL.toml";
pub const MODEL_PAYLOAD_SCHEMA: &str = "agentlibre.model/v2";

#[derive(Clone, Debug)]
pub struct ModelArtifactAdapter {
    descriptor: ArtifactAdapterDescriptor,
}

impl ModelArtifactAdapter {
    pub fn new() -> Result<Self, ArtifactError> {
        Ok(Self {
            descriptor: ArtifactAdapterDescriptor::new(
                ArtifactTypeId::new("model")?,
                "models",
                ArtifactEntrypoint::new(MODEL_FILE_NAME)?,
            )?,
        })
    }
}

impl Default for ModelArtifactAdapter {
    fn default() -> Self {
        Self::new().expect("model adapter descriptor is valid")
    }
}

impl ArtifactAdapter for ModelArtifactAdapter {
    fn descriptor(&self) -> &ArtifactAdapterDescriptor {
        &self.descriptor
    }

    fn extract_envelope(
        &self,
        package: &dyn ArtifactPackageView,
    ) -> Result<ArtifactEnvelope, ArtifactError> {
        let path = MODEL_FILE_NAME.parse()?;
        let bytes = package.read_file(&path)?;
        let document: ModelDocument =
            toml::from_str(std::str::from_utf8(&bytes).map_err(|error| {
                ArtifactError::AdapterPayload {
                    type_id: self.descriptor.type_id.to_string(),
                    reason: format!("MODEL.toml is not UTF-8: {error}"),
                }
            })?)
            .map_err(|error| ArtifactError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: format!("failed to parse MODEL.toml: {error}"),
            })?;
        if document.artifact.type_id != ArtifactTypeId::new("model")? {
            return Err(ArtifactError::AdapterTypeMismatch {
                type_id: self.descriptor.type_id.to_string(),
                actual_type: document.artifact.type_id.to_string(),
            });
        }
        if document.artifact.payload_schema.as_str() != MODEL_PAYLOAD_SCHEMA {
            return Err(ArtifactError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: format!(
                    "unsupported model payload schema `{}`; expected {MODEL_PAYLOAD_SCHEMA}",
                    document.artifact.payload_schema
                ),
            });
        }
        document.artifact.validate()?;
        Ok(document.artifact)
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
                reason: "MODEL.toml envelope changed during validation".to_owned(),
            });
        }
        let model = parse_model_package(package, envelope).map_err(|error| {
            ArtifactError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: error.to_string(),
            }
        })?;
        Ok(Box::new(model))
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelDocument {
    artifact: ArtifactEnvelope,
    display_name: String,
    capabilities: Vec<CatalogCapability>,
    license: String,
    license_url: String,
    repository: String,
    upstream_revision: String,
    weights: Vec<ModelArtifact>,
    profiles: Vec<CatalogRuntimeProfile>,
}

pub fn model_adapter_registry() -> Result<Arc<agl_artifact::ArtifactAdapterRegistry>, ArtifactError>
{
    Ok(Arc::new(agl_artifact::ArtifactAdapterRegistry::new([
        ModelArtifactAdapter::default(),
    ])?))
}

pub fn builtin_model_source() -> Result<Arc<dyn ArtifactSource>, ArtifactError> {
    let source_id: ArtifactSourceId = "builtin".parse()?;
    let mut candidates = Vec::new();
    for package in agl_assets::BUILTIN_ARTIFACT_PACKAGES
        .iter()
        .filter(|package| package.type_id == "model")
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

pub fn parse_model_package(
    package: &dyn ArtifactPackageView,
    envelope: &ArtifactEnvelope,
) -> Result<ModelPackage> {
    let path = MODEL_FILE_NAME.parse()?;
    let bytes = package.read_file(&path)?;
    let text = std::str::from_utf8(&bytes).context("MODEL.toml is not UTF-8")?;
    let document: ModelDocument = toml::from_str(text).context("failed to parse MODEL.toml")?;
    if document.artifact != *envelope {
        bail!("MODEL.toml envelope does not match resolved envelope");
    }
    for profile in &document.profiles {
        let evidence = profile.benchmark_evidence.parse()?;
        package
            .read_file(&evidence)
            .with_context(|| format!("missing profile evidence {}", profile.benchmark_evidence))?;
    }
    Ok(ModelPackage {
        id: ModelPackageId::new(envelope.id.as_str())?,
        display_name: document.display_name,
        capabilities: document.capabilities,
        license: document.license,
        license_url: document.license_url,
        repository: document.repository,
        revision: document.upstream_revision,
        artifacts: document.weights,
        profiles: document.profiles,
    })
}

pub fn resolved_builtin_model_packages() -> Result<Vec<ModelPackage>> {
    let registry = model_adapter_registry()?;
    let source = builtin_model_source()?;
    let candidates = source.candidates(&ArtifactTypeId::new("model")?)?;
    let resolver = ArtifactResolver::new(registry.clone(), vec![source]);
    let mut packages = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let reference = ArtifactPackageRef::parse(&format!(
            "model:{}@{}",
            candidate.package_id, candidate.version
        ))?;
        let graph = resolver.resolve_and_validate(&reference, None)?;
        let node = graph
            .nodes
            .get(&graph.root)
            .context("resolved model graph is missing its root")?;
        let payload = registry
            .lookup(&node.candidate.type_id)?
            .validate_payload(node.candidate.view(), &node.envelope)?;
        packages.push(
            *payload.downcast::<ModelPackage>().map_err(|_| {
                anyhow::anyhow!("model adapter returned an unexpected payload type")
            })?,
        );
    }
    packages.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(packages)
}

#[cfg(test)]
mod tests {
    use agl_artifact::{ArtifactPackageRef, ArtifactResolver};

    use super::*;

    #[test]
    fn every_builtin_model_resolves_through_the_common_adapter() {
        let registry = model_adapter_registry().unwrap();
        let source = builtin_model_source().unwrap();
        let candidates = source
            .candidates(&ArtifactTypeId::new("model").unwrap())
            .unwrap();
        let resolver = ArtifactResolver::new(registry.clone(), vec![source]);

        assert_eq!(candidates.len(), 5);
        for candidate in candidates {
            let reference = ArtifactPackageRef::parse(&format!(
                "model:{}@{}",
                candidate.package_id, candidate.version
            ))
            .unwrap();
            let graph = resolver.resolve_and_validate(&reference, None).unwrap();
            let node = graph.nodes.get(&graph.root).unwrap();
            let payload = registry
                .lookup(&node.candidate.type_id)
                .unwrap()
                .validate_payload(node.candidate.view(), &node.envelope)
                .unwrap();
            let package = payload.downcast::<ModelPackage>().unwrap();
            assert_eq!(package.id.as_str(), candidate.package_id.as_str());
            assert!(!package.artifacts.is_empty());
            assert!(node.package_digest.as_str().starts_with("sha256:"));
        }
    }
}
