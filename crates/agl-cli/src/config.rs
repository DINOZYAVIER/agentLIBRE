use std::path::{Path, PathBuf};

use agl_config::{BackendKind, LocalInferenceConfig, load_local_inference_config};
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
        (
            "local_inference_config",
            runtime.paths.default_local_inference_config(),
        ),
        ("app_log", runtime.paths.app_log_path()),
        ("inference_log", runtime.paths.inference_log_path()),
        ("sessions_root", runtime.paths.sessions_root()),
    ]
}

fn run_config_status(
    options: ConfigStatusOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let report = build_config_status_report(options.config.as_deref(), runtime);
    crate::print_json_or(options.json, &report, || {
        print_config_status_report(&report)
    })?;
    if options.strict && report.has_errors() {
        bail!("agentLIBRE config status is not healthy");
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct ConfigStatusReport {
    paths: ConfigStatusPaths,
    runtime_config: RuntimeConfigStatus,
    local_inference_config: LocalInferenceConfigStatus,
    logs: LogStatus,
    skill_trust_store: FileStatus,
    workspace_root: Option<PathBuf>,
    store_root: PathBuf,
    sessions_root: PathBuf,
    next_steps: Vec<String>,
}

impl ConfigStatusReport {
    fn has_errors(&self) -> bool {
        self.runtime_config.error.is_some() || self.local_inference_config.error.is_some()
    }
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
struct LocalInferenceConfigStatus {
    path: PathBuf,
    source: &'static str,
    exists: bool,
    status: &'static str,
    error: Option<String>,
    backend: Option<&'static str>,
    model_path: Option<PathBuf>,
    model_exists: Option<bool>,
    context_tokens: Option<u32>,
    gpu_layers: Option<u32>,
    threads: Option<u32>,
    prompt_skills: Vec<String>,
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

fn build_config_status_report(
    config_override: Option<&Path>,
    runtime: &AgentLibreRuntimeConfig,
) -> ConfigStatusReport {
    let runtime_config = runtime_config_status(&runtime.paths);
    let resolved_runtime = AgentLibreRuntimeConfig::from_paths(runtime.paths.clone()).ok();
    let local_inference_config = local_inference_config_status(config_override, &runtime.paths);
    let workspace_root = resolved_runtime
        .as_ref()
        .and_then(|runtime| runtime.resolve_workspace_root(None).ok());
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
        next_steps.push("optional: agl config init".to_string());
    }
    if local_inference_config.error.is_some() {
        next_steps.push(format!(
            "write a valid local inference profile or pass --config PATH: {}",
            local_inference_config.path.display()
        ));
    }
    if local_inference_config.model_exists == Some(false)
        && let Some(model_path) = &local_inference_config.model_path
    {
        next_steps.push(format!(
            "point [backend].model at an existing GGUF file: {}",
            model_path.display()
        ));
    }
    next_steps.push("list usable skills: agl skill list --trusted-only".to_string());
    next_steps.push("verify workspace skills: agl skill verify".to_string());
    next_steps.push("run with a skill: agl run --skill repo-status --prompt \"...\"".to_string());

    ConfigStatusReport {
        paths: ConfigStatusPaths {
            config_dir: runtime.paths.config_dir.clone(),
            data_dir: runtime.paths.data_dir.clone(),
            state_dir: runtime.paths.state_dir.clone(),
            cache_dir: runtime.paths.cache_dir.clone(),
        },
        runtime_config,
        local_inference_config,
        logs,
        skill_trust_store,
        workspace_root,
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
        Err(err) => RuntimeConfigStatus {
            path,
            exists,
            status: "invalid",
            error: Some(format!("{err:#}")),
        },
    }
}

fn local_inference_config_status(
    config_override: Option<&Path>,
    paths: &AgentLibrePaths,
) -> LocalInferenceConfigStatus {
    let (path, source) = resolve_local_inference_config_path(config_override, paths);
    let exists = path.exists();
    match load_local_inference_config(&path) {
        Ok(config) => local_inference_config_loaded(path, source, exists, config),
        Err(err) => LocalInferenceConfigStatus {
            path,
            source,
            exists,
            status: if exists { "invalid" } else { "missing" },
            error: Some(format!("{err:#}")),
            backend: None,
            model_path: None,
            model_exists: None,
            context_tokens: None,
            gpu_layers: None,
            threads: None,
            prompt_skills: Vec::new(),
        },
    }
}

fn resolve_local_inference_config_path(
    config_override: Option<&Path>,
    paths: &AgentLibrePaths,
) -> (PathBuf, &'static str) {
    if let Some(path) = config_override {
        return (path.to_path_buf(), "option");
    }
    if let Some(path) = std::env::var_os("AGL_LOCAL_INFERENCE_CONFIG")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
    {
        return (path, "env");
    }
    (paths.default_local_inference_config(), "default")
}

fn local_inference_config_loaded(
    path: PathBuf,
    source: &'static str,
    exists: bool,
    config: LocalInferenceConfig,
) -> LocalInferenceConfigStatus {
    let model_path = config.backend.model.clone();
    LocalInferenceConfigStatus {
        path,
        source,
        exists,
        status: "loaded",
        error: None,
        backend: Some(backend_kind(config.backend.kind)),
        model_exists: Some(model_path.exists()),
        model_path: Some(model_path),
        context_tokens: Some(config.runtime.context_tokens),
        gpu_layers: Some(config.runtime.gpu_layers),
        threads: Some(config.runtime.threads),
        prompt_skills: config.prompt.skills,
    }
}

fn backend_kind(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::LlamaCpp => "llama_cpp",
    }
}

fn file_status(path: PathBuf) -> FileStatus {
    let exists = path.exists();
    FileStatus { path, exists }
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
    print_local_inference_config_status(&report.local_inference_config);
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

fn print_local_inference_config_status(status: &LocalInferenceConfigStatus) {
    println!(
        "local_inference_config path={} source={} exists={} status={}",
        status.path.display(),
        status.source,
        status.exists,
        status.status
    );
    if let Some(error) = &status.error {
        println!("local_inference_config.error={error}");
    }
    if let Some(backend) = status.backend {
        println!("local_inference_config.backend={backend}");
    }
    if let Some(model_path) = &status.model_path {
        println!("local_inference_config.model_path={}", model_path.display());
    }
    if let Some(model_exists) = status.model_exists {
        println!("local_inference_config.model_exists={model_exists}");
    }
    if let Some(context_tokens) = status.context_tokens {
        println!("local_inference_config.context_tokens={context_tokens}");
    }
    if let Some(gpu_layers) = status.gpu_layers {
        println!("local_inference_config.gpu_layers={gpu_layers}");
    }
    if let Some(threads) = status.threads {
        println!("local_inference_config.threads={threads}");
    }
    if !status.prompt_skills.is_empty() {
        println!(
            "local_inference_config.prompt_skills={}",
            status.prompt_skills.join(",")
        );
    }
}

#[cfg(test)]
mod tests {
    use agl_runtime::AgentLibrePaths;

    use super::*;

    #[test]
    fn config_paths_include_runtime_files() {
        let runtime =
            AgentLibreRuntimeConfig::from_paths(AgentLibrePaths::from_agl_home("/tmp/agl-home"))
                .unwrap();

        let paths = config_paths(&runtime);

        assert!(paths.iter().any(|(name, _)| *name == "runtime_config"));
        assert!(paths.iter().any(|(name, _)| *name == "app_log"));
        assert!(paths.iter().any(|(name, _)| *name == "sessions_root"));
    }

    #[test]
    fn config_status_loads_explicit_inference_profile() {
        let root =
            std::env::temp_dir().join(format!("agl-cli-config-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let paths = AgentLibrePaths::from_agl_home(&root);
        std::fs::create_dir_all(paths.config_dir.join("inference")).unwrap();
        let model_path = root.join("model.gguf");
        std::fs::write(&model_path, "test model placeholder").unwrap();
        let config_path = paths.config_dir.join("inference").join("test.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"[backend]
kind = "llama_cpp"
model = "{}"

[runtime]
gpu_layers = 0
context_tokens = 4096
threads = 2

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"

[prompt]
skills = ["repo-status"]
"#,
                model_path.display()
            ),
        )
        .unwrap();
        let runtime = AgentLibreRuntimeConfig::from_paths(paths).unwrap();

        let report = build_config_status_report(Some(&config_path), &runtime);

        assert_eq!(report.local_inference_config.status, "loaded");
        assert_eq!(report.local_inference_config.source, "option");
        assert_eq!(report.local_inference_config.model_exists, Some(true));
        assert_eq!(
            report.local_inference_config.prompt_skills,
            vec!["repo-status".to_string()]
        );

        std::fs::remove_dir_all(root).unwrap();
    }
}
