use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry, fmt};

use crate::{
    AgentLibreLogFormat, AgentLibreLoggingConfig, AgentLibrePaths, AgentLibreStderrLogMode,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentLibreProcessMode {
    Interactive,
    Batch,
}

pub struct TracingGuards {
    _guards: Vec<WorkerGuard>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LoggedMessageFields {
    pub role: String,
    pub content_bytes: usize,
    pub content: Option<String>,
}

pub fn logged_message_fields(
    role: impl Into<String>,
    content: &str,
    include_message_text: bool,
) -> LoggedMessageFields {
    LoggedMessageFields {
        role: role.into(),
        content_bytes: content.len(),
        content: include_message_text.then(|| content.to_string()),
    }
}

pub fn init_tracing(
    paths: &AgentLibrePaths,
    config: &AgentLibreLoggingConfig,
    process_mode: AgentLibreProcessMode,
) -> Result<TracingGuards> {
    let mut guards = Vec::new();
    let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync>> = Vec::new();

    if config.file {
        let log_dir = paths.state_dir.join("logs");
        std::fs::create_dir_all(&log_dir)
            .with_context(|| format!("failed to create log directory {}", log_dir.display()))?;
        let app_appender = tracing_appender::rolling::never(&log_dir, "agentLIBRE.log");
        let (app_writer, app_guard) = tracing_appender::non_blocking(app_appender);
        guards.push(app_guard);
        layers.push(format_layer(config.format, app_writer).with_filter_boxed(log_filter(config)));

        let inference_appender = tracing_appender::rolling::never(&log_dir, "inference.log");
        let (inference_writer, inference_guard) =
            tracing_appender::non_blocking(inference_appender);
        guards.push(inference_guard);
        let inference_filter =
            Targets::new().with_target("agentlibre::inference", LevelFilter::TRACE);
        layers.push(
            format_layer(config.format, inference_writer).with_filter_boxed(inference_filter),
        );
    }

    if stderr_logs_enabled(config.stderr, process_mode) {
        layers.push(
            format_layer(config.format, std::io::stderr).with_filter_boxed(log_filter(config)),
        );
    }

    tracing_subscriber::registry()
        .with(layers)
        .try_init()
        .context("failed to initialize tracing subscriber")?;

    Ok(TracingGuards { _guards: guards })
}

fn stderr_logs_enabled(mode: AgentLibreStderrLogMode, process_mode: AgentLibreProcessMode) -> bool {
    match mode {
        AgentLibreStderrLogMode::Auto => process_mode == AgentLibreProcessMode::Interactive,
        AgentLibreStderrLogMode::Always => true,
        AgentLibreStderrLogMode::Never => false,
    }
}

trait BoxLayerExt {
    fn with_filter_boxed<F>(self, filter: F) -> Box<dyn Layer<Registry> + Send + Sync>
    where
        F: tracing_subscriber::layer::Filter<Registry> + Send + Sync + 'static;
}

impl<L> BoxLayerExt for L
where
    L: Layer<Registry> + Send + Sync + 'static,
{
    fn with_filter_boxed<F>(self, filter: F) -> Box<dyn Layer<Registry> + Send + Sync>
    where
        F: tracing_subscriber::layer::Filter<Registry> + Send + Sync + 'static,
    {
        Box::new(self.with_filter(filter))
    }
}

fn format_layer<W>(format: AgentLibreLogFormat, writer: W) -> Box<dyn Layer<Registry> + Send + Sync>
where
    W: for<'writer> fmt::MakeWriter<'writer> + Send + Sync + 'static,
{
    match format {
        AgentLibreLogFormat::Compact => {
            Box::new(fmt::layer().compact().with_ansi(false).with_writer(writer))
        }
        AgentLibreLogFormat::Json => {
            Box::new(fmt::layer().json().with_ansi(false).with_writer(writer))
        }
    }
}

fn log_filter(config: &AgentLibreLoggingConfig) -> EnvFilter {
    if std::env::var_os("AGL_LOG").is_some() {
        EnvFilter::from_env("AGL_LOG")
    } else if std::env::var_os("RUST_LOG").is_some() {
        EnvFilter::from_default_env()
    } else {
        EnvFilter::new(&config.level)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logged_message_fields_redact_content_by_default() {
        let fields = logged_message_fields("user", "secret prompt", false);

        assert_eq!(fields.role, "user");
        assert_eq!(fields.content_bytes, 13);
        assert_eq!(fields.content, None);
    }

    #[test]
    fn logged_message_fields_can_include_content_explicitly() {
        let fields = logged_message_fields("assistant", "hello", true);

        assert_eq!(fields.content.as_deref(), Some("hello"));
    }

    #[test]
    fn auto_stderr_mode_follows_process_mode() {
        assert!(stderr_logs_enabled(
            AgentLibreStderrLogMode::Auto,
            AgentLibreProcessMode::Interactive
        ));
        assert!(!stderr_logs_enabled(
            AgentLibreStderrLogMode::Auto,
            AgentLibreProcessMode::Batch
        ));
    }

    #[test]
    fn explicit_stderr_modes_override_process_mode() {
        assert!(!stderr_logs_enabled(
            AgentLibreStderrLogMode::Never,
            AgentLibreProcessMode::Interactive
        ));
        assert!(stderr_logs_enabled(
            AgentLibreStderrLogMode::Always,
            AgentLibreProcessMode::Batch
        ));
    }

    #[test]
    fn file_disabled_does_not_require_log_directory() {
        let paths = AgentLibrePaths::from_agl_home(
            std::env::temp_dir().join(format!("agl-runtime-no-file-logs-{}", std::process::id())),
        );
        let _ = std::fs::remove_dir_all(&paths.state_dir);
        let config = AgentLibreLoggingConfig {
            file: false,
            stderr: AgentLibreStderrLogMode::Never,
            ..AgentLibreLoggingConfig::default()
        };

        init_tracing(&paths, &config, AgentLibreProcessMode::Batch).unwrap();

        assert!(!paths.state_dir.join("logs").exists());
        let _ = std::fs::remove_dir_all(&paths.state_dir);
    }
}
