use std::path::{Path, PathBuf};

use agl_skills::{
    SkillLockOptions, SkillTrustOptions, WorkspaceSkillStatus, builtin_registry,
    lock_workspace_skills, revoke_workspace_skill, trust_workspace_skill, workspace_skill_report,
    workspace_skill_report_with_trust,
};
use agl_tools::{ToolHandler, ToolInput, ToolOutput};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_LIMIT: usize = 100;
const MAX_BODY_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug)]
pub struct SkillTools {
    workspace_root: PathBuf,
    trust_store_path: PathBuf,
    agentlibre_version: String,
}

impl SkillTools {
    pub fn new(
        workspace_root: impl AsRef<Path>,
        trust_store_path: impl AsRef<Path>,
        agentlibre_version: impl Into<String>,
    ) -> Self {
        Self {
            workspace_root: workspace_root.as_ref().to_path_buf(),
            trust_store_path: trust_store_path.as_ref().to_path_buf(),
            agentlibre_version: agentlibre_version.into(),
        }
    }

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<String> {
        match name {
            agl_tools::SKILL_LIST_TOOL_ID => self.list(arguments),
            agl_tools::SKILL_INSPECT_TOOL_ID => self.inspect(arguments),
            agl_tools::SKILL_STATUS_TOOL_ID => self.status(arguments),
            agl_tools::SKILL_VERIFY_TOOL_ID => self.verify(arguments),
            agl_tools::SKILL_LOCK_TOOL_ID => self.lock(arguments),
            agl_tools::SKILL_TRUST_TOOL_ID => self.trust(arguments),
            agl_tools::SKILL_REVOKE_TOOL_ID => self.revoke(arguments),
            _ => bail!("unknown skill tool `{name}`"),
        }
    }

