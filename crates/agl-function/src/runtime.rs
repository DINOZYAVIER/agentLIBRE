use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;

use agl_package::{PackageAdapterRegistry, PackageCandidate, ResolvedPackageGraph};

#[cfg(test)]
use crate::loader::load_function;
use crate::loader::{LoadedFunction, load_function_candidate};
use crate::locator::FunctionPackageSource;
#[cfg(test)]
use crate::locator::resolve_function_package;
use crate::manifest::{
    FunctionDelegationBudget, FunctionToolMode, FunctionToolPolicy, RuntimeIdentityValidation,
};
use crate::render::render_function_context;
use crate::subagent::{
    RuntimeDelegationPlan, RuntimeSubagent, RuntimeSubagentSpec, resolve_runtime_subagent_specs,
};
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RuntimeFunction {
    pub reference: String,
    pub source: FunctionPackageSource,
    pub path: PathBuf,
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub model_profile: Option<String>,
    pub generation_policy: Option<agl_model::GenerationPolicy>,
    pub tool_mode: Option<FunctionToolMode>,
    pub tool_policy: Option<FunctionToolPolicy>,
    pub max_output_tokens: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub skills: Vec<String>,
    pub extensions: Vec<String>,
    pub memory_enabled: bool,
    pub subagents: Vec<RuntimeSubagent>,
    pub subagent_specs: BTreeMap<String, RuntimeSubagentSpec>,
    pub delegation: Option<FunctionDelegationBudget>,
    pub system_prompt_path: PathBuf,
    pub runtime_identity_validation: Option<RuntimeIdentityValidation>,
    pub context: String,
}

impl RuntimeFunction {
    pub fn delegation_plan(&self) -> Option<RuntimeDelegationPlan> {
        self.delegation
            .as_ref()
            .map(|budget| RuntimeDelegationPlan {
                budget: budget.clone(),
                root_subagents: self
                    .subagents
                    .iter()
                    .map(|subagent| subagent.id.clone())
                    .collect(),
                subagent_specs: self.subagent_specs.clone(),
            })
    }
}

#[cfg(test)]
pub fn resolve_runtime_function(
    reference: &str,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
) -> Result<RuntimeFunction> {
    resolve_runtime_function_with_profile_policy(reference, workspace_root, config_dir, true)
}

#[cfg(test)]
pub fn resolve_runtime_function_allow_missing_profile(
    reference: &str,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
) -> Result<RuntimeFunction> {
    resolve_runtime_function_with_profile_policy(reference, workspace_root, config_dir, false)
}

#[cfg(test)]
pub(crate) fn resolve_runtime_function_with_profile_policy(
    reference: &str,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
    require_profile: bool,
) -> Result<RuntimeFunction> {
    let locator = resolve_function_package(reference, &workspace_root, &config_dir)?;
    let loaded = load_function(locator)?;
    let subagent_specs = resolve_runtime_subagent_specs(
        &loaded,
        workspace_root.as_ref(),
        config_dir.as_ref(),
        require_profile,
    )?;
    Ok(runtime_function_from_loaded(loaded, subagent_specs))
}

pub fn runtime_function_from_candidate(
    candidate: &PackageCandidate,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
    require_profile: bool,
) -> Result<RuntimeFunction> {
    let loaded = load_function_candidate(candidate)?;
    runtime_function_from_loaded_with_profile_policy(
        loaded,
        workspace_root,
        config_dir,
        require_profile,
    )
}

pub fn runtime_function_from_resolved_graph(
    graph: &ResolvedPackageGraph,
    registry: &PackageAdapterRegistry,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
    require_profile: bool,
) -> Result<RuntimeFunction> {
    let root = graph
        .nodes
        .get(&graph.root)
        .ok_or_else(|| anyhow::anyhow!("resolved Function graph has no root candidate"))?;
    let loaded = load_function_candidate(&root.candidate)?;
    crate::validate_resolved_function_model_contract(&loaded.front_matter, None, graph, registry)?;
    runtime_function_from_loaded_with_profile_policy(
        loaded,
        workspace_root,
        config_dir,
        require_profile,
    )
}

fn runtime_function_from_loaded_with_profile_policy(
    loaded: LoadedFunction,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
    require_profile: bool,
) -> Result<RuntimeFunction> {
    let subagent_specs = resolve_runtime_subagent_specs(
        &loaded,
        workspace_root.as_ref(),
        config_dir.as_ref(),
        require_profile,
    )?;
    Ok(runtime_function_from_loaded(loaded, subagent_specs))
}

pub(crate) fn runtime_function_from_loaded(
    loaded: LoadedFunction,
    subagent_specs: BTreeMap<String, RuntimeSubagentSpec>,
) -> RuntimeFunction {
    let selected_subagents = loaded.front_matter.selected_subagents().to_vec();
    RuntimeFunction {
        reference: loaded.locator.reference.clone(),
        source: loaded.locator.source,
        path: loaded.locator.path.clone(),
        id: loaded.front_matter.id().to_owned(),
        title: loaded.front_matter.title.clone(),
        description: loaded.front_matter.description.clone(),
        model_profile: loaded.front_matter.model_profile().map(str::to_string),
        generation_policy: loaded.front_matter.runtime.as_ref().map(|runtime| {
            agl_model::GenerationPolicy::greedy(
                runtime.max_output_tokens.unwrap_or(256),
                runtime.stop_rules.clone(),
                runtime.structured_generation,
                runtime.repair_malformed_tool_calls,
            )
            .expect("validated Function runtime must produce a generation policy")
        }),
        tool_mode: loaded.front_matter.runtime_tool_mode(),
        tool_policy: loaded.front_matter.tool_policy(),
        max_output_tokens: loaded.front_matter.runtime_max_output_tokens(),
        max_tool_calls: loaded.front_matter.runtime_max_tool_calls(),
        skills: loaded.front_matter.selected_skills().to_vec(),
        extensions: loaded
            .front_matter
            .required_extensions()
            .into_iter()
            .map(|id| id.as_str().to_string())
            .collect(),
        memory_enabled: loaded.front_matter.enables_memory_context(),
        system_prompt_path: loaded.system_prompt_path.clone(),
        runtime_identity_validation: loaded.front_matter.runtime_identity_validation(),
        subagents: loaded
            .subagents
            .iter()
            .filter(|subagent| selected_subagents.contains(&subagent.front_matter.id))
            .map(|subagent| RuntimeSubagent {
                id: subagent.front_matter.id.clone(),
                title: subagent.front_matter.title.clone(),
                description: subagent.front_matter.description.clone(),
            })
            .collect(),
        subagent_specs,
        delegation: loaded.front_matter.delegation.clone(),
        context: render_function_context(&loaded),
    }
}
