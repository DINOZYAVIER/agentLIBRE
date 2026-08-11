use std::sync::Arc;

use agl_package::{
    ErasedPackagePayload, InMemoryPackageView, PackageAdapter, PackageAdapterDescriptor,
    PackageCandidate, PackageEntrypoint, PackageEnvelope, PackageError, PackageRef, PackageSource,
    PackageSourceId, PackageSourceKind, PackageSourceTier, PackageTypeId, PackageView,
    StaticPackageSource,
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    CatalogCapability, CatalogRuntimeProfile, ModelArtifact, ModelPackage, ModelPackageId,
    ModelPackageProvenance,
};

pub const MODEL_FILE_NAME: &str = "MODEL.toml";
pub const MODEL_PAYLOAD_SCHEMA: &str = "agentlibre.model/v3";

#[derive(Clone, Debug)]
pub struct ModelPackageAdapter {
    descriptor: PackageAdapterDescriptor,
}

impl ModelPackageAdapter {
    pub fn new() -> Result<Self, PackageError> {
        Ok(Self {
            descriptor: PackageAdapterDescriptor::new(
                PackageTypeId::new("model")?,
                "models",
                PackageEntrypoint::new(MODEL_FILE_NAME)?,
            )?,
        })
    }
}

impl Default for ModelPackageAdapter {
    fn default() -> Self {
        Self::new().expect("model adapter descriptor is valid")
    }
}

impl PackageAdapter for ModelPackageAdapter {
    fn descriptor(&self) -> &PackageAdapterDescriptor {
        &self.descriptor
    }

    fn extract_envelope(&self, package: &dyn PackageView) -> Result<PackageEnvelope, PackageError> {
        let path = MODEL_FILE_NAME.parse()?;
        let bytes = package.read_file(&path)?;
        let document: ModelDocument =
            toml::from_str(std::str::from_utf8(&bytes).map_err(|error| {
                PackageError::AdapterEnvelope {
                    type_id: self.descriptor.type_id.to_string(),
                    reason: format!("MODEL.toml is not UTF-8: {error}"),
                }
            })?)
            .map_err(|error| PackageError::AdapterEnvelope {
                type_id: self.descriptor.type_id.to_string(),
                reason: format!("failed to parse MODEL.toml: {error}"),
            })?;
        if document.package.type_id != PackageTypeId::new("model")? {
            return Err(PackageError::AdapterTypeMismatch {
                type_id: self.descriptor.type_id.to_string(),
                actual_type: document.package.type_id.to_string(),
            });
        }
        if document.package.payload_schema.as_str() != MODEL_PAYLOAD_SCHEMA {
            return Err(PackageError::AdapterEnvelope {
                type_id: self.descriptor.type_id.to_string(),
                reason: format!(
                    "unsupported model payload schema `{}`; expected {MODEL_PAYLOAD_SCHEMA}",
                    document.package.payload_schema
                ),
            });
        }
        document.package.validate()?;
        Ok(document.package)
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
                reason: "MODEL.toml envelope changed during validation".to_owned(),
            });
        }
        let model = parse_model_package(package, envelope).map_err(|error| {
            PackageError::AdapterPayload {
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
    package: PackageEnvelope,
    display_name: String,
    capabilities: Vec<CatalogCapability>,
    license: String,
    license_url: String,
    repository: String,
    upstream_revision: String,
    weights: Vec<ModelArtifact>,
    profiles: Vec<CatalogRuntimeProfile>,
}

pub fn builtin_model_source() -> Result<Arc<dyn PackageSource>, PackageError> {
    let source_id: PackageSourceId = "builtin".parse()?;
    let mut candidates = Vec::new();
    for package in agl_assets::BUILTIN_PACKAGES
        .iter()
        .filter(|package| package.type_id == "model")
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

pub fn parse_model_package(
    package: &dyn PackageView,
    envelope: &PackageEnvelope,
) -> Result<ModelPackage> {
    let path = MODEL_FILE_NAME.parse()?;
    let bytes = package.read_file(&path)?;
    let text = std::str::from_utf8(&bytes).context("MODEL.toml is not UTF-8")?;
    let document: ModelDocument = toml::from_str(text).context("failed to parse MODEL.toml")?;
    if document.package != *envelope {
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
        provenance: None,
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
    let adapter = ModelPackageAdapter::default();
    let source = builtin_model_source()?;
    let candidates = source.candidates(&PackageTypeId::new("model")?)?;
    let mut packages = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let reference = PackageRef::parse(&format!(
            "model:{}@={}",
            candidate.package_id, candidate.version
        ))?;
        let envelope = adapter.extract_envelope(candidate.view())?;
        let payload = adapter.validate_payload(candidate.view(), &envelope)?;
        let mut package = *payload
            .downcast::<ModelPackage>()
            .map_err(|_| anyhow::anyhow!("model adapter returned an unexpected payload type"))?;
        package.provenance = Some(ModelPackageProvenance {
            reference: reference.clone(),
            source_id: candidate.source_id.clone(),
            source_tier: candidate.tier,
            source_kind: candidate.kind,
            package_tree_digest: agl_package::compute_package_digest(candidate.view())?,
        });
        packages.push(package);
    }
    packages.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(packages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_model_resolves_through_the_common_adapter() {
        let adapter = ModelPackageAdapter::default();
        let source = builtin_model_source().unwrap();
        let candidates = source
            .candidates(&PackageTypeId::new("model").unwrap())
            .unwrap();

        assert_eq!(candidates.len(), 5);
        for candidate in candidates {
            let envelope = adapter.extract_envelope(candidate.view()).unwrap();
            let payload = adapter
                .validate_payload(candidate.view(), &envelope)
                .unwrap();
            let package = payload.downcast::<ModelPackage>().unwrap();
            assert_eq!(package.id.as_str(), candidate.package_id.as_str());
            assert!(!package.artifacts.is_empty());
            assert!(
                agl_package::compute_package_digest(candidate.view())
                    .unwrap()
                    .as_str()
                    .starts_with("sha256:")
            );
        }
    }
}