    fn list(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<ListArgs>(agl_tools::SKILL_LIST_TOOL_ID, arguments)?;
        let source = args.source.as_deref().unwrap_or("all");
        ensure!(
            matches!(source, "all" | "workspace" | "core" | "community" | "local"),
            "skill.list source must be all, workspace, core, community, or local"
        );
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).min(DEFAULT_LIMIT);
        let trusted_only = args.trusted_only.unwrap_or(false);
        let registry = builtin_registry()?;
        let workspace =
            workspace_skill_report_with_trust(&self.workspace_root, &self.trust_store_path)?;
        let workspace_overrides = workspace
            .skills
            .iter()
            .filter_map(|skill| {
                skill
                    .overrides_builtin
                    .then(|| skill.name.clone())
                    .flatten()
            })
            .collect::<std::collections::BTreeSet<_>>();
        let mut output = format!(
            "tool=skill.list\nsource={source}\ntrusted_only={trusted_only}\nlimit={limit}\nworkspace_state={:?}\n---",
            workspace.state
        );
        let mut emitted = 0usize;
        if source != "workspace" && source != "community" && source != "local" {
            for skill in registry.skills() {
                if emitted >= limit {
                    break;
                }
                if trusted_only && !skill.permits_context_injection() {
                    continue;
                }
                emitted += 1;
                output.push('\n');
                output.push_str(&format!(
                    "skill id={} source={} pack={} usable={} overridden_by_workspace={} allowed={} requestable={} denied={}",
                    skill.harness.id,
                    skill.harness.source.as_str(),
                    skill.harness.pack,
                    skill.permits_context_injection(),
                    workspace_overrides.contains(&skill.harness.name),
                    render_tool_ids(&skill.harness.allowed_tools),
                    render_tool_ids(&skill.harness.requestable_tools),
                    render_tool_ids(&skill.harness.denied_tools)
                ));
            }
        }
        for skill in &workspace.skills {
            if emitted >= limit {
                break;
            }
            if !workspace_skill_source_matches(source, skill) {
                continue;
            }
            if trusted_only && !skill.usable {
                continue;
            }
            emitted += 1;
            output.push('\n');
            output.push_str(&render_workspace_skill_line(skill));
        }
        Ok(output)
    }

    fn inspect(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<InspectArgs>(agl_tools::SKILL_INSPECT_TOOL_ID, arguments)?;
        let include_body = args.include_body.unwrap_or(false);
        let include_references = args.include_references.unwrap_or(false);
        let max_bytes = args.max_bytes.unwrap_or(MAX_BODY_BYTES).min(MAX_BODY_BYTES);
        let registry = builtin_registry()?;
        let workspace =
            workspace_skill_report_with_trust(&self.workspace_root, &self.trust_store_path)?;
        let mut found = false;
        let mut output = format!(
            "tool=skill.inspect\nid={}\ninclude_body={include_body}\ninclude_references={include_references}\nmax_bytes={max_bytes}\n---",
            args.id
        );
        for skill in registry
            .skills()
            .iter()
            .filter(|skill| skill.harness.id.as_str() == args.id || skill.harness.name == args.id)
        {
            found = true;
            output.push('\n');
            output.push_str(&format!(
                "skill id={} source={} pack={} version={} usable={} manifest_sha256={} tree_sha256={}",
                skill.harness.id,
                skill.harness.source.as_str(),
                skill.harness.pack,
                skill.harness.version,
                skill.permits_context_injection(),
                skill.harness.manifest_sha256,
                skill.harness.tree_sha256
            ));
            render_harness_details(
                &mut output,
                &skill.harness,
                include_body,
                include_references,
                max_bytes,
            );
        }
        for skill in workspace
            .skills
            .iter()
            .filter(|skill| skill.name.as_deref() == Some(args.id.as_str()))
        {
            found = true;
            output.push('\n');
            output.push_str(&render_workspace_skill_line(skill));
            if let Some(harness) = &skill.harness {
                render_harness_details(
                    &mut output,
                    harness,
                    include_body,
                    include_references,
                    max_bytes,
                );
            }
        }
        ensure!(found, "skill not found: {}", args.id);
        Ok(output)
    }

    fn status(&self, arguments: Value) -> Result<String> {
        parse_args::<StatusArgs>(agl_tools::SKILL_STATUS_TOOL_ID, arguments)?;
        let report =
            workspace_skill_report_with_trust(&self.workspace_root, &self.trust_store_path)?;
        Ok(render_workspace_report(
            agl_tools::SKILL_STATUS_TOOL_ID,
            &report,
        ))
    }

    fn verify(&self, arguments: Value) -> Result<String> {
        parse_args::<VerifyArgs>(agl_tools::SKILL_VERIFY_TOOL_ID, arguments)?;
        let report = workspace_skill_report(&self.workspace_root)?;
        Ok(render_workspace_report(
            agl_tools::SKILL_VERIFY_TOOL_ID,
            &report,
        ))
    }

    fn lock(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<LockArgs>(agl_tools::SKILL_LOCK_TOOL_ID, arguments)?;
        let report = lock_workspace_skills(
            &self.workspace_root,
            &SkillLockOptions {
                dry_run: args.dry_run.unwrap_or(false),
            },
        )?;
        Ok(format!(
            "tool=skill.lock\nworkspace_root={}\ndry_run={}\nwrote={}\nwarnings={}\nerrors={}",
            report.workspace_root.display(),
            report.dry_run,
            report.wrote,
            report.warnings.len(),
            report.errors.len()
        ))
    }

    fn trust(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<TrustArgs>(agl_tools::SKILL_TRUST_TOOL_ID, arguments)?;
        let report = trust_workspace_skill(
            &self.workspace_root,
            &self.trust_store_path,
            &args.name,
            &SkillTrustOptions {
                approve: args.approve.unwrap_or(true),
                agentlibre_version: self.agentlibre_version.clone(),
            },
        )?;
        Ok(format!(
            "tool=skill.trust\nskill={}\naction={:?}\nwrote={}\nwarnings={}\nerrors={}",
            report.skill_name,
            report.action,
            report.wrote,
            report.warnings.len(),
            report.errors.len()
        ))
    }

    fn revoke(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<RevokeArgs>(agl_tools::SKILL_REVOKE_TOOL_ID, arguments)?;
        let report =
            revoke_workspace_skill(&self.workspace_root, &self.trust_store_path, &args.name)?;
        Ok(format!(
            "tool=skill.revoke\nskill={}\naction={:?}\nwrote={}\nwarnings={}\nerrors={}",
            report.skill_name,
            report.action,
            report.wrote,
            report.warnings.len(),
            report.errors.len()
        ))
    }
}

impl ToolHandler for SkillTools {
    fn dispatch(&self, input: ToolInput) -> Result<ToolOutput> {
        let observation = self.dispatch(input.id.as_str(), input.arguments)?;
        Ok(ToolOutput { observation })
    }
}

fn parse_args<T: for<'de> Deserialize<'de>>(tool: &str, arguments: Value) -> Result<T> {
    serde_json::from_value(arguments).with_context(|| format!("{tool} arguments are invalid"))
}

