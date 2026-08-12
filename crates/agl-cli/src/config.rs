use std::path::PathBuf;

use agl_runtime::{AgentLibrePaths, AgentLibreRuntimeConfig, write_default_runtime_config};
use anyhow::{Result, bail};
use serde::Serialize;

use crate::args::{ConfigCommand, ConfigStatusOptions};

pub(crate) fn run_config(command: ConfigCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    match command {
        ConfigCommand::Paths => {
            for (name, path) in config_paths(runtime) {
                println!("{name}={}", path.display());
            }
            Ok(())
        }
        ConfigCommand::Status(options) => run_config_status(options, runtime),
        ConfigCommand::Init { force } => {
            let path = runtime.paths.runtime_config_path();
            write_default_runtime_config(&path, force)?;
            println!("wrote {}", path.display());
            Ok(())
        }
    }
}

fn config_paths(runtime: &AgentLibreRuntimeConfig) -> Vec<(&'static str, PathBuf)> {
    vec![
        ("config_dir", runtime.paths.config_dir.clone()),
        ("data_dir", runtime.paths.data_dir.clone()),
        ("state_dir", runtime.paths.state_dir.clone()),
        ("cache_dir", runtime.paths.cache_dir.clone()),
        ("runtime_config", runtime.paths.runtime_config_path()),
        ("app_log", runtime.paths.app_log_path()),
        ("inference_log", runtime.paths.inference_log_path()),
        ("sessions_root", runtime.paths.sessions_root()),
    ]
}

fn run_config_status(
    options: ConfigStatusOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let report = build_config_status_report(runtime);
    crate::print_json_or(options.json, &report, || {
        print_config_status_report(&report)
    })?;
    if options.strict && report.runtime_config.error.is_some() {
        bail!("agentLIBRE config status is not healthy");
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct ConfigStatusReport {
    paths: ConfigStatusPaths,
    runtime_config: RuntimeConfigStatus,
    logs: LogStatus,
    skill_trust_store: FileStatus,
    workspace_root: Option<PathBuf>,
    store_root: PathBuf,
    sessions_root: PathBuf,
    next_steps: Vec<String>,
}

#[derive(Debug, Serialize)]
struct ConfigStatusPaths {
    config_dir: PathBuf,
    data_dir: PathBuf,
    state_dir: PathBuf,
    cache_dir: PathBuf,
}

#[derive(Debug, Serialize)]
struct RuntimeConfigStatus {
    path: PathBuf,
    exists: bool,
    status: &'static str,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct LogStatus {
    app_log: FileStatus,
    inference_log: FileStatus,
}

#[derive(Debug, Serialize)]
struct FileStatus {
    path: PathBuf,
    exists: bool,
}

fn build_config_status_report(runtime: &AgentLibreRuntimeConfig) -> ConfigStatusReport {
    let runtime_config = runtime_config_status(&runtime.paths);
    let resolved_runtime = AgentLibreRuntimeConfig::from_paths(runtime.paths.clone()).ok();
    let logs = LogStatus {
        app_log: file_status(runtime.paths.app_log_path()),
        inference_log: file_status(runtime.paths.inference_log_path()),
    };
    let skill_trust_store = file_status(runtime.paths.state_dir.join("skill-trust.toml"));
    let mut next_steps = Vec::new();
    if runtime_config.error.is_some() {
        next_steps.push(format!(
            "fix or replace runtime config: {}",
            runtime_config.path.display()
        ));
    } else if !runtime_config.exists {
        next_steps.push("optional: agl config init".to_owned());
    }
    next_steps.push("list usable skills: agl skill list --trusted-only".to_owned());
    next_steps.push("verify workspace skills: agl skill verify".to_owned());

    ConfigStatusReport {
        paths: ConfigStatusPaths {
            config_dir: runtime.paths.config_dir.clone(),
            data_dir: runtime.paths.data_dir.clone(),
            state_dir: runtime.paths.state_dir.clone(),
            cache_dir: runtime.paths.cache_dir.clone(),
        },
        runtime_config,
        logs,
        skill_trust_store,
        workspace_root: resolved_runtime
            .as_ref()
            .and_then(|runtime| runtime.resolve_workspace_root(None).ok()),
        store_root: runtime.paths.store_root(),
        sessions_root: runtime.paths.sessions_root(),
        next_steps,
    }
}

fn runtime_config_status(paths: &AgentLibrePaths) -> RuntimeConfigStatus {
    let path = paths.runtime_config_path();
    let exists = path.exists();
    match AgentLibreRuntimeConfig::from_paths(paths.clone()) {
        Ok(_) => RuntimeConfigStatus {
            path,
            exists,
            status: if exists { "loaded" } else { "default" },
            error: None,
        },
        Err(error) => RuntimeConfigStatus {
            path,
            exists,
            status: "invalid",
            error: Some(format!("{error:#}")),
        },
    }
}

fn file_status(path: PathBuf) -> FileStatus {
    FileStatus {
        exists: path.exists(),
        path,
    }
}

fn print_config_status_report(report: &ConfigStatusReport) {
    println!("config_dir={}", report.paths.config_dir.display());
    println!("data_dir={}", report.paths.data_dir.display());
    println!("state_dir={}", report.paths.state_dir.display());
    println!("cache_dir={}", report.paths.cache_dir.display());
    println!(
        "runtime_config path={} exists={} status={}",
        report.runtime_config.path.display(),
        report.runtime_config.exists,
        report.runtime_config.status
    );
    if let Some(error) = &report.runtime_config.error {
        println!("runtime_config.error={error}");
    }
    println!(
        "log app path={} exists={}",
        report.logs.app_log.path.display(),
        report.logs.app_log.exists
    );
    println!(
        "log inference path={} exists={}",
        report.logs.inference_log.path.display(),
        report.logs.inference_log.exists
    );
    println!(
        "skill_trust_store path={} exists={}",
        report.skill_trust_store.path.display(),
        report.skill_trust_store.exists
    );
    if let Some(workspace_root) = &report.workspace_root {
        println!("workspace_root={}", workspace_root.display());
    }
    println!("store_root={}", report.store_root.display());
    println!("sessions_root={}", report.sessions_root.display());
    for next_step in &report.next_steps {
        println!("next_step={next_step}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_paths_have_no_obsolete_inference_profile() {
        let runtime =
            AgentLibreRuntimeConfig::from_paths(AgentLibrePaths::from_agl_home("/tmp/agl-home"))
                .unwrap();
        let paths = config_paths(&runtime);
        assert!(paths.iter().any(|(name, _)| *name == "runtime_config"));
        assert!(
            !paths
                .iter()
                .any(|(name, _)| *name == "local_inference_config")
        );
    }
}
