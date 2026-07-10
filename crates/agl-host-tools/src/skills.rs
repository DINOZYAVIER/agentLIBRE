use std::path::{Path, PathBuf};

use agl_capabilities::{
    ActionHandler, ActionHandlerError, ActionInvocation, ActionResult, CapabilityId,
};
use agl_skills::{
    SkillHarness, SkillLockOptions, SkillTrustOptions, WorkspaceSkillStatus, builtin_registry,
    lock_workspace_skills, revoke_workspace_skill, trust_workspace_skill, workspace_skill_report,
    workspace_skill_report_with_trust,
};
use agl_tools::skills::{
    SkillInspectArgs, SkillListArgs, SkillListSource, SkillLockArgs, SkillRevokeArgs,
    SkillStatusArgs, SkillTrustArgs, SkillVerifyArgs,
};
use anyhow::{Context, Result, ensure};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

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

    fn dispatch_action(&self, id: &CapabilityId, arguments: Value) -> Result<ActionResult> {
        let data = match id.as_str() {
            agl_tools::SKILL_LIST_TOOL_ID => self.list(parse_args(id.as_str(), arguments)?)?,
            agl_tools::SKILL_INSPECT_TOOL_ID => {
                self.inspect(parse_args(id.as_str(), arguments)?)?
            }
            agl_tools::SKILL_STATUS_TOOL_ID => self.status(parse_args(id.as_str(), arguments)?)?,
            agl_tools::SKILL_VERIFY_TOOL_ID => self.verify(parse_args(id.as_str(), arguments)?)?,
            agl_tools::SKILL_LOCK_TOOL_ID => self.lock(parse_args(id.as_str(), arguments)?)?,
            agl_tools::SKILL_TRUST_TOOL_ID => self.trust(parse_args(id.as_str(), arguments)?)?,
            agl_tools::SKILL_REVOKE_TOOL_ID => self.revoke(parse_args(id.as_str(), arguments)?)?,
            _ => anyhow::bail!("unknown skill capability `{id}`"),
        };
        Ok(ActionResult::new(data))
    }

    fn list(&self, args: SkillListArgs) -> Result<Value> {
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).min(DEFAULT_LIMIT);
        let registry = builtin_registry()?;
        let workspace =
            workspace_skill_report_with_trust(&self.workspace_root, &self.trust_store_path)?;
        let workspace_overrides = workspace
            .skills
            .iter()
            .filter(|skill| skill.overrides_builtin)
            .filter_map(|skill| skill.name.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let mut skills = Vec::new();

        if matches!(args.source, SkillListSource::All | SkillListSource::Core) {
            for skill in registry.skills() {
                if args.trusted_only && !skill.permits_context_injection() {
                    continue;
                }
                skills.push(json!({
                    "id": skill.harness.id.as_str(),
                    "source": skill.harness.source,
                    "pack": skill.harness.pack,
                    "version": skill.harness.version,
                    "usable": skill.permits_context_injection(),
                    "trust": skill.trust,
                    "overridden_by_workspace": workspace_overrides.contains(&skill.harness.name),
                    "routing": routing_summary(&skill.harness),
                }));
            }
        }
        for skill in &workspace.skills {
            if !workspace_skill_source_matches(args.source, skill) {
                continue;
            }
            if args.trusted_only && !skill.usable {
                continue;
            }
            skills.push(workspace_skill_summary(skill));
        }

        let total = skills.len();
        skills.truncate(limit);
        Ok(json!({
            "capability_id": agl_tools::SKILL_LIST_TOOL_ID,
            "source": args.source.as_str(),
            "trusted_only": args.trusted_only,
            "limit": limit,
            "workspace_state": workspace.state,
            "skills": skills,
            "total": total,
            "truncated": total > limit,
        }))
    }

    fn inspect(&self, args: SkillInspectArgs) -> Result<Value> {
        let max_bytes = args.max_bytes.unwrap_or(MAX_BODY_BYTES).min(MAX_BODY_BYTES);
        let registry = builtin_registry()?;
        let workspace =
            workspace_skill_report_with_trust(&self.workspace_root, &self.trust_store_path)?;
        let mut matches = Vec::new();

        for skill in registry
            .skills()
            .iter()
            .filter(|skill| skill.harness.id.as_str() == args.id || skill.harness.name == args.id)
        {
            matches.push(json!({
                "kind": "builtin",
                "trust": skill.trust,
                "usable": skill.permits_context_injection(),
                "harness": harness_details(
                    &skill.harness,
                    args.include_body,
                    args.include_references,
                    max_bytes,
                ),
            }));
        }
        for skill in workspace
            .skills
            .iter()
            .filter(|skill| skill.name.as_deref() == Some(args.id.as_str()))
        {
            matches.push(json!({
                "kind": "workspace",
                "status": skill,
                "harness": skill.harness.as_ref().map(|harness| harness_details(
                    harness,
                    args.include_body,
                    args.include_references,
                    max_bytes,
                )),
            }));
        }
        ensure!(!matches.is_empty(), "skill not found: {}", args.id);

        Ok(json!({
            "capability_id": agl_tools::SKILL_INSPECT_TOOL_ID,
            "id": args.id,
            "include_body": args.include_body,
            "include_references": args.include_references,
            "max_bytes": max_bytes,
            "matches": matches,
        }))
    }

    fn status(&self, _args: SkillStatusArgs) -> Result<Value> {
        let report =
            workspace_skill_report_with_trust(&self.workspace_root, &self.trust_store_path)?;
        report_value(agl_tools::SKILL_STATUS_TOOL_ID, &report)
    }

    fn verify(&self, _args: SkillVerifyArgs) -> Result<Value> {
        let report = workspace_skill_report(&self.workspace_root)?;
        report_value(agl_tools::SKILL_VERIFY_TOOL_ID, &report)
    }

    fn lock(&self, args: SkillLockArgs) -> Result<Value> {
        let report = lock_workspace_skills(
            &self.workspace_root,
            &SkillLockOptions {
                dry_run: args.dry_run,
            },
        )?;
        Ok(json!({
            "capability_id": agl_tools::SKILL_LOCK_TOOL_ID,
            "report": report,
        }))
    }

    fn trust(&self, args: SkillTrustArgs) -> Result<Value> {
        let report = trust_workspace_skill(
            &self.workspace_root,
            &self.trust_store_path,
            &args.name,
            &SkillTrustOptions {
                approve: args.approve,
                agentlibre_version: self.agentlibre_version.clone(),
            },
        )?;
        Ok(json!({
            "capability_id": agl_tools::SKILL_TRUST_TOOL_ID,
            "report": report,
        }))
    }

    fn revoke(&self, args: SkillRevokeArgs) -> Result<Value> {
        let report =
            revoke_workspace_skill(&self.workspace_root, &self.trust_store_path, &args.name)?;
        Ok(json!({
            "capability_id": agl_tools::SKILL_REVOKE_TOOL_ID,
            "report": report,
        }))
    }
}

