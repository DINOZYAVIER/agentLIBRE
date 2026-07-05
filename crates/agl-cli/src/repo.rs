use agl_repo::{
    HookInstallReport, RepoComponentInitAction,
    RepoComponentInitOptions as AglRepoComponentInitOptions, RepoComponentInitReport,
    RepoExportProfileOptions as AglRepoExportProfileOptions, RepoExportProfileReport,
    RepoHooksOptions as AglRepoHooksOptions, RepoInitAction, RepoInitOptions as AglRepoInitOptions,
    RepoInitReport, RepoStatusOptions as AglRepoStatusOptions, RepoStatusReport,
    TaskSpecVerifyOptions as AglTaskSpecVerifyOptions, TaskSpecVerifyReport, TaskSpecVerifyState,
    export_repo_profile, init_repo_component, init_repo_workspace, install_repo_hooks,
    status_repo_workspace, verify_task_specs,
};
use anyhow::{Context, Result, bail};

use crate::args::{
    RepoCommand, RepoComponentInitOptions, RepoExportProfileOptions, RepoHooksOptions,
    RepoImportProfileOptions, RepoInitOptions, RepoStatusOptions, TaskSpecVerifyOptions,
};

pub(crate) fn run_repo(command: RepoCommand) -> Result<()> {
    match command {
        RepoCommand::Init(options) => run_repo_init(options),
        RepoCommand::InitComponent(options) => run_repo_init_component(options),
        RepoCommand::ImportProfile(options) => run_repo_import_profile(options),
        RepoCommand::Status(options) => run_repo_status(options),
        RepoCommand::VerifyTasks(options) => run_repo_verify_tasks(options),
        RepoCommand::InstallHooks(options) => run_install_hooks(options),
        RepoCommand::ExportProfile(options) => run_repo_export_profile(options),
    }
}

fn run_repo_init(options: RepoInitOptions) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "init", "starting command");
    let report = init_repo_workspace(
        std::env::current_dir().context("failed to resolve current directory")?,
        &AglRepoInitOptions {
            profile: options.profile,
            profile_file: options.profile_file,
            skills_url: options.skills_url,
            skills_rev: options.skills_rev,
            tasks_url: options.tasks_url,
            tasks_rev: options.tasks_rev,
            dry_run: options.dry_run,
            force: options.force,
        },
    )?;
    print_repo_init_report(&report);
    Ok(())
}

fn run_repo_init_component(options: RepoComponentInitOptions) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "repo init-component", "starting command");
    let report = init_repo_component(
        std::env::current_dir().context("failed to resolve current directory")?,
        &AglRepoComponentInitOptions {
            component: options.component,
            dry_run: options.dry_run,
        },
    )?;
    crate::print_json_or(options.json, &report, || {
        print_repo_component_init_report(&report)
    })?;
    if report.has_errors() {
        bail!("repo component initialization failed");
    }
    Ok(())
}

fn run_repo_import_profile(options: RepoImportProfileOptions) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "repo import-profile", "starting command");
    let report = init_repo_workspace(
        std::env::current_dir().context("failed to resolve current directory")?,
        &AglRepoInitOptions {
            profile: agl_repo::DEFAULT_PROFILE.to_string(),
            profile_file: Some(options.profile_file),
            skills_url: None,
            skills_rev: None,
            tasks_url: None,
            tasks_rev: None,
            dry_run: options.dry_run,
            force: options.force,
        },
    )?;
    print_repo_init_report(&report);
    Ok(())
}

fn run_repo_status(options: RepoStatusOptions) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "status", "starting command");
    let report = status_repo_workspace(
        std::env::current_dir().context("failed to resolve current directory")?,
        &AglRepoStatusOptions {
            component: options.component,
            strict: options.strict,
        },
    )?;

    crate::print_json_or(options.json, &report, || print_repo_status_report(&report))?;

    if report.should_fail(options.strict) {
        bail!("repo workspace status is not healthy");
    }
    Ok(())
}

fn run_repo_verify_tasks(options: TaskSpecVerifyOptions) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "repo verify-tasks", "starting command");
    let report = verify_task_specs(
        std::env::current_dir().context("failed to resolve current directory")?,
        &AglTaskSpecVerifyOptions {
            strict: options.strict,
        },
    )?;
    crate::print_json_or(options.json, &report, || {
        print_task_spec_verify_report(&report)
    })?;
    if report.should_fail(options.strict) {
        bail!("task spec verification failed");
    }
    Ok(())
}

fn run_install_hooks(options: RepoHooksOptions) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "install-hooks", "starting command");
    let report = install_repo_hooks(
        std::env::current_dir().context("failed to resolve current directory")?,
        &AglRepoHooksOptions {
            dry_run: options.dry_run,
            force: options.force,
        },
    )?;
    print_hook_install_report(&report);
    if report.has_errors() {
        bail!("git hook installation has conflicts");
    }
    Ok(())
}

fn run_repo_export_profile(options: RepoExportProfileOptions) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "repo export-profile", "starting command");
    let report = export_repo_profile(
        std::env::current_dir().context("failed to resolve current directory")?,
        &AglRepoExportProfileOptions {
            out: options.out,
            force: options.force,
        },
    )?;
    crate::print_json_or(options.json, &report, || {
        print_repo_export_profile_report(&report)
    })
}

fn print_repo_init_report(report: &RepoInitReport) {
    println!("state=initialized");
    println!("workspace_root={}", report.workspace_root.display());
    println!("manifest_path={}", report.manifest_path.display());
    println!("dry_run={}", report.dry_run);
    for change in &report.changes {
        println!(
            "change path={} action={}",
            change.path.display(),
            repo_init_action(change.action)
        );
    }
    for next_step in &report.next_steps {
        println!("next_step={next_step}");
    }
}

