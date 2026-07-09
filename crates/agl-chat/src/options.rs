use std::path::PathBuf;

pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 256;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ToolAccessMode {
    #[default]
    ReadOnly,
    Write,
    Execute,
    Approve,
    Admin,
}

impl ToolAccessMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Write => "write",
            Self::Execute => "execute",
            Self::Approve => "approve",
            Self::Admin => "admin",
        }
    }

    pub fn operation_ceiling(self) -> Option<agl_tools::ToolOperationKind> {
        match self {
            Self::ReadOnly => None,
            Self::Write => Some(agl_tools::ToolOperationKind::Write),
            Self::Execute => Some(agl_tools::ToolOperationKind::Execute),
            Self::Approve => Some(agl_tools::ToolOperationKind::Approve),
            Self::Admin => Some(agl_tools::ToolOperationKind::Admin),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferenceOptions {
    pub config: Option<PathBuf>,
    pub function_ref: Option<String>,
    pub artifact_root: Option<PathBuf>,
    pub run_id: Option<String>,
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
            run_id: None,
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
    pub session_id: Option<String>,
    pub no_history: bool,
    pub new_session: bool,
}
