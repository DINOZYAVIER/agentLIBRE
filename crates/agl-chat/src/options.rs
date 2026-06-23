use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToolAccessMode {
    #[default]
    ReadOnly,
    Write,
}

impl ToolAccessMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Write => "write",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferenceOptions {
    pub config: Option<PathBuf>,
    pub artifact_root: Option<PathBuf>,
    pub run_id: Option<String>,
    pub max_output_tokens: u32,
    pub tool_mode: ToolAccessMode,
    pub skills: Vec<String>,
}

impl Default for InferenceOptions {
    fn default() -> Self {
        Self {
            config: None,
            artifact_root: None,
            run_id: None,
            max_output_tokens: 256,
            tool_mode: ToolAccessMode::ReadOnly,
            skills: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ChatOptions {
    pub inference: InferenceOptions,
    pub workspace_root: Option<PathBuf>,
    pub session_id: Option<String>,
    pub no_history: bool,
    pub new_session: bool,
}
