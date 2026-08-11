use agl_package::{
    ErasedPackagePayload, PackageAdapter, PackageAdapterDescriptor, PackageEntrypoint,
    PackageEnvelope, PackageError, PackageTypeId, PackageView,
};
use anyhow::{Context, Result, ensure};

use crate::loader::parse_function_document;
use crate::manifest::{AgentFunctionFrontMatter, FUNCTION_FILE_NAME};

#[derive(Clone, Debug)]
pub struct FunctionPackageAdapter {
    descriptor: PackageAdapterDescriptor,
}

impl FunctionPackageAdapter {
    pub fn new() -> Result<Self, PackageError> {
        Ok(Self {
            descriptor: PackageAdapterDescriptor::new(
                PackageTypeId::new("function")?,
                "functions",
                PackageEntrypoint::new(FUNCTION_FILE_NAME)?,
            )?,
        })
    }
}

impl Default for FunctionPackageAdapter {
    fn default() -> Self {
        Self::new().expect("function adapter descriptor is valid")
    }
}

impl PackageAdapter for FunctionPackageAdapter {
    fn descriptor(&self) -> &PackageAdapterDescriptor {
        &self.descriptor
    }

    fn extract_envelope(&self, package: &dyn PackageView) -> Result<PackageEnvelope, PackageError> {
        let entrypoint: agl_package::PackageRelativePath = FUNCTION_FILE_NAME.parse()?;
        let content = package.read_file(&entrypoint)?;
        let content =
            std::str::from_utf8(&content).map_err(|error| PackageError::AdapterEnvelope {
                type_id: self.descriptor.type_id.to_string(),
                reason: format!("FUNCTION.md is not UTF-8: {error}"),
            })?;
        let (front_matter, body) =
            parse_function_document(content).map_err(|error| PackageError::AdapterEnvelope {
                type_id: self.descriptor.type_id.to_string(),
                reason: error.to_string(),
            })?;
        if !body.trim().is_empty() {
            return Err(PackageError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: "FUNCTION.md body is not supported".to_owned(),
            });
        }
        front_matter
            .validate()
            .map_err(|error| PackageError::AdapterEnvelope {
                type_id: self.descriptor.type_id.to_string(),
                reason: error.to_string(),
            })?;
        Ok(front_matter.package)
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
                reason: "FUNCTION.md envelope changed during validation".to_owned(),
            });
        }
        let entrypoint: agl_package::PackageRelativePath = FUNCTION_FILE_NAME.parse()?;
        let content = package.read_file(&entrypoint)?;
        let content =
            std::str::from_utf8(&content).map_err(|error| PackageError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: format!("FUNCTION.md is not UTF-8: {error}"),
            })?;
        let (front_matter, _) =
            parse_function_document(content).map_err(|error| PackageError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: error.to_string(),
            })?;
        front_matter
            .validate()
            .map_err(|error| PackageError::AdapterPayload {
                type_id: self.descriptor.type_id.to_string(),
                reason: error.to_string(),
            })?;
        Ok(Box::new(front_matter))
    }
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

pub fn validate_resolved_function_model_contract(
    front_matter: &AgentFunctionFrontMatter,
    _inference_config: Option<&str>,
    graph: &agl_package::ResolvedPackageGraph,
    registry: &agl_package::PackageAdapterRegistry,
) -> Result<()> {
    let model_requirements = front_matter
        .package
        .requires
        .iter()
        .filter(|requirement| requirement.type_id().as_str() == "model")
        .collect::<Vec<_>>();
    let Some(selected_profile) = front_matter.model_profile() else {
        ensure!(
            model_requirements.is_empty(),
            "function `{}` declares a Model dependency without model.profile",
            front_matter.id()
        );
        return Ok(());
    };
    ensure!(
        model_requirements.len() == 1,
        "function `{}` must declare exactly one Model dependency for model.profile",
        front_matter.id()
    );
    let model_requirement = model_requirements[0];
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
    ensure!(
        model
            .profiles
            .iter()
            .any(|profile| profile.id == selected_profile),
        "function `{}` references missing Model profile `{selected_profile}`",
        front_matter.id(),
    );
    Ok(())
}
