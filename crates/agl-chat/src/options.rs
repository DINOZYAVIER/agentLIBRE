use std::path::PathBuf;

use agl_ids::SessionId;

pub use agl_capabilities::ToolAccessMode;

pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferenceOptions {
    pub config: Option<PathBuf>,
    pub function_ref: Option<String>,
    pub artifact_root: Option<PathBuf>,
    pub workspace_root: Option<PathBuf>,
    pub max_output_tokens: u32,
    pub tool_mode: ToolAccessMode,
    pub skills: Vec<String>,
    pub memory: bool,
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
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ChatOptions {
    pub inference: InferenceOptions,
    pub workspace_root: Option<PathBuf>,
    pub session_id: Option<SessionId>,
    pub no_history: bool,
    pub new_session: bool,
}
