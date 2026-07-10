mod config;
mod features;
mod paths;
mod tracing_setup;

pub use config::{
    AgentLibreHistoryConfig, AgentLibreLogFormat, AgentLibreLoggingConfig, AgentLibreRuntimeConfig,
    AgentLibreStderrLogMode, AgentLibreWorkspaceConfig, DEFAULT_RUNTIME_CONFIG_TOML,
    resolve_workspace_root_from, write_default_runtime_config,
};
pub use features::{
    DEFAULT_RUNTIME_FEATURE_CONTEXT_CHAR_CAP, RenderedRuntimeFeatureContext, RuntimeFeature,
    RuntimeFeatureContextEvidence, RuntimeFeatureRenderOptions, first_party_runtime_features,
    render_runtime_feature_context, runtime_feature_registry_hash,
};
pub use paths::AgentLibrePaths;
pub use tracing_setup::{
    AgentLibreProcessMode, TracingGuards, init_tracing, logged_message_fields,
};
