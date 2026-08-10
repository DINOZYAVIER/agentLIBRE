use std::path::{Path, PathBuf};

use agl_core_tools::skills::{
    SkillInspectArgs, SkillListArgs, SkillListSource, SkillRevokeArgs, SkillStatusArgs,
    SkillTrustArgs, SkillVerifyArgs,
};
use agl_kernel::{EffectId, ObservedEffect, ToolDispatchContext, ToolHandler, ToolId, ToolResult};
use agl_skill::{SkillHarness, SkillSource, SkillTrustStore};
use anyhow::{Context, Result, bail, ensure};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

const DEFAULT_LIMIT: usize = 100;
const MAX_BODY_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug)]
pub struct SkillTools {
    workspace_root: PathBuf,
    trust_store_path: PathBuf,
    runtime_paths: agl_runtime::AgentLibrePaths,
}

impl SkillTools {
    pub fn new(
        workspace_root: impl AsRef<Path>,
        _trust_store_path: impl AsRef<Path>,
        _agentlibre_version: impl Into<String>,
        runtime_paths: agl_runtime::AgentLibrePaths,
    ) -> Self {
        Self {
            workspace_root: workspace_root.as_ref().to_path_buf(),
            trust_store_path: _trust_store_path.as_ref().to_path_buf(),
            runtime_paths,
        }
    }

    fn dispatch_action(&self, id: &ToolId, arguments: Value) -> Result<ToolResult> {
        let data = match id.as_str() {
            agl_core_tools::SKILL_LIST_TOOL_ID => self.list(parse_args(id.as_str(), arguments)?)?,
            agl_core_tools::SKILL_INSPECT_TOOL_ID => {
                self.inspect(parse_args(id.as_str(), arguments)?)?
            }
            agl_core_tools::SKILL_STATUS_TOOL_ID => self.status(
                parse_args::<SkillStatusArgs>(id.as_str(), arguments)?,
                false,
            )?,
            agl_core_tools::SKILL_VERIFY_TOOL_ID => {
                self.status(parse_args::<SkillVerifyArgs>(id.as_str(), arguments)?, true)?
            }
            agl_core_tools::SKILL_TRUST_TOOL_ID => {
                let args = parse_args::<SkillTrustArgs>(id.as_str(), arguments)?;
                ensure!(args.approve, "Skill trust requires approve=true");
                let identity = self.update_trust(&args.name, true)?;
                return Ok(ToolResult::new(json!({
                    "tool_id": id,
                    "status": "trusted",
                    "identity": identity,
                }))
                .with_observed_effects([ObservedEffect::new(
                    EffectId::skill_trust(),
                    [("identity".to_owned(), identity)],
                )]));
            }
            agl_core_tools::SKILL_REVOKE_TOOL_ID => {
                let args = parse_args::<SkillRevokeArgs>(id.as_str(), arguments)?;
                let identity = self.update_trust(&args.name, false)?;
                return Ok(ToolResult::new(json!({
                    "tool_id": id,
                    "status": "revoked",
                    "identity": identity,
                }))
                .with_observed_effects([ObservedEffect::new(
                    EffectId::skill_trust(),
                    [("identity".to_owned(), identity)],
                )]));
            }
            _ => anyhow::bail!("unknown skill tool `{id}`"),
        };
        Ok(ToolResult::new(data))
    }

