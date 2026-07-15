use std::path::{Path, PathBuf};

use agl_capabilities::{
    ActionDeclaration, ActionHandler, ActionHandlerError, ActionInvocation, ActionResult,
    CapabilityId, OperationKind, ProviderDeclaration, ProviderId, StateEffect,
};
use agl_repo::{
    RepoHooksOptions, RepoInitOptions, RepoStatusOptions, init_repo_workspace, install_repo_hooks,
    render_repo_profile_toml, status_repo_workspace,
};
use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ToolCatalog, ToolCatalogError, parse_action_args as parse_args};

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

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<Value> {
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

    fn status(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<StatusArgs>(REPO_STATUS_TOOL_ID, arguments)?;
        let report = status_repo_workspace(
            &self.workspace_root,
            &RepoStatusOptions {
                component: args.component,
                strict: args.strict.unwrap_or(false),
            },
        )?;
        let state = serde_json::to_value(report.state)?;
        Ok(json!({
            "tool": REPO_STATUS_TOOL_ID,
            "status": state,
            "workspace_root": report.workspace_root,
            "manifest_path": report.manifest_path,
            "component_count": report.components.len(),
            "warning_count": report.warnings.len(),
            "error_count": report.errors.len(),
            "components": report.components,
            "warnings": report.warnings,
            "errors": report.errors,
            "next_steps": report.next_steps,
        }))
    }

    fn export_profile(&self, arguments: Value) -> Result<Value> {
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
        Ok(json!({
            "tool": REPO_EXPORT_PROFILE_TOOL_ID,
            "status": "ok",
            "bytes": profile.len(),
            "truncated": truncated,
            "profile": profile,
        }))
    }

    fn hooks_status(&self, arguments: Value) -> Result<Value> {
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

    fn init(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<InitArgs>(REPO_INIT_TOOL_ID, arguments)?;
        let report = init_repo_workspace(
            &self.workspace_root,
            &RepoInitOptions {
                profile: args
                    .profile
                    .unwrap_or_else(|| agl_repo::DEFAULT_PROFILE.to_string()),
                profile_file: args.profile_file.map(PathBuf::from),
                artifacts: Vec::new(),
                skills_url: None,
                skills_rev: None,
                tasks_url: None,
                tasks_rev: None,
                dry_run: args.dry_run.unwrap_or(false),
                force: args.force.unwrap_or(false),
            },
        )?;
        Ok(render_init_report(REPO_INIT_TOOL_ID, report))
    }

    fn import_profile(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<ImportProfileArgs>(REPO_IMPORT_PROFILE_TOOL_ID, arguments)?;
        let report = init_repo_workspace(
            &self.workspace_root,
            &RepoInitOptions {
                profile: agl_repo::DEFAULT_PROFILE.to_string(),
                profile_file: Some(PathBuf::from(args.profile_file)),
                artifacts: Vec::new(),
                skills_url: None,
                skills_rev: None,
                tasks_url: None,
                tasks_rev: None,
                dry_run: args.dry_run.unwrap_or(false),
                force: args.force.unwrap_or(false),
            },
        )?;
        Ok(render_init_report(REPO_IMPORT_PROFILE_TOOL_ID, report))
    }

    fn install_hooks(&self, arguments: Value) -> Result<Value> {
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

impl ActionHandler for RepoTools {
    fn dispatch(&self, invocation: ActionInvocation) -> Result<ActionResult, ActionHandlerError> {
        self.dispatch(invocation.capability_id.as_str(), invocation.arguments)
            .map(ActionResult::new)
            .map_err(Into::into)
    }
}

pub fn declaration() -> ProviderDeclaration {
    ProviderDeclaration::builtin(
        ProviderId::new(PROVIDER_ID).expect("builtin repo provider id is valid"),
        "Repo Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin repo provider declaration is valid")
    .with_action(action::<StatusArgs>(
        REPO_STATUS_TOOL_ID,
        "Inspect agentLIBRE workspace manifest and component health.",
        OperationKind::Read,
    ))
    .with_action(action::<ExportProfileArgs>(
        REPO_EXPORT_PROFILE_TOOL_ID,
        "Render the current agentLIBRE workspace profile without writing a file.",
        OperationKind::Read,
    ))
    .with_action(action::<HooksStatusArgs>(
        REPO_HOOKS_STATUS_TOOL_ID,
        "Dry-run repository hook installation status.",
        OperationKind::Read,
    ))
    .with_action(
        action::<InitArgs>(
            REPO_INIT_TOOL_ID,
            "Initialize agentLIBRE workspace files.",
            OperationKind::Admin,
        )
        .with_state_effects([StateEffect::RepoWorkspace]),
    )
    .with_action(
        action::<ImportProfileArgs>(
            REPO_IMPORT_PROFILE_TOOL_ID,
            "Apply an explicit agentLIBRE workspace profile file.",
            OperationKind::Admin,
        )
        .with_state_effects([StateEffect::RepoWorkspace]),
    )
    .with_action(
        action::<InstallHooksArgs>(
            REPO_INSTALL_HOOKS_TOOL_ID,
            "Install agentLIBRE managed git hooks.",
            OperationKind::Admin,
        )
        .with_state_effects([StateEffect::RepoHooks]),
    )
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn action<T: JsonSchema>(
    id: &str,
    description: &str,
    operation_kind: OperationKind,
) -> ActionDeclaration {
    ActionDeclaration::from_schema::<T>(
        CapabilityId::new(id).expect("builtin repo action id is valid"),
        description,
        operation_kind,
    )
    .expect("builtin repo action schema is valid")
}

fn render_hooks_report(tool_id: &str, report: &agl_repo::HookInstallReport) -> Value {
    json!({
        "tool": tool_id,
        "status": if report.errors.is_empty() { "ok" } else { "failed" },
        "workspace_root": report.workspace_root,
        "dry_run": report.dry_run,
        "hook_count": report.hooks.len(),
        "error_count": report.errors.len(),
        "hooks": report.hooks,
        "errors": report.errors,
    })
}

fn render_init_report(tool_id: &str, report: agl_repo::RepoInitReport) -> Value {
    json!({
        "tool": tool_id,
        "status": "ok",
        "workspace_root": report.workspace_root,
        "manifest_path": report.manifest_path,
        "dry_run": report.dry_run,
        "change_count": report.changes.len(),
        "changes": report.changes,
        "next_steps": report.next_steps,
    })
}

fn previous_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    component: Option<String>,
    strict: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExportProfileArgs {
    max_bytes: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct HooksStatusArgs {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct InitArgs {
    profile: Option<String>,
    profile_file: Option<String>,
    dry_run: Option<bool>,
    force: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ImportProfileArgs {
    profile_file: String,
    dry_run: Option<bool>,
    force: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct InstallHooksArgs {
    dry_run: Option<bool>,
    force: Option<bool>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::test_support::temp_root;

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

        assert_eq!(init["status"], "ok");
        assert!(root.join(".agl/workspace.toml").is_file());
        assert_eq!(status["tool"], REPO_STATUS_TOOL_ID);
        assert!(status["components"].as_array().unwrap().is_empty());
        assert_eq!(profile["tool"], REPO_EXPORT_PROFILE_TOOL_ID);
        assert!(
            profile["profile"]
                .as_str()
                .unwrap()
                .contains("name = \"repo-workflow\"")
        );
        assert_eq!(hooks["tool"], REPO_HOOKS_STATUS_TOOL_ID);
        assert_eq!(hooks["dry_run"], true);
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

[artifacts.skills]
kind = "local"
path = ".agl/skills"
required = true
access = "read"
create = ["."]

[artifacts.tasks]
kind = "local"
path = ".agl/tasks"
required = true
access = "read_write"
validation = "agl.task_spec.v1"
create = ["."]
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

        assert_eq!(imported["tool"], REPO_IMPORT_PROFILE_TOOL_ID);
        assert_eq!(imported["status"], "ok");
        assert!(root.join(".agl/workspace.toml").is_file());
    }

    #[test]
    fn repo_declarations_expose_closed_schemas() {
        let declaration = declaration();
        for action in &declaration.actions {
            assert_eq!(action.input_schema["additionalProperties"], false);
        }
        let import = declaration
            .actions
            .iter()
            .find(|action| action.id.as_str() == REPO_IMPORT_PROFILE_TOOL_ID)
            .unwrap();
        assert_eq!(import.input_schema["required"], json!(["profile_file"]));
        assert!(
            import
                .compile_schema()
                .unwrap()
                .validate(&json!({"profile_file": "profile.toml", "extra": true}))
                .is_err()
        );
    }
}
