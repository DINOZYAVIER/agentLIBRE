use std::path::{Path, PathBuf};

use agl_repo::{
    RepoHooksOptions, RepoInitOptions, RepoStatusOptions, init_repo_workspace, install_repo_hooks,
    render_repo_profile_toml, status_repo_workspace,
};
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::{
    ToolCapability, ToolCatalog, ToolCatalogError, ToolDeclaration, ToolHandler, ToolId, ToolInput,
    ToolOperationKind, ToolOutput, ToolProviderDeclaration, ToolProviderId, ToolStateEffect,
    parse_tool_args as parse_args,
};

pub const PROVIDER_ID: &str = "repo-tools";
pub const REPO_STATUS_TOOL_ID: &str = "repo.status";
pub const REPO_EXPORT_PROFILE_TOOL_ID: &str = "repo.export_profile";
pub const REPO_HOOKS_STATUS_TOOL_ID: &str = "repo.hooks.status";
pub const REPO_INIT_TOOL_ID: &str = "repo.init";
pub const REPO_IMPORT_PROFILE_TOOL_ID: &str = "repo.import_profile";
pub const REPO_INSTALL_HOOKS_TOOL_ID: &str = "repo.install_hooks";

const DEFAULT_PROFILE_MAX_BYTES: usize = 16 * 1024;
const MAX_PROFILE_BYTES: usize = 128 * 1024;

#[derive(Clone, Debug)]
pub struct RepoTools {
    workspace_root: PathBuf,
}

impl RepoTools {
    pub fn new(workspace_root: impl AsRef<Path>) -> Self {
        Self {
            workspace_root: workspace_root.as_ref().to_path_buf(),
        }
    }

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<String> {
        match name {
            REPO_STATUS_TOOL_ID => self.status(arguments),
            REPO_EXPORT_PROFILE_TOOL_ID => self.export_profile(arguments),
            REPO_HOOKS_STATUS_TOOL_ID => self.hooks_status(arguments),
            REPO_INIT_TOOL_ID => self.init(arguments),
            REPO_IMPORT_PROFILE_TOOL_ID => self.import_profile(arguments),
            REPO_INSTALL_HOOKS_TOOL_ID => self.install_hooks(arguments),
            _ => anyhow::bail!("unknown repo tool `{name}`"),
        }
    }

    fn status(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<StatusArgs>(REPO_STATUS_TOOL_ID, arguments)?;
        let report = status_repo_workspace(
            &self.workspace_root,
            &RepoStatusOptions {
                component: args.component,
                strict: args.strict.unwrap_or(false),
            },
        )?;
        let mut output = format!(
            "tool=repo.status\nstate={:?}\nworkspace_root={}\ncomponents={}\nwarnings={}\nerrors={}\n---",
            report.state,
            report.workspace_root.display(),
            report.components.len(),
            report.warnings.len(),
            report.errors.len()
        );
        for component in report.components {
            output.push('\n');
            output.push_str(&format!(
                "component name={} kind={:?} state={:?} exists={} path={}",
                component.name,
                component.kind,
                component.state,
                component.exists,
                component.path.display()
            ));
        }
        Ok(output)
    }

    fn export_profile(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<ExportProfileArgs>(REPO_EXPORT_PROFILE_TOOL_ID, arguments)?;
        let max_bytes = args
            .max_bytes
            .unwrap_or(DEFAULT_PROFILE_MAX_BYTES)
            .min(MAX_PROFILE_BYTES);
        let mut profile = render_repo_profile_toml(&self.workspace_root)?;
        let truncated = profile.len() > max_bytes;
        if truncated {
            profile.truncate(previous_char_boundary(&profile, max_bytes));
        }
        Ok(format!(
            "tool=repo.export_profile\nbytes={}\ntruncated={truncated}\n---\n{}",
            profile.len(),
            profile
        ))
    }

