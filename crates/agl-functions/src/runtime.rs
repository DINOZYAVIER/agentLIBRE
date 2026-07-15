use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::Serialize;

use crate::loader::{LoadedFunction, load_function};
use crate::locator::{FunctionSource, resolve_function_reference, resolve_profile};
use crate::manifest::{
    FunctionDelegationBudget, FunctionToolMode, FunctionToolPolicy, RuntimeIdentityValidation,
};
use crate::render::render_function_context;
use crate::subagent::{
    RuntimeDelegationPlan, RuntimeSubagent, RuntimeSubagentSpec, resolve_runtime_subagent_specs,
};
use crate::validation::join_paths;
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RuntimeFunction {
    pub reference: String,
    pub source: FunctionSource,
    pub path: PathBuf,
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub model_profile: Option<String>,
    pub profile_path: Option<PathBuf>,
    pub inference_config_path: Option<PathBuf>,
    pub inference_config_toml: Option<String>,
    pub tool_mode: Option<FunctionToolMode>,
    pub tool_policy: Option<FunctionToolPolicy>,
    pub max_output_tokens: Option<u32>,
    pub skills: Vec<String>,
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

pub fn resolve_runtime_function(
    reference: &str,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
) -> Result<RuntimeFunction> {
    resolve_runtime_function_with_profile_policy(reference, workspace_root, config_dir, true)
}

pub fn resolve_runtime_function_allow_missing_profile(
    reference: &str,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
) -> Result<RuntimeFunction> {
    resolve_runtime_function_with_profile_policy(reference, workspace_root, config_dir, false)
}

pub(crate) fn resolve_runtime_function_with_profile_policy(
    reference: &str,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
    require_profile: bool,
) -> Result<RuntimeFunction> {
    let locator = resolve_function_reference(reference, &workspace_root, &config_dir)?;
    let loaded = load_function(locator)?;
    let profile_path = if let Some(profile) = loaded.front_matter.model_profile() {
        let resolution = resolve_profile(profile, &workspace_root, &config_dir)?;
        match resolution.selected_path {
            Some(path) => Some(path),
            None if require_profile => {
                bail!(
                    "inference profile `{profile}` not found; checked {}",
                    join_paths(&resolution.candidates)
                );
            }
            None => None,
        }
    } else {
        None
    };
    let subagent_specs = resolve_runtime_subagent_specs(
        &loaded,
        workspace_root.as_ref(),
        config_dir.as_ref(),
        require_profile,
    )?;
    Ok(runtime_function_from_loaded(
        loaded,
        profile_path,
        subagent_specs,
    ))
}

pub(crate) fn runtime_function_from_loaded(
    loaded: LoadedFunction,
    profile_path: Option<PathBuf>,
    subagent_specs: BTreeMap<String, RuntimeSubagentSpec>,
) -> RuntimeFunction {
    let selected_subagents = loaded.front_matter.selected_subagents().to_vec();
    RuntimeFunction {
        reference: loaded.locator.reference.clone(),
        source: loaded.locator.source,
        path: loaded.locator.path.clone(),
        id: loaded.front_matter.id.clone(),
        title: loaded.front_matter.title.clone(),
        description: loaded.front_matter.description.clone(),
        model_profile: loaded.front_matter.model_profile().map(str::to_string),
        profile_path: profile_path.clone(),
        inference_config_path: loaded
            .inference_config_path
            .clone()
            .or_else(|| profile_path.clone()),
        inference_config_toml: loaded.inference_config_toml.clone(),
        tool_mode: loaded.front_matter.runtime_tool_mode(),
        tool_policy: loaded.front_matter.tool_policy(),
        max_output_tokens: loaded.front_matter.runtime_max_output_tokens(),
        skills: loaded.front_matter.selected_skills().to_vec(),
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
