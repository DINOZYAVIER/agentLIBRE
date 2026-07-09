use agl_functions::{FunctionSource, list_functions, workspace_functions_root};
use agl_repo::{
    DEFAULT_FUNCTION, RepoArtifactSourceOverride as AglRepoArtifactSourceOverride,
    RepoInitOptions as AglRepoInitOptions, init_repo_workspace,
};
use agl_runtime::AgentLibreRuntimeConfig;
use anyhow::{Context, Result};

use crate::args::RepoInitOptions;
use crate::repo::print_repo_init_report;

pub(crate) fn run_init(options: RepoInitOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "init", "starting bootstrap init");
    let start = std::env::current_dir().context("failed to resolve current directory")?;
    let dry_run = options.dry_run;
    let report = init_repo_workspace(start, &repo_init_options(options))?;
    print_repo_init_report(&report);

    let workspace_root = report.workspace_root.clone();
    let functions_root = workspace_functions_root(&workspace_root);
    let function_root_action = ensure_functions_root(&functions_root, dry_run)?;
    println!(
        "bootstrap.functions_root.path={}",
        display_workspace_path(&workspace_root, &functions_root)
    );
    println!("bootstrap.functions_root.action={function_root_action}");
    println!("bootstrap.default_function={DEFAULT_FUNCTION}");

    let builtin_functions = list_functions(&workspace_root, &runtime.paths.config_dir)?
        .into_iter()
        .filter(|function| function.source == FunctionSource::Builtin)
        .collect::<Vec<_>>();
    for function in &builtin_functions {
        println!("bootstrap.builtin_function={}", function.id);
    }
    println!("next_step=agl function status {DEFAULT_FUNCTION}");
    println!("next_step=agl run --prompt \"Reply with init-ok\"");

    Ok(())
}

fn repo_init_options(options: RepoInitOptions) -> AglRepoInitOptions {
    AglRepoInitOptions {
        profile: options.profile,
        profile_file: options.profile_file,
        artifact_sources: options
            .artifact_sources
            .into_iter()
            .map(|source| AglRepoArtifactSourceOverride {
                name: source.name,
                url: source.url,
                rev: source.rev,
            })
            .collect(),
        skills_url: options.skills_url,
        skills_rev: options.skills_rev,
        tasks_url: options.tasks_url,
        tasks_rev: options.tasks_rev,
        dry_run: options.dry_run,
        force: options.force,
    }
}

fn ensure_functions_root(path: &std::path::Path, dry_run: bool) -> Result<&'static str> {
    if path.is_dir() {
        return Ok("exists");
    }
    if dry_run {
        return Ok("would_create_dir");
    }
    std::fs::create_dir_all(path)
        .with_context(|| format!("failed to create functions root {}", path.display()))?;
    Ok("created_dir")
}

fn display_workspace_path(workspace_root: &std::path::Path, path: &std::path::Path) -> String {
    path.strip_prefix(workspace_root)
        .unwrap_or(path)
        .display()
        .to_string()
}