impl ActionHandler for SkillTools {
    fn dispatch(&self, invocation: ActionInvocation) -> Result<ActionResult, ActionHandlerError> {
        self.dispatch_action(&invocation.capability_id, invocation.arguments)
            .map_err(Into::into)
    }
}

fn parse_args<T: DeserializeOwned>(capability_id: &str, arguments: Value) -> Result<T> {
    serde_json::from_value(arguments)
        .with_context(|| format!("{capability_id} arguments are invalid"))
}

fn report_value(capability_id: &str, report: &agl_skills::WorkspaceSkillReport) -> Result<Value> {
    Ok(json!({
        "capability_id": capability_id,
        "report": report,
    }))
}

fn routing_summary(harness: &SkillHarness) -> Value {
    json!({
        "required_hooks": id_strings(&harness.required_hooks),
        "allowed": id_strings(&harness.allowed_tools),
        "requestable": id_strings(&harness.requestable_tools),
        "denied": id_strings(&harness.denied_tools),
    })
}

fn harness_details(
    harness: &SkillHarness,
    include_body: bool,
    include_references: bool,
    max_bytes: usize,
) -> Value {
    let body = include_body.then(|| {
        json!({
            "content": truncate_str(&harness.body, max_bytes),
            "truncated": harness.body.len() > max_bytes,
        })
    });
    let references = include_references.then(|| {
        harness
            .references
            .iter()
            .map(|reference| {
                json!({
                    "path": reference.path,
                    "sha256": reference.sha256,
                    "bytes": reference.content.len(),
                })
            })
            .collect::<Vec<_>>()
    });
    json!({
        "id": harness.id.as_str(),
        "name": harness.name,
        "description": harness.description,
        "version": harness.version,
        "source": harness.source,
        "pack": harness.pack,
        "manifest_sha256": harness.manifest_sha256,
        "tree_sha256": harness.tree_sha256,
        "routing": routing_summary(harness),
        "permission_request_templates": harness.permission_request_templates,
        "permissions": harness.permissions,
        "artifacts": harness.artifacts,
        "guarantees": harness.guarantees,
        "body": body,
        "references": references,
    })
}