    fn hooks_status(&self, arguments: Value) -> Result<String> {
        parse_args::<HooksStatusArgs>(REPO_HOOKS_STATUS_TOOL_ID, arguments)?;
        let report = install_repo_hooks(
            &self.workspace_root,
            &RepoHooksOptions {
                dry_run: true,
                force: false,
            },
        )?;
        Ok(render_hooks_report(REPO_HOOKS_STATUS_TOOL_ID, &report))
    }

    fn init(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<InitArgs>(REPO_INIT_TOOL_ID, arguments)?;
        let report = init_repo_workspace(
            &self.workspace_root,
            &RepoInitOptions {
                profile: args
                    .profile
                    .unwrap_or_else(|| agl_repo::DEFAULT_PROFILE.to_string()),
                profile_file: args.profile_file.map(PathBuf::from),
                dry_run: args.dry_run.unwrap_or(false),
                force: args.force.unwrap_or(false),
            },
        )?;
        Ok(format!(
            "tool=repo.init\nworkspace_root={}\ndry_run={}\nchanges={}\nstatus=ok",
            report.workspace_root.display(),
            report.dry_run,
            report.changes.len()
        ))
    }

    fn import_profile(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<ImportProfileArgs>(REPO_IMPORT_PROFILE_TOOL_ID, arguments)?;
        let report = init_repo_workspace(
            &self.workspace_root,
            &RepoInitOptions {
                profile: agl_repo::DEFAULT_PROFILE.to_string(),
                profile_file: Some(PathBuf::from(args.profile_file)),
                dry_run: args.dry_run.unwrap_or(false),
                force: args.force.unwrap_or(false),
            },
        )?;
        Ok(format!(
            "tool=repo.import_profile\nworkspace_root={}\nmanifest_path={}\ndry_run={}\nchanges={}\nstatus=ok",
            report.workspace_root.display(),
            report.manifest_path.display(),
            report.dry_run,
            report.changes.len()
        ))
    }

    fn install_hooks(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<InstallHooksArgs>(REPO_INSTALL_HOOKS_TOOL_ID, arguments)?;
        let report = install_repo_hooks(
            &self.workspace_root,
            &RepoHooksOptions {
                dry_run: args.dry_run.unwrap_or(false),
                force: args.force.unwrap_or(false),
            },
        )?;
        Ok(render_hooks_report(REPO_INSTALL_HOOKS_TOOL_ID, &report))
    }
}

impl ToolHandler for RepoTools {
    fn dispatch(&self, input: ToolInput) -> Result<ToolOutput> {
        let observation = self.dispatch(input.id.as_str(), input.arguments)?;
        Ok(ToolOutput { observation })
    }
}

