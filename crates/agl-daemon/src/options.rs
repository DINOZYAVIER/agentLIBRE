use std::path::PathBuf;

use agl_chat::InferenceOptions;
use agl_runtime::AgentLibrePaths;

use crate::ListenerSource;

pub const DEFAULT_SOCKET_FILE: &str = "agl.sock";
pub const DEFAULT_CRON_INTERVAL_SECONDS: u64 = 60;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonOptions {
    pub listener_source: ListenerSource,
    pub inference: InferenceOptions,
    pub cron_interval_seconds: u64,
}

impl DaemonOptions {
    pub fn new(paths: &AgentLibrePaths, inference: InferenceOptions) -> Self {
        Self {
            listener_source: ListenerSource::Bind(default_socket_path(paths)),
            inference,
            cron_interval_seconds: DEFAULT_CRON_INTERVAL_SECONDS,
        }
    }
}

pub fn default_socket_path(paths: &AgentLibrePaths) -> PathBuf {
    paths.state_dir.join("daemon").join(DEFAULT_SOCKET_FILE)
}