fn workspace_skill_summary(skill: &WorkspaceSkillStatus) -> Value {
    json!({
        "id": workspace_skill_key(skill),
        "source": skill.source.as_deref().unwrap_or("unknown"),
        "usable": skill.usable,
        "trust": skill.trust_state,
        "valid": skill.valid,
        "broadens_builtin_routing": skill.broadens_builtin_routing,
        "artifact_folder_count": skill.artifact_folders.len(),
        "routing": {
            "allowed": skill.allowed_tools,
            "requestable": skill.requestable_tools,
            "denied": skill.denied_tools,
        },
    })
}

fn workspace_skill_source_matches(source: SkillListSource, skill: &WorkspaceSkillStatus) -> bool {
    match source {
        SkillListSource::All | SkillListSource::Workspace => true,
        SkillListSource::Core => skill.source.as_deref() == Some("core"),
        SkillListSource::Community => skill.source.as_deref() == Some("community"),
        SkillListSource::Local => skill.source.as_deref() == Some("local"),
    }
}

fn workspace_skill_key(skill: &WorkspaceSkillStatus) -> String {
    skill
        .name
        .clone()
        .unwrap_or_else(|| format!("path:{}", skill.path.display()))
}

fn id_strings<T: ToString>(ids: &[T]) -> Vec<String> {
    ids.iter().map(ToString::to_string).collect()
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

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::*;

    #[test]
    fn skill_tools_list_and_inspect_return_structured_core_skills() {
        let root = temp_root("list-inspect");
        std::fs::create_dir_all(&root).unwrap();
        let tools = SkillTools::new(&root, root.join("skill-trust.toml"), "test");

        let list = tools
            .dispatch_action(
                &CapabilityId::new(agl_tools::SKILL_LIST_TOOL_ID).unwrap(),
                json!({"source": "core"}),
            )
            .unwrap();
        let inspect = tools
            .dispatch_action(
                &CapabilityId::new(agl_tools::SKILL_INSPECT_TOOL_ID).unwrap(),
                json!({"id": "skill", "include_references": true}),
            )
            .unwrap();

        assert_eq!(list.data["capability_id"], agl_tools::SKILL_LIST_TOOL_ID);
        assert!(
            list.data["skills"]
                .as_array()
                .unwrap()
                .iter()
                .any(|skill| skill["id"] == "skill" && skill["source"] == "core")
        );
        let matches = inspect.data["matches"].as_array().unwrap();
        assert_eq!(matches[0]["harness"]["id"], "skill");
        assert!(matches[0]["harness"]["manifest_sha256"].is_string());
        assert!(matches[0]["harness"]["references"].is_array());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn skill_tools_status_and_lock_return_structured_reports() {
        let root = temp_root("status-lock");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".agl/skills")).unwrap();
        let tools = SkillTools::new(&root, root.join("skill-trust.toml"), "test");

        let status = tools
            .dispatch_action(
                &CapabilityId::new(agl_tools::SKILL_STATUS_TOOL_ID).unwrap(),
                json!({}),
            )
            .unwrap();
        let lock = tools
            .dispatch_action(
                &CapabilityId::new(agl_tools::SKILL_LOCK_TOOL_ID).unwrap(),
                json!({"dry_run": true}),
            )
            .unwrap();

        assert!(status.data["report"]["diagnostics"].is_array());
        assert!(status.data["report"]["errors"].is_array());
        assert_eq!(lock.data["report"]["dry_run"], true);
        assert!(lock.data["report"]["errors"].is_array());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn shared_argument_dtos_reject_unknown_fields_in_handler_too() {
        let root = temp_root("unknown");
        std::fs::create_dir_all(&root).unwrap();
        let tools = SkillTools::new(&root, root.join("skill-trust.toml"), "test");
        let error = tools
            .dispatch_action(
                &CapabilityId::new(agl_tools::SKILL_LIST_TOOL_ID).unwrap(),
                json!({"unknown": true}),
            )
            .unwrap_err();
        assert!(error.to_string().contains("arguments are invalid"));
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
