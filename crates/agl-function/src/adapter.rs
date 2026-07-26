use std::sync::Arc;

use agl_artifact::{
    ArtifactAdapter, ArtifactAdapterDescriptor, ArtifactCandidate, ArtifactEntrypoint,
    ArtifactEnvelope, ArtifactError, ArtifactPackageRef, ArtifactPackageView, ArtifactResolver,
    ArtifactSource, ArtifactSourceKind, ArtifactSourceTier, ArtifactTypeId,
    DirectoryArtifactSource, ErasedArtifactPayload, InMemoryPackageView, StaticArtifactSource,
};
use anyhow::{Context, Result, ensure};

use crate::loader::parse_function_document;
use crate::locator::{FunctionPackageLocation, FunctionPackageSource};
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
    let adapters: [Arc<dyn ArtifactAdapter>; 2] = [
        Arc::new(FunctionArtifactAdapter::default()),
        Arc::new(agl_model::ModelArtifactAdapter::default()),
    ];
    Ok(Arc::new(agl_artifact::ArtifactAdapterRegistry::from_dyn(
        adapters,
    )?))
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

pub fn validate_function_model_contract(
    front_matter: &AgentFunctionFrontMatter,
    inference_config: Option<&str>,
    locator: &FunctionPackageLocation,
) -> Result<()> {
    let model_requirements = front_matter
        .artifact
        .requires
        .iter()
        .filter(|requirement| requirement.type_id().as_str() == "model")
        .collect::<Vec<_>>();
    let Some(inference_config) = inference_config else {
        ensure!(
            model_requirements.is_empty(),
            "function `{}` declares a Model dependency without an inference config",
            front_matter.id()
        );
        return Ok(());
    };
    ensure!(
        model_requirements.len() == 1,
        "function `{}` must declare exactly one Model dependency for its inference config",
        front_matter.id()
    );
    let model_requirement = model_requirements[0];
    let registry = function_adapter_registry()?;
    let mut sources = Vec::new();
    if locator.source != FunctionPackageSource::Builtin {
        let (source_id, tier, root) = match locator.source {
            FunctionPackageSource::Workspace => (
                "workspace",
                ArtifactSourceTier::Workspace,
                locator
                    .root_dir
                    .parent()
                    .and_then(std::path::Path::parent)
                    .context("workspace function path has no workspace root")?
                    .to_path_buf(),
            ),
            FunctionPackageSource::Global => (
                "global",
                ArtifactSourceTier::User,
                locator
                    .root_dir
                    .parent()
                    .and_then(std::path::Path::parent)
                    .context("global function path has no config root")?
                    .to_path_buf(),
            ),
            FunctionPackageSource::Explicit => {
                anyhow::bail!(
                    "explicit Function packages with Model dependencies require a workspace or global source"
                )
            }
            FunctionPackageSource::Builtin => unreachable!(),
        };
        sources.push(directory_function_source(
            source_id.parse()?,
            tier,
            root,
            registry.clone(),
        ));
    }
    sources.push(builtin_source()?);
    let resolver = ArtifactResolver::new(registry.clone(), sources);
    let reference = ArtifactPackageRef::parse(&format!(
        "function:{}@{}",
        front_matter.id(),
        front_matter.artifact.version
    ))?;
    let graph = resolver.resolve_and_validate(&reference, None)?;
    let model_node = graph
        .nodes
        .values()
        .find(|node| {
            node.candidate.type_id.as_str() == "model"
                && node.candidate.package_id == *model_requirement.package_id()
        })
        .with_context(|| format!("resolved Model dependency `{model_requirement}` is missing"))?;
    let model_payload = registry
        .lookup(&model_node.candidate.type_id)?
        .validate_payload(model_node.candidate.view(), &model_node.envelope)?;
    let model = model_payload
        .downcast::<agl_model::ModelPackage>()
        .map_err(|_| anyhow::anyhow!("Model adapter returned an unexpected payload type"))?;
    let preset =
        agl_config::load_inference_preset_from_str("function inference.toml", inference_config)?;
    let main = model
        .artifacts
        .iter()
        .find(|artifact| artifact.model_id == preset.backend.model_id)
        .with_context(|| {
            format!(
                "function `{}` references missing Model weight `{}`",
                front_matter.id(),
                preset.backend.model_id
            )
        })?;
    ensure!(
        main.role == agl_model::ModelArtifactRole::Main,
        "function `{}` model_id `{}` must reference the Model main weight",
        front_matter.id(),
        preset.backend.model_id
    );
    if let Some(projector_id) = preset.backend.multimodal_projector_id.as_ref() {
        let projector = model
            .artifacts
            .iter()
            .find(|artifact| &artifact.model_id == projector_id)
            .with_context(|| {
                format!(
                    "function `{}` references missing Model projector `{projector_id}`",
                    front_matter.id()
                )
            })?;
        ensure!(
            projector.role == agl_model::ModelArtifactRole::Projector,
            "function `{}` projector `{projector_id}` has the wrong Model weight role",
            front_matter.id()
        );
    }
    if let Some(draft_id) = preset
        .runtime
        .fixed()
        .and_then(|runtime| runtime.mtp.draft_model_id.as_ref())
    {
        let draft = model
            .artifacts
            .iter()
            .find(|artifact| &artifact.model_id == draft_id)
            .with_context(|| {
                format!(
                    "function `{}` references missing Model draft `{draft_id}`",
                    front_matter.id()
                )
            })?;
        ensure!(
            draft.role == agl_model::ModelArtifactRole::Draft,
            "function `{}` draft `{draft_id}` has the wrong Model weight role",
            front_matter.id()
        );
    }
    Ok(())
}