fn render_workspace_report(tool_id: &str, report: &agl_skills::WorkspaceSkillReport) -> String {
    let mut output = format!(
        "tool={tool_id}\nstate={:?}\nworkspace_root={}\nskills={}\ndiagnostics={}\nwarnings={}\nerrors={}\n---",
        report.state,
        report.workspace_root.display(),
        report.skills.len(),
        report.diagnostics.len(),
        report.warnings.len(),
        report.errors.len()
    );
    for skill in &report.skills {
        output.push('\n');
        output.push_str(&render_workspace_skill_line(skill));
        for folder in &skill.artifact_folders {
            output.push_str(&format!(
                "\nskill.{}.folder id={} path={} kind={:?} access={:?} exists={}",
                workspace_skill_key(skill),
                folder.id,
                folder.path.display(),
                folder.kind,
                folder.access,
                folder.exists
            ));
            for readiness in &folder.readiness {
                output.push_str(&format!(
                    "\nskill.{}.folder.{}.ready.when={} action={}",
                    workspace_skill_key(skill),
                    folder.id,
                    skill_folder_create_situation(readiness.situation),
                    skill_folder_sync_action(readiness.action)
                ));
            }
        }
    }
    for diagnostic in &report.diagnostics {
        output.push('\n');
        output.push_str(&render_workspace_skill_diagnostic(diagnostic));
    }
    output
}

fn render_workspace_skill_line(skill: &WorkspaceSkillStatus) -> String {
    format!(
        "skill id={} source={} usable={} trust={:?} valid={} broadens_builtin_routing={} folders={} allowed={} requestable={} denied={}",
        workspace_skill_key(skill),
        skill.source.as_deref().unwrap_or("unknown"),
        skill.usable,
        skill.trust_state,
        skill.valid,
        skill.broadens_builtin_routing,
        skill.artifact_folders.len(),
        skill.allowed_tools.join(","),
        skill.requestable_tools.join(","),
        skill.denied_tools.join(",")
    )
}

fn workspace_skill_source_matches(source: &str, skill: &WorkspaceSkillStatus) -> bool {
    match source {
        "all" | "workspace" => true,
        "core" | "community" | "local" => skill.source.as_deref() == Some(source),
        _ => false,
    }
}

fn workspace_skill_key(skill: &WorkspaceSkillStatus) -> String {
    skill
        .name
        .clone()
        .unwrap_or_else(|| format!("path:{}", skill.path.display()))
}

fn render_workspace_skill_diagnostic(diagnostic: &agl_skills::WorkspaceSkillDiagnostic) -> String {
    let mut output = format!(
        "diagnostic severity={} scope={} code={} message={}",
        workspace_skill_diagnostic_severity(diagnostic.severity),
        workspace_skill_diagnostic_scope(diagnostic.scope),
        diagnostic.code,
        diagnostic.message
    );
    if let Some(component) = &diagnostic.component {
        output.push_str(&format!(" component={component}"));
    }
    if let Some(skill) = &diagnostic.skill {
        output.push_str(&format!(" skill={skill}"));
    }
    if let Some(skill_path) = &diagnostic.skill_path {
        output.push_str(&format!(" skill_path={}", skill_path.display()));
    }
    if let Some(folder_id) = &diagnostic.folder_id {
        output.push_str(&format!(" folder={folder_id}"));
    }
    if let Some(path) = &diagnostic.path {
        output.push_str(&format!(" path={}", path.display()));
    }
    output
}

fn workspace_skill_diagnostic_severity(
    severity: agl_skills::WorkspaceSkillDiagnosticSeverity,
) -> &'static str {
    match severity {
        agl_skills::WorkspaceSkillDiagnosticSeverity::Warning => "warning",
        agl_skills::WorkspaceSkillDiagnosticSeverity::Error => "error",
    }
}

fn workspace_skill_diagnostic_scope(
    scope: agl_skills::WorkspaceSkillDiagnosticScope,
) -> &'static str {
    match scope {
        agl_skills::WorkspaceSkillDiagnosticScope::Workspace => "workspace",
        agl_skills::WorkspaceSkillDiagnosticScope::Component => "component",
        agl_skills::WorkspaceSkillDiagnosticScope::Lock => "lock",
        agl_skills::WorkspaceSkillDiagnosticScope::SkillManifest => "skill_manifest",
        agl_skills::WorkspaceSkillDiagnosticScope::SkillArtifactFolder => "skill_artifact_folder",
        agl_skills::WorkspaceSkillDiagnosticScope::SkillTrust => "skill_trust",
    }
}

