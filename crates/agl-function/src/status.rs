use std::path::{Path, PathBuf};

use serde::Serialize;

#[cfg(test)]
use crate::loader::load_function;
#[cfg(test)]
use crate::locator::{looks_like_path, resolve_function_package};
use crate::manifest::FunctionToolPolicy;
use crate::subagent::RuntimeSubagent;
#[cfg(test)]
use crate::validation::is_valid_function_id;
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
    pub tool_policy: Option<FunctionToolPolicy>,
    pub skills: Vec<String>,
    pub subagents: Vec<RuntimeSubagent>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub next_steps: Vec<String>,
}

#[cfg(test)]
pub fn function_status(
    reference: &str,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
) -> FunctionStatusReport {
    function_status_with_model_bindings(reference, workspace_root, config_dir, None)
}

#[cfg(test)]
pub fn function_status_with_model_bindings(
    reference: &str,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
    model_bindings: Option<&Path>,
) -> FunctionStatusReport {
    let workspace_root = workspace_root.as_ref();
    let config_dir = config_dir.as_ref();
    let mut report = empty_function_status(reference);

    let locator = match resolve_function_package(reference, workspace_root, config_dir) {
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
    populate_function_status(report, loaded, workspace_root, config_dir, model_bindings)
}

pub fn function_status_from_loaded(
    reference: &str,
    loaded: crate::LoadedFunction,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
    model_bindings: Option<&Path>,
) -> FunctionStatusReport {
    let mut report = empty_function_status(reference);
    report.source = Some(loaded.locator.source.as_str().to_owned());
    report.path = Some(loaded.locator.path.clone());
    populate_function_status(
        report,
        loaded,
        workspace_root.as_ref(),
        config_dir.as_ref(),
        model_bindings,
    )
}

fn empty_function_status(reference: &str) -> FunctionStatusReport {
    FunctionStatusReport {
        reference: reference.to_string(),
        state: "invalid".to_string(),
        source: None,
        path: None,
        system_prompt_path: None,
        id: None,
        title: None,
        profile: None,
        tool_policy: None,
        skills: Vec::new(),
        subagents: Vec::new(),
        warnings: Vec::new(),
        errors: Vec::new(),
        next_steps: Vec::new(),
    }
}

fn populate_function_status(
    mut report: FunctionStatusReport,
    loaded: crate::LoadedFunction,
    _workspace_root: &Path,
    _config_dir: &Path,
    _model_bindings: Option<&Path>,
) -> FunctionStatusReport {
    report.id = Some(loaded.front_matter.id().to_owned());
    report.title = Some(loaded.front_matter.title.clone());
    report.system_prompt_path = Some(loaded.system_prompt_path.clone());
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    use agl_config::{ModelBinding, ModelBindings, ModelId};

    use super::*;

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn staged_model_bindings_are_used_for_static_status() {
        let root = std::env::temp_dir().join(format!(
            "agl-function-status-staged-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let workspace = root.join("workspace");
        let config = root.join("config");
        std::fs::create_dir_all(&workspace).unwrap();
        let main = root.join("main.gguf");
        let projector = root.join("projector.gguf");
        std::fs::write(&main, b"model").unwrap();
        std::fs::write(&projector, b"projector").unwrap();
        let staged = root.join("staged-models.toml");
        agl_config::write_model_bindings(
            &staged,
            &ModelBindings {
                version: 1,
                models: BTreeMap::from([
                    (
                        ModelId::new("gemma4-e4b").unwrap(),
                        ModelBinding { path: main.clone() },
                    ),
                    (
                        ModelId::new("gemma4-e4b-mmproj").unwrap(),
                        ModelBinding {
                            path: projector.clone(),
                        },
                    ),
                ]),
            },
        )
        .unwrap();

        let report =
            function_status_with_model_bindings("gemma4-e4b", &workspace, &config, Some(&staged));
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(report.profile.as_deref(), Some("gpu-rx7900xtx-32768"));
        let _ = std::fs::remove_dir_all(root);
    }
}
