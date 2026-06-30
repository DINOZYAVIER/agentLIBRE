use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::{
    HookInstallAction, HookInstallReport, HookInstallStatus, RepoHooksOptions, resolve_repo_root,
};

const MANAGED_HOOK_MARKER: &str = "agentLIBRE managed hook";

pub fn install_repo_hooks(
    start: impl AsRef<Path>,
    options: &RepoHooksOptions,
) -> Result<HookInstallReport> {
    let workspace_root = resolve_repo_root(start)?;
    let git_dir = workspace_root.join(".git");
    if !git_dir.is_dir() {
        bail!(
            "repo hooks require a git directory at {}",
            git_dir.display()
        );
    }

    let hooks_dir = git_dir.join("hooks");
    let mut hooks = Vec::new();
    let mut errors = Vec::new();
    for hook in ["pre-commit", "pre-push"] {
        let status = plan_hook_install(&hooks_dir, hook, options)?;
        if status.action == HookInstallAction::Conflict {
            errors.push(format!("hook_conflict: {}", status.path.display()));
        }
        hooks.push(status);
    }

    if !errors.is_empty() {
        for status in &mut hooks {
            status.action = dry_run_hook_action(status.action);
        }
    } else if !options.dry_run {
        for status in &mut hooks {
            apply_hook_install(&hooks_dir, status)?;
        }
    }

    Ok(HookInstallReport {
        workspace_root,
        dry_run: options.dry_run,
        hooks,
        errors,
    })
}

fn plan_hook_install(
    hooks_dir: &Path,
    hook: &str,
    options: &RepoHooksOptions,
) -> Result<HookInstallStatus> {
    let path = hooks_dir.join(hook);

    if path.exists() {
        let existing = fs::read_to_string(&path).unwrap_or_default();
        let managed = existing.contains(MANAGED_HOOK_MARKER);
        if !managed && !options.force {
            return Ok(HookInstallStatus {
                hook: hook.to_string(),
                path,
                action: HookInstallAction::Conflict,
            });
        }
        if managed && !options.force {
            return Ok(HookInstallStatus {
                hook: hook.to_string(),
                path,
                action: HookInstallAction::AlreadyManaged,
            });
        }
        if options.dry_run {
            return Ok(HookInstallStatus {
                hook: hook.to_string(),
                path,
                action: if managed {
                    HookInstallAction::WouldReplaceManaged
                } else {
                    HookInstallAction::WouldReplaceUnmanaged
                },
            });
        }
        return Ok(HookInstallStatus {
            hook: hook.to_string(),
            path,
            action: if managed {
                HookInstallAction::ReplacedManaged
            } else {
                HookInstallAction::ReplacedUnmanaged
            },
        });
    }

    if options.dry_run {
        return Ok(HookInstallStatus {
            hook: hook.to_string(),
            path,
            action: HookInstallAction::WouldInstall,
        });
    }
    Ok(HookInstallStatus {
        hook: hook.to_string(),
        path,
        action: HookInstallAction::Installed,
    })
}

fn apply_hook_install(hooks_dir: &Path, status: &mut HookInstallStatus) -> Result<()> {
    if matches!(
        status.action,
        HookInstallAction::AlreadyManaged | HookInstallAction::Conflict
    ) {
        return Ok(());
    }
    let content = hook_content(&status.hook);
    fs::create_dir_all(hooks_dir)
        .with_context(|| format!("failed to create hooks directory {}", hooks_dir.display()))?;
    fs::write(&status.path, content)
        .with_context(|| format!("failed to write hook {}", status.path.display()))?;
    make_executable(&status.path)
}

fn dry_run_hook_action(action: HookInstallAction) -> HookInstallAction {
    match action {
        HookInstallAction::Installed => HookInstallAction::WouldInstall,
        HookInstallAction::ReplacedManaged => HookInstallAction::WouldReplaceManaged,
        HookInstallAction::ReplacedUnmanaged => HookInstallAction::WouldReplaceUnmanaged,
        other => other,
    }
}

pub(crate) fn hook_content(hook: &str) -> String {
    format!(
        r#"#!/bin/sh
# {MANAGED_HOOK_MARKER}: {hook}
set -eu
AGL_BIN="${{AGL_BIN:-agl}}"
if ! command -v "$AGL_BIN" >/dev/null 2>&1; then
  echo "agentLIBRE hook error: $AGL_BIN not found on PATH; install agl or set AGL_BIN." >&2
  exit 127
fi
"$AGL_BIN" status --strict
"$AGL_BIN" skill verify
"#
    )
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("failed to make hook executable {}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}
