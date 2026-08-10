use agl_kernel::ArtifactId;
use agl_repo::ArtifactGitRepository;
use agl_runtime::AgentLibreRuntimeConfig;
use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::args::{ArtifactCommand, ArtifactStatusOptions};

#[derive(Serialize)]
struct ArtifactStatusReport {
    artifact_id: String,
    valid: bool,
    path: Option<String>,
    remote_url: Option<String>,
    gitlink: Option<String>,
    child_head: Option<String>,
    error: Option<String>,
}

pub(crate) fn run_artifact(
    command: ArtifactCommand,
    _runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let (options, verify) = match command {
        ArtifactCommand::Status(options) => (options, false),
        ArtifactCommand::Verify(options) => (options, true),
    };
    let report = inspect_core_tasks(&options)?;
    crate::print_json_or(options.json, &report, || print_report(&report))?;
    if verify && !report.valid {
        bail!(
            "Artifact binding {} is invalid: {}",
            report.artifact_id,
            report.error.as_deref().unwrap_or("unknown error")
        );
    }
    Ok(())
}

fn inspect_core_tasks(options: &ArtifactStatusOptions) -> Result<ArtifactStatusReport> {
    let id = ArtifactId::new("core.repo:tasks").expect("fixed Artifact ID is valid");
    if let Some(filter) = &options.artifact
        && filter != id.as_str()
    {
        bail!("unknown declared Artifact `{filter}`");
    }
    let descriptor = agl_core_tools::repo::declaration();
    let declaration = descriptor
        .artifact(&id)
        .context("core.repo:tasks declaration is missing")?;
    let root = std::env::current_dir().context("failed to resolve current directory")?;
    match ArtifactGitRepository::open(&root).and_then(|repo| repo.verify_binding(declaration)) {
        Ok(binding) => Ok(ArtifactStatusReport {
            artifact_id: id.to_string(),
            valid: true,
            path: Some(binding.submodule_path().to_string_lossy().into_owned()),
            remote_url: Some(binding.remote_url().to_owned()),
            gitlink: Some(binding.gitlink().to_owned()),
            child_head: Some(binding.child_head().to_owned()),
            error: None,
        }),
        Err(error) => Ok(ArtifactStatusReport {
            artifact_id: id.to_string(),
            valid: false,
            path: None,
            remote_url: None,
            gitlink: None,
            child_head: None,
            error: Some(error.to_string()),
        }),
    }
}

fn print_report(report: &ArtifactStatusReport) {
    println!("artifact_id={}", report.artifact_id);
    println!("valid={}", report.valid);
    if let Some(path) = &report.path {
        println!("path={path}");
    }
    if let Some(error) = &report.error {
        println!("error={error}");
    }
}
