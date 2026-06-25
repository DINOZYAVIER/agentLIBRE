use std::path::PathBuf;

use agl_chat::InferenceOptions;
use agl_runtime::AgentLibrePaths;

pub const DEFAULT_SOCKET_FILE: &str = "agl.sock";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonOptions {
    pub socket_path: PathBuf,
    pub inference: InferenceOptions,
}

impl DaemonOptions {
    pub fn new(paths: &AgentLibrePaths, inference: InferenceOptions) -> Self {
        Self {
            socket_path: default_socket_path(paths),
            inference,
        }
    }
}

pub fn default_socket_path(paths: &AgentLibrePaths) -> PathBuf {
    paths.state_dir.join("daemon").join(DEFAULT_SOCKET_FILE)
}
