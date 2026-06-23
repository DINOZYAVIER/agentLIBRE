mod config;
mod paths;
mod tracing_setup;

pub use config::{
    AgentLibreHistoryConfig, AgentLibreLogFormat, AgentLibreLoggingConfig, AgentLibreRuntimeConfig,
    AgentLibreStderrLogMode, AgentLibreWorkspaceConfig, DEFAULT_RUNTIME_CONFIG_TOML,
    resolve_workspace_root_from, write_default_runtime_config,
};
pub use paths::AgentLibrePaths;
pub use tracing_setup::{
    AgentLibreProcessMode, TracingGuards, init_tracing, logged_message_fields,
};
