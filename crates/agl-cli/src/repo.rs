use std::fs;
use std::path::Path;

use agl_artifact::ArtifactHandle;
use agl_kernel::ArtifactId;
use agl_package::{PackageRef, WorkspaceConfigReferences, WorkspaceManifest, WorkspacePolicy};
use anyhow::{Context, Result, bail, ensure};

use crate::args::{RepoCommand, RepoHooksOptions, RepoInitOptions, TaskSpecVerifyOptions};

pub(crate) fn run_repo(command: RepoCommand) -> Result<()> {
    match command {
        RepoCommand::Init(options) => init_workspace(options),
        RepoCommand::VerifyTasks(options) => verify_tasks(options),
        RepoCommand::InstallHooks(options) => install_hooks(options),
    }
}

fn init_workspace(options: RepoInitOptions) -> Result<()> {
    let root = agl_repo::resolve_repo_root(std::env::current_dir()?)?;
    let path = root.join(agl_repo::WORKSPACE_MANIFEST_PATH);
    ensure!(
        options.force || !path.exists(),
        "workspace manifest already exists"
    );
    let manifest = WorkspaceManifest {
        version: WorkspaceManifest::VERSION,
        default_function: PackageRef::parse(agl_repo::DEFAULT_FUNCTION)?,
        sources: Vec::new(),
        policy: WorkspacePolicy::default(),
        config: WorkspaceConfigReferences::default(),
    };
    if !options.dry_run {
        agl_repo::write_workspace_manifest(&path, &manifest)?;
    }
    println!(
        "workspace={} manifest={} dry_run={}",
        root.display(),
        path.display(),
        options.dry_run
    );
    Ok(())
}

fn verify_tasks(options: TaskSpecVerifyOptions) -> Result<()> {
    let root = agl_repo::resolve_repo_root(std::env::current_dir()?)?;
    let descriptor = agl_core_tools::repo::declaration();
    let id = ArtifactId::new("core.repo:tasks").expect("fixed Artifact ID is valid");
    let declaration = descriptor
        .artifact(&id)
        .context("core.repo:tasks declaration is missing")?;
    let binding = agl_repo::ArtifactGitRepository::open(&root)?.verify_binding(declaration)?;
    let handle = ArtifactHandle::bind(declaration.clone(), binding)?;
    let report = agl_repo::verify_task_specs(
        &handle,
        &agl_repo::TaskSpecVerifyOptions {
            strict: options.strict,
        },
    )?;
    if options.json {
        crate::print_json(&serde_json::json!({
            "artifact_id": id,
            "files": report.files,
            "errors": report.errors,
        }))?;
    } else {
        println!("artifact_id={id}");
        for file in &report.files {
            println!("file={file}");
        }
        for error in &report.errors {
            println!("error={error}");
        }
    }
    if report.should_fail(options.strict) {
        bail!("task specification verification failed");
    }
    Ok(())
}

fn install_hooks(options: RepoHooksOptions) -> Result<()> {
    let root = agl_repo::resolve_repo_root(std::env::current_dir()?)?;
    let hooks = root.join(".git/hooks");
    for name in ["pre-commit", "pre-push"] {
        let path = hooks.join(name);
        if path.exists() {
            let managed = fs::read_to_string(&path)
                .unwrap_or_default()
                .contains("agentLIBRE managed hook");
            ensure!(
                managed || options.force,
                "unmanaged hook exists: {}",
                path.display()
            );
        }
        if !options.dry_run {
            write_hook(&path, name)?;
        }
        println!("hook={} dry_run={}", path.display(), options.dry_run);
    }
    Ok(())
}

fn write_hook(path: &Path, name: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        format!("#!/bin/sh\n# agentLIBRE managed hook: {name}\nset -eu\nagl package lock\n"),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}