    fn list(&self, args: SkillListArgs) -> Result<Value> {
        let limit = args.limit.unwrap_or(DEFAULT_LIMIT).min(DEFAULT_LIMIT);
        let resolved = self.resolved()?;
        let registry = &resolved.registry;
        let mut skills = Vec::new();

        {
            for skill in registry
                .skills()
                .iter()
                .filter(|skill| source_matches(args.source, skill.harness.source))
            {
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
                    "routing": routing_summary(&skill.harness),
                }));
            }
        }

        let total = skills.len();
        skills.truncate(limit);
        Ok(json!({
            "tool_id": agl_core_tools::SKILL_LIST_TOOL_ID,
            "source": args.source.as_str(),
            "trusted_only": args.trusted_only,
            "limit": limit,
            "skills": skills,
            "total": total,
            "truncated": total > limit,
        }))
    }

    fn inspect(&self, args: SkillInspectArgs) -> Result<Value> {
        let max_bytes = args.max_bytes.unwrap_or(MAX_BODY_BYTES).min(MAX_BODY_BYTES);
        let resolved = self.resolved()?;
        let registry = &resolved.registry;
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
        ensure!(!matches.is_empty(), "skill not found: {}", args.id);

        Ok(json!({
            "tool_id": agl_core_tools::SKILL_INSPECT_TOOL_ID,
            "id": args.id,
            "include_body": args.include_body,
            "include_references": args.include_references,
            "max_bytes": max_bytes,
            "matches": matches,
        }))
    }

    fn resolved(&self) -> Result<agl_runtime::WorkspaceSkillRegistry> {
        agl_runtime::resolve_workspace_skills(
            &self.runtime_paths,
            &self.workspace_root,
            &self.trust_store_path,
        )
    }

    fn status<T>(&self, _args: T, verify: bool) -> Result<Value> {
        let resolved = self.resolved()?;
        let unusable = resolved
            .registry
            .skills()
            .iter()
            .filter(|skill| !skill.permits_context_injection())
            .count();
        let lock_valid = resolved.external_package_count == 0 || resolved.package_lock_present;
        let valid = lock_valid && unusable == 0;
        if verify && !valid {
            bail!(
                "Skill package verification failed: lock_valid={lock_valid}, unusable={unusable}"
            );
        }
        Ok(json!({
            "tool_id": if verify { agl_core_tools::SKILL_VERIFY_TOOL_ID } else { agl_core_tools::SKILL_STATUS_TOOL_ID },
            "status": if valid { "ok" } else { "invalid" },
            "package_count": resolved.registry.skills().len(),
            "external_package_count": resolved.external_package_count,
            "package_lock_present": resolved.package_lock_present,
            "unusable": unusable,
        }))
    }

    fn update_trust(&self, name: &str, approve: bool) -> Result<String> {
        let resolved = self.resolved()?;
        let skill = resolved
            .registry
            .skills()
            .iter()
            .find(|skill| skill.harness.id.as_str() == name || skill.harness.name == name)
            .with_context(|| format!("skill not found: {name}"))?;
        ensure!(
            skill.harness.source != SkillSource::Core,
            "core Skill trust is binary-owned"
        );
        ensure!(
            resolved.package_lock_present,
            "workspace Skill trust requires .agl/package-lock.toml"
        );
        let identity = agl_skill::skill_identity(&skill.harness);
        let mut store = SkillTrustStore::load(&self.trust_store_path)?;
        if approve {
            store.trust(&skill.harness);
        } else {
            store.revoke(&skill.harness);
        }
        store.write_atomic(&self.trust_store_path)?;
        Ok(identity)
    }
}

fn source_matches(filter: SkillListSource, source: SkillSource) -> bool {
    match filter {
        SkillListSource::All => true,
        SkillListSource::Core => source == SkillSource::Core,
        SkillListSource::Community => source == SkillSource::Community,
        SkillListSource::Workspace | SkillListSource::Local => source == SkillSource::Local,
    }
}

impl ToolHandler for SkillTools {
    fn dispatch(&self, context: ToolDispatchContext) -> agl_kernel::ToolHandlerFuture<'_> {
        Box::pin(async move {
            let invocation = context.into_invocation();
            self.dispatch_action(&invocation.tool_id, invocation.arguments)
                .map_err(Into::into)
        })
    }
}

fn parse_args<T: DeserializeOwned>(tool_id: &str, arguments: Value) -> Result<T> {
    serde_json::from_value(arguments).with_context(|| format!("{tool_id} arguments are invalid"))
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
        "guarantees": harness.guarantees,
        "body": body,
        "references": references,
    })
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
        let tools = SkillTools::new(
            &root,
            root.join("skill-trust.toml"),
            "test",
            agl_runtime::AgentLibrePaths::from_agl_home(&root),
        );

        let list = tools
            .dispatch_action(
                &ToolId::new(agl_core_tools::SKILL_LIST_TOOL_ID).unwrap(),
                json!({"source": "core"}),
            )
            .unwrap();
        let inspect = tools
            .dispatch_action(
                &ToolId::new(agl_core_tools::SKILL_INSPECT_TOOL_ID).unwrap(),
                json!({"id": "skill", "include_references": true}),
            )
            .unwrap();

        assert_eq!(list.data["tool_id"], agl_core_tools::SKILL_LIST_TOOL_ID);
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
    fn skill_tools_status_return_structured_report() {
        let root = temp_root("status");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join(".agl/skills")).unwrap();
        let tools = SkillTools::new(
            &root,
            root.join("skill-trust.toml"),
            "test",
            agl_runtime::AgentLibrePaths::from_agl_home(&root),
        );

        let status = tools
            .dispatch_action(
                &ToolId::new(agl_core_tools::SKILL_STATUS_TOOL_ID).unwrap(),
                json!({}),
            )
            .unwrap();
        assert!(status.data["package_count"].is_number());
        assert!(status.data["external_package_count"].is_number());
        assert!(status.data["package_lock_present"].is_boolean());
        assert!(status.data["unusable"].is_number());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn shared_argument_dtos_reject_unknown_fields_in_handler_too() {
        let root = temp_root("unknown");
        std::fs::create_dir_all(&root).unwrap();
        let tools = SkillTools::new(
            &root,
            root.join("skill-trust.toml"),
            "test",
            agl_runtime::AgentLibrePaths::from_agl_home(&root),
        );
        let error = tools
            .dispatch_action(
                &ToolId::new(agl_core_tools::SKILL_LIST_TOOL_ID).unwrap(),
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
