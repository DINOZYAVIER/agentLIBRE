use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::loader::load_function;
use crate::locator::{
    FunctionSource, looks_like_path, resolve_function_reference, resolve_profile,
};
use crate::manifest::FunctionToolPolicy;
use crate::subagent::RuntimeSubagent;
use crate::validation::{is_valid_function_id, join_paths};
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FunctionStatusReport {
    pub reference: String,
    pub state: String,
    pub source: Option<String>,
    pub path: Option<PathBuf>,
    pub system_prompt_path: Option<PathBuf>,
    pub id: Option<String>,
    pub title: Option<String>,
    pub profile: Option<String>,
    pub profile_path: Option<PathBuf>,
    pub inference_config_path: Option<PathBuf>,
    pub inference_config_embedded: bool,
    pub inference_model_id: Option<String>,
    pub inference_multimodal_projector_id: Option<String>,
    pub inference_draft_model_id: Option<String>,
    pub inference_model_path: Option<PathBuf>,
    pub inference_multimodal_projector_path: Option<PathBuf>,
    pub inference_draft_model_path: Option<PathBuf>,
    pub inference_model_exists: Option<bool>,
    pub tool_policy: Option<FunctionToolPolicy>,
    pub skills: Vec<String>,
    pub subagents: Vec<RuntimeSubagent>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub next_steps: Vec<String>,
}

pub fn function_status(
    reference: &str,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
) -> FunctionStatusReport {
    let workspace_root = workspace_root.as_ref();
    let config_dir = config_dir.as_ref();
    let mut report = FunctionStatusReport {
        reference: reference.to_string(),
        state: "invalid".to_string(),
        source: None,
        path: None,
        system_prompt_path: None,
        id: None,
        title: None,
        profile: None,
        profile_path: None,
        inference_config_path: None,
        inference_config_embedded: false,
        inference_model_id: None,
        inference_multimodal_projector_id: None,
        inference_draft_model_id: None,
        inference_model_path: None,
        inference_multimodal_projector_path: None,
        inference_draft_model_path: None,
        inference_model_exists: None,
        tool_policy: None,
        skills: Vec::new(),
        subagents: Vec::new(),
        warnings: Vec::new(),
        errors: Vec::new(),
        next_steps: Vec::new(),
    };

    let locator = match resolve_function_reference(reference, workspace_root, config_dir) {
        Ok(locator) => locator,
        Err(err) => {
            report.errors.push(format!("{err:#}"));
            if !looks_like_path(reference) && is_valid_function_id(reference) {
                report
                    .next_steps
                    .push(format!("agl function init {reference} --workspace"));
            }
            return report;
        }
    };
    report.source = Some(locator.source.as_str().to_string());
    report.path = Some(locator.path.clone());

    let loaded = match load_function(locator) {
        Ok(loaded) => loaded,
        Err(err) => {
            report.errors.push(format!("{err:#}"));
            return report;
        }
    };
    report.id = Some(loaded.front_matter.id.clone());
    report.title = Some(loaded.front_matter.title.clone());
    report.system_prompt_path = Some(loaded.system_prompt_path.clone());
    report.inference_config_path = loaded.inference_config_path.clone();
    report.inference_config_embedded =
        loaded.locator.source == FunctionSource::Builtin && loaded.inference_config_toml.is_some();
    if let Some(config_toml) = loaded.inference_config_toml.as_deref() {
        let source_name = loaded
            .inference_config_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<function inference config>".to_string());
        match agl_config::load_inference_preset_from_str(&source_name, config_toml) {
            Ok(preset) => {
                report.inference_model_id = Some(preset.backend.model_id.to_string());
                report.inference_multimodal_projector_id = preset
                    .backend
                    .multimodal_projector_id
                    .as_ref()
                    .map(ToString::to_string);
                report.inference_draft_model_id = preset
                    .runtime
                    .mtp
                    .draft_model_id
                    .as_ref()
                    .map(ToString::to_string);
                let bindings_path = agl_config::model_bindings_path(config_dir);
                match agl_config::resolve_inference_preset(preset, &bindings_path) {
                    Ok(config) => {
                        report.inference_model_path = Some(config.backend.model);
                        report.inference_multimodal_projector_path =
                            config.backend.multimodal_projector;
                        report.inference_draft_model_path = config.runtime.mtp.draft_model;
                        report.inference_model_exists = Some(true);
                    }
                    Err(error) => {
                        report.warnings.push(format!("{error:#}"));
                        report.next_steps.push(format!(
                            "configure model ids in {}",
                            bindings_path.display()
                        ));
                    }
                }
            }
            Err(err) => report.errors.push(format!("{err:#}")),
        }
    }
    report.skills = loaded.front_matter.selected_skills().to_vec();
    report.tool_policy = loaded.front_matter.tool_policy();
    report.subagents = loaded
        .front_matter
        .selected_subagents()
        .iter()
        .filter_map(|id| {
            loaded
                .subagents
                .iter()
                .find(|subagent| &subagent.front_matter.id == id)
        })
        .map(|subagent| RuntimeSubagent {
            id: subagent.front_matter.id.clone(),
            title: subagent.front_matter.title.clone(),
            description: subagent.front_matter.description.clone(),
        })
        .collect();

    if let Some(profile) = loaded.front_matter.model_profile() {
        report.profile = Some(profile.to_string());
        match resolve_profile(profile, workspace_root, config_dir) {
            Ok(resolution) => {
                report.profile_path = resolution.selected_path.clone();
                match resolution.selected_path {
                    Some(path) if path.is_file() => {}
                    Some(path) => report.errors.push(format!(
                        "inference profile `{profile}` not found: {}",
                        path.display()
                    )),
                    None => report.errors.push(format!(
                        "inference profile `{profile}` not found; checked {}",
                        join_paths(&resolution.candidates)
                    )),
                }
            }
            Err(err) => report.errors.push(format!("{err:#}")),
        }
    }

    if report.errors.is_empty() {
        report.state = if report.warnings.is_empty() {
            "ok".to_string()
        } else {
            "warning".to_string()
        };
    }
    report
}