fn print_repo_status_report(report: &RepoStatusReport) {
    println!("state={}", repo_status_state(report.state));
    println!("workspace_root={}", report.workspace_root.display());
    println!("manifest_path={}", report.manifest_path.display());
    for component in &report.components {
        crate::print_component_status(component);
    }
    for warning in &report.warnings {
        println!("warning={warning}");
    }
    for error in &report.errors {
        println!("error={error}");
    }
    for next_step in &report.next_steps {
        println!("next_step={next_step}");
    }
}

pub(crate) fn print_repo_component_init_report(report: &RepoComponentInitReport) {
    println!("state={}", if report.has_errors() { "error" } else { "ok" });
    println!("workspace_root={}", report.workspace_root.display());
    println!("manifest_path={}", report.manifest_path.display());
    println!("component={}", report.component);
    println!("path={}", report.path.display());
    println!("dry_run={}", report.dry_run);
    for action in &report.actions {
        println!("action={}", repo_component_init_action(*action));
    }
    for error in &report.errors {
        println!("error={error}");
    }
}

fn print_task_spec_verify_report(report: &TaskSpecVerifyReport) {
    println!("state={}", task_spec_verify_state(report.state));
    println!("workspace_root={}", report.workspace_root.display());
    println!("root={}", report.root.display());
    if let Some(component) = &report.component {
        crate::print_component_status(component);
    }
    for file in &report.files {
        println!(
            "task_spec path={} valid={}",
            file.path.display(),
            file.valid
        );
        for section in &file.missing_sections {
            println!(
                "task_spec.missing_section path={} section={}",
                file.path.display(),
                section
            );
        }
        for warning in &file.warnings {
            println!("task_spec.warning path={} {warning}", file.path.display());
        }
        for error in &file.errors {
            println!("task_spec.error path={} {error}", file.path.display());
        }
    }
    for warning in &report.warnings {
        println!("warning={warning}");
    }
    for error in &report.errors {
        println!("error={error}");
    }
}

fn task_spec_verify_state(state: TaskSpecVerifyState) -> &'static str {
    match state {
        TaskSpecVerifyState::Ok => "ok",
        TaskSpecVerifyState::Warning => "warning",
        TaskSpecVerifyState::Invalid => "invalid",
    }
}

fn print_repo_export_profile_report(report: &RepoExportProfileReport) {
    println!("profile.exported={}", report.wrote);
    println!("profile.path={}", report.profile_path.display());
    println!("profile.name={}", report.profile.name);
    println!("profile.version={}", report.profile.version);
    println!(
        "profile.policy.hooks.managed={}",
        report.profile.policy.hooks.managed
    );
    println!(
        "profile.policy.trust.import_local_trust={}",
        report.profile.policy.trust.import_local_trust
    );
    if let Some(skill_pack) = &report.profile.skill_pack {
        println!("profile.skill_pack.component={}", skill_pack.component);
        println!("profile.skill_pack.path={}", skill_pack.path.display());
        if let Some(url) = &skill_pack.url {
            println!("profile.skill_pack.url={url}");
        }
        if let Some(rev) = &skill_pack.rev {
            println!("profile.skill_pack.rev={rev}");
        }
        if let Some(commit) = &skill_pack.commit {
            println!("profile.skill_pack.commit={commit}");
        }
        if let Some(tree) = &skill_pack.tree {
            println!("profile.skill_pack.tree={tree}");
        }
        println!(
            "profile.skill_pack.same_ids_when_pinned={}",
            skill_pack.same_ids_when_pinned
        );
    }
}

fn print_hook_install_report(report: &HookInstallReport) {
    println!(
        "state={}",
        if report.has_errors() {
            "conflict"
        } else {
            "ok"
        }
    );
    println!("workspace_root={}", report.workspace_root.display());
    println!("dry_run={}", report.dry_run);
    for hook in &report.hooks {
        println!(
            "hook name={} path={} action={:?}",
            hook.hook,
            hook.path.display(),
            hook.action
        );
    }
    for error in &report.errors {
        println!("error={error}");
    }
}

fn repo_component_init_action(action: RepoComponentInitAction) -> &'static str {
    match action {
        RepoComponentInitAction::WouldAddSubmodule => "would_add_submodule",
        RepoComponentInitAction::AddedSubmodule => "added_submodule",
        RepoComponentInitAction::WouldUpdateSubmodule => "would_update_submodule",
        RepoComponentInitAction::UpdatedSubmodule => "updated_submodule",
        RepoComponentInitAction::WouldCheckoutRev => "would_checkout_rev",
        RepoComponentInitAction::CheckedOutRev => "checked_out_rev",
        RepoComponentInitAction::AlreadyInitialized => "already_initialized",
    }
}

fn repo_init_action(action: RepoInitAction) -> &'static str {
    match action {
        RepoInitAction::WouldCreateDir => "would_create_dir",
        RepoInitAction::CreatedDir => "created_dir",
        RepoInitAction::Exists => "exists",
        RepoInitAction::WouldWriteFile => "would_write_file",
        RepoInitAction::WroteFile => "wrote_file",
        RepoInitAction::WouldOverwriteFile => "would_overwrite_file",
        RepoInitAction::OverwroteFile => "overwrote_file",
        RepoInitAction::DeclaredSubmodule => "declared_submodule",
        RepoInitAction::DeclaredGitComponent => "declared_git_component",
    }
}

fn repo_status_state(state: agl_repo::RepoStatusState) -> &'static str {
    match state {
        agl_repo::RepoStatusState::Ok => "ok",
        agl_repo::RepoStatusState::Warning => "warning",
        agl_repo::RepoStatusState::Invalid => "invalid",
    }
}