pub fn declaration() -> ToolProviderDeclaration {
    ToolProviderDeclaration::new(
        ToolProviderId::new(PROVIDER_ID).expect("builtin repo provider id is valid"),
        "Repo Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin repo provider declaration is valid")
    .with_tool(ToolDeclaration::new(
        ToolId::new(REPO_STATUS_TOOL_ID).expect("builtin repo tool id is valid"),
        "Inspect AgentLIBRE workspace manifest and component health.",
        ToolCapability::Read,
        std::iter::empty::<&str>(),
    ))
    .with_tool(ToolDeclaration::new(
        ToolId::new(REPO_EXPORT_PROFILE_TOOL_ID).expect("builtin repo tool id is valid"),
        "Render the current AgentLIBRE workspace profile without writing a file.",
        ToolCapability::Read,
        std::iter::empty::<&str>(),
    ))
    .with_tool(ToolDeclaration::new(
        ToolId::new(REPO_HOOKS_STATUS_TOOL_ID).expect("builtin repo tool id is valid"),
        "Dry-run repository hook installation status.",
        ToolCapability::Read,
        std::iter::empty::<&str>(),
    ))
    .with_tool(
        ToolDeclaration::new(
            ToolId::new(REPO_INIT_TOOL_ID).expect("builtin repo tool id is valid"),
            "Initialize AgentLIBRE workspace files.",
            ToolCapability::Write,
            std::iter::empty::<&str>(),
        )
        .with_operation_kind(ToolOperationKind::Admin)
        .with_state_effects([ToolStateEffect::RepoWorkspace]),
    )
    .with_tool(
        ToolDeclaration::new(
            ToolId::new(REPO_IMPORT_PROFILE_TOOL_ID).expect("builtin repo tool id is valid"),
            "Apply an explicit AgentLIBRE workspace profile file.",
            ToolCapability::Write,
            ["profile_file"],
        )
        .with_operation_kind(ToolOperationKind::Admin)
        .with_state_effects([ToolStateEffect::RepoWorkspace]),
    )
    .with_tool(
        ToolDeclaration::new(
            ToolId::new(REPO_INSTALL_HOOKS_TOOL_ID).expect("builtin repo tool id is valid"),
            "Install AgentLIBRE managed git hooks.",
            ToolCapability::Write,
            std::iter::empty::<&str>(),
        )
        .with_operation_kind(ToolOperationKind::Admin)
        .with_state_effects([ToolStateEffect::RepoHooks]),
    )
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn render_hooks_report(tool_id: &str, report: &agl_repo::HookInstallReport) -> String {
    let mut output = format!(
        "tool={tool_id}\nworkspace_root={}\ndry_run={}\nhooks={}\nerrors={}\n---",
        report.workspace_root.display(),
        report.dry_run,
        report.hooks.len(),
        report.errors.len()
    );
    for hook in &report.hooks {
        output.push('\n');
        output.push_str(&format!(
            "hook name={} action={:?} path={}",
            hook.hook,
            hook.action,
            hook.path.display()
        ));
    }
    output
}

fn previous_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    component: Option<String>,
    strict: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportProfileArgs {
    max_bytes: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HooksStatusArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InitArgs {
    profile: Option<String>,
    profile_file: Option<String>,
    dry_run: Option<bool>,
    force: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportProfileArgs {
    profile_file: String,
    dry_run: Option<bool>,
    force: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallHooksArgs {
    dry_run: Option<bool>,
    force: Option<bool>,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::*;

    #[test]
    fn repo_tools_initialize_status_export_and_check_hooks() {
        let root = temp_root("workspace");
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        let tools = RepoTools::new(&root);

        let init = tools.dispatch(REPO_INIT_TOOL_ID, json!({})).unwrap();
        let status = tools.dispatch(REPO_STATUS_TOOL_ID, json!({})).unwrap();
        let profile = tools
            .dispatch(REPO_EXPORT_PROFILE_TOOL_ID, json!({"max_bytes": 4096}))
            .unwrap();
        let hooks = tools
            .dispatch(REPO_HOOKS_STATUS_TOOL_ID, json!({}))
            .unwrap();

        assert!(init.contains("status=ok"));
        assert!(root.join(".agl/workspace.toml").is_file());
        assert!(status.contains("tool=repo.status"));
        assert!(status.contains("component name=skills"));
        assert!(profile.contains("tool=repo.export_profile"));
        assert!(profile.contains("name = \"repo-workflow\""));
        assert!(hooks.contains("tool=repo.hooks.status"));
        assert!(hooks.contains("dry_run=true"));

        cleanup(root);
    }

    #[test]
    fn repo_tools_import_profile_requires_explicit_profile_file() {
        let root = temp_root("import-profile");
        std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
        let profile_path = root.join("profile.toml");
        std::fs::write(
            &profile_path,
            r#"
version = 1
name = "team-profile"

[components.skills]
path = ".agl/skills"
kind = "local"

[components.tasks]
path = ".agl/tasks"
kind = "local"
"#,
        )
        .unwrap();
        let tools = RepoTools::new(&root);

        let imported = tools
            .dispatch(
                REPO_IMPORT_PROFILE_TOOL_ID,
                json!({"profile_file": profile_path, "dry_run": false}),
            )
            .unwrap();

        assert!(imported.contains("tool=repo.import_profile"));
        assert!(imported.contains("status=ok"));
        assert!(root.join(".agl/workspace.toml").is_file());

        cleanup(root);
    }

    fn temp_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "agl-repo-tools-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn cleanup(root: PathBuf) {
        let _ = std::fs::remove_dir_all(root);
    }
}
