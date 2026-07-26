use std::path::PathBuf;

use agl_ids::SessionId;
use serde::{Deserialize, Serialize};

pub use agl_capabilities::ToolAccessMode;

pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 256;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InferenceOptions {
    pub config: Option<PathBuf>,
    pub function_ref: Option<String>,
    pub artifact_root: Option<PathBuf>,
    pub workspace_root: Option<PathBuf>,
    pub max_output_tokens: u32,
    pub tool_mode: ToolAccessMode,
    pub skills: Vec<String>,
    pub memory: bool,
    #[serde(skip)]
    pub model_bindings_path: Option<PathBuf>,
    #[serde(skip)]
    pub model_bindings_override: Option<agl_config::ModelBindings>,
    #[serde(skip)]
    pub runtime_plan_override: Option<agl_model::RuntimePlan>,
}

impl Default for InferenceOptions {
    fn default() -> Self {
        Self {
            config: None,
            function_ref: None,
            artifact_root: None,
            workspace_root: None,
            max_output_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
            tool_mode: ToolAccessMode::ReadOnly,
            skills: Vec::new(),
            memory: false,
            model_bindings_path: None,
            model_bindings_override: None,
            runtime_plan_override: None,
        }
    }
}

impl InferenceOptions {
    #[doc(hidden)]
    pub fn with_model_bindings_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.model_bindings_path = Some(path.into());
        self
    }

    #[doc(hidden)]
    pub fn with_model_bindings_override(mut self, bindings: agl_config::ModelBindings) -> Self {
        self.model_bindings_override = Some(bindings);
        self
    }

    #[doc(hidden)]
    pub fn with_runtime_plan_override(mut self, plan: agl_model::RuntimePlan) -> Self {
        self.runtime_plan_override = Some(plan);
        self
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatOptions {
    pub inference: InferenceOptions,
    pub workspace_root: Option<PathBuf>,
    pub session_id: Option<SessionId>,
    pub no_history: bool,
    pub new_session: bool,
}