fn render_harness_details(
    output: &mut String,
    harness: &agl_skills::SkillHarness,
    include_body: bool,
    include_references: bool,
    max_bytes: usize,
) {
    output.push_str(&format!(
        "\ndescription={}\nrequired_hooks={}\nallowed_tools={}\nrequestable_tools={}\ndenied_tools={}",
        harness.description,
        harness
            .required_hooks
            .iter()
            .map(agl_tools::HookId::as_str)
            .collect::<Vec<_>>()
            .join(","),
        render_tool_ids(&harness.allowed_tools),
        render_tool_ids(&harness.requestable_tools),
        render_tool_ids(&harness.denied_tools)
    ));
    for artifact in &harness.artifacts {
        output.push_str(&format!(
            "\nfolder id={} path={} kind={:?} access={:?} create={} provides={} schema={}",
            artifact.id,
            artifact.path.display(),
            artifact.kind,
            artifact.access,
            artifact
                .create
                .iter()
                .map(|rule| skill_folder_create_situation(rule.when))
                .collect::<Vec<_>>()
                .join(","),
            artifact.provides.join(","),
            artifact.schema.as_deref().unwrap_or("")
        ));
    }
    if include_body {
        output.push_str("\nbody_truncated=");
        output.push_str(if harness.body.len() > max_bytes {
            "true\n"
        } else {
            "false\n"
        });
        output.push_str(truncate_str(&harness.body, max_bytes));
    }
    if include_references {
        for reference in &harness.references {
            output.push_str(&format!(
                "\nreference path={} sha256={} bytes={}",
                reference.path,
                reference.sha256,
                reference.content.len()
            ));
        }
    }
}

fn render_tool_ids(tools: &[agl_tools::ToolId]) -> String {
    tools
        .iter()
        .map(agl_tools::ToolId::as_str)
        .collect::<Vec<_>>()
        .join(",")
}

fn skill_folder_create_situation(when: agl_skills::SkillFolderCreateSituation) -> &'static str {
    match when {
        agl_skills::SkillFolderCreateSituation::SkillSync => "skill_sync",
        agl_skills::SkillFolderCreateSituation::RuntimePrepare => "runtime_prepare",
        agl_skills::SkillFolderCreateSituation::ArtifactWrite => "artifact_write",
    }
}

fn skill_folder_sync_action(action: agl_skills::SkillFolderSyncActionKind) -> &'static str {
    match action {
        agl_skills::SkillFolderSyncActionKind::Exists => "exists",
        agl_skills::SkillFolderSyncActionKind::SkippedReadOnly => "skipped_read_only",
        agl_skills::SkillFolderSyncActionKind::SkippedSource => "skipped_source",
        agl_skills::SkillFolderSyncActionKind::SkippedNoCreateRule => "skipped_no_create_rule",
        agl_skills::SkillFolderSyncActionKind::SkippedSituationMismatch => {
            "skipped_situation_mismatch"
        }
        agl_skills::SkillFolderSyncActionKind::WouldCreateDir => "would_create_dir",
        agl_skills::SkillFolderSyncActionKind::CreatedDir => "created_dir",
    }
}

fn truncate_str(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut index = max_bytes;
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    &value[..index]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    source: Option<String>,
    trusted_only: Option<bool>,
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectArgs {
    id: String,
    include_body: Option<bool>,
    include_references: Option<bool>,
    max_bytes: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifyArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LockArgs {
    dry_run: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustArgs {
    name: String,
    approve: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeArgs {
    name: String,
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::*;

    #[test]
    fn skill_tools_list_and_inspect_core_skills() {
        let root = temp_root("list-inspect");
        std::fs::create_dir_all(&root).unwrap();
        let tools = SkillTools::new(&root, root.join("skill-trust.toml"), "test");

        let list = tools
            .dispatch(agl_tools::SKILL_LIST_TOOL_ID, json!({"source": "core"}))
            .unwrap();
        let inspect = tools
            .dispatch(
                agl_tools::SKILL_INSPECT_TOOL_ID,
                json!({"id": "skill", "include_references": true}),
            )
            .unwrap();

        assert!(list.contains("tool=skill.list"));
        assert!(list.contains("skill id=skill"));
        assert!(list.contains("source=core"));
        assert!(inspect.contains("tool=skill.inspect"));
        assert!(inspect.contains("skill id=skill"));
        assert!(inspect.contains("source=core"));
        assert!(inspect.contains("manifest_sha256="));
        assert!(!inspect.contains("reference path="));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn skill_tools_status_and_lock_dry_run_report_workspace_errors() {
        let root = temp_root("status-lock");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".agl/skills")).unwrap();
        let tools = SkillTools::new(&root, root.join("skill-trust.toml"), "test");

        let status = tools
            .dispatch(agl_tools::SKILL_STATUS_TOOL_ID, json!({}))
            .unwrap();
        let lock = tools
            .dispatch(agl_tools::SKILL_LOCK_TOOL_ID, json!({"dry_run": true}))
            .unwrap();

        assert!(status.contains("tool=skill.status"));
        assert!(status.contains("diagnostics="));
        assert!(status.contains("diagnostic severity=error"));
        assert!(status.contains("errors="));
        assert!(lock.contains("tool=skill.lock"));
        assert!(lock.contains("dry_run=true"));

        let _ = std::fs::remove_dir_all(root);
    }

    fn temp_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "agl-host-tools-{label}-{}-{nanos}",
            std::process::id()
        ))
    }
}
