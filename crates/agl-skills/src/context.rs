use agl_tools::{HookId, SkillId, ToolCatalog, ToolId};
use serde::Serialize;

use crate::{SkillRegistry, SkillRegistryError};

const APPROX_BYTES_PER_TOKEN: usize = 4;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillContextBundle {
    pub content: String,
    pub evidence: Vec<SkillContextEvidence>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillContextBlock {
    pub content: String,
    pub evidence: SkillContextEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkillContextEvidence {
    pub skill_id: String,
    pub source: String,
    pub pack: String,
    pub manifest_sha256: String,
    pub tree_sha256: String,
    pub required_hooks: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub requestable_tools: Vec<String>,
    pub denied_tools: Vec<String>,
    pub permission_request_templates: Vec<SkillPermissionRequestTemplateEvidence>,
    pub memory_read_scopes: Vec<String>,
    pub notes_read: bool,
    pub notes_write: bool,
    pub included_references: Vec<SkillContextReferenceEvidence>,
    pub context_budget_tokens: u32,
    pub budget_bytes: usize,
    pub context_bytes: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkillPermissionRequestTemplateEvidence {
    pub id: String,
    pub tools: Vec<String>,
    pub max_operation_kind: Option<String>,
    pub state_effects: Vec<String>,
    pub default_duration: String,
    pub reason_template: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkillContextReferenceEvidence {
    pub path: String,
    pub sha256: String,
    pub bytes: usize,
}

#[derive(Debug, Eq, PartialEq)]
pub enum SkillContextError {
    Registry(SkillRegistryError),
}

impl std::fmt::Display for SkillContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Registry(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for SkillContextError {}

impl From<SkillRegistryError> for SkillContextError {
    fn from(err: SkillRegistryError) -> Self {
        Self::Registry(err)
    }
}

pub fn build_verified_context_bundle(
    registry: &SkillRegistry,
    tool_catalog: &ToolCatalog,
    selections: &[SkillId],
) -> Result<SkillContextBundle, SkillContextError> {
    let mut blocks = Vec::with_capacity(selections.len());
    for skill_id in selections {
        registry.verify_required_hooks(skill_id, tool_catalog)?;
        registry.verify_allowed_tools(skill_id, tool_catalog)?;
        let skill = registry.resolve_for_context_injection(skill_id)?;
        blocks.push(build_context_block(skill));
    }

    Ok(SkillContextBundle {
        content: blocks
            .iter()
            .map(|block| block.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
        evidence: blocks.into_iter().map(|block| block.evidence).collect(),
    })
}

fn build_context_block(skill: &crate::RegisteredSkill) -> SkillContextBlock {
    let harness = &skill.harness;
    let mut content = String::new();
    content.push_str("<agentlibre_skill_context>\n");
    content.push_str(&format!("skill_id: {}\n", harness.id));
    content.push_str(&format!("source: {}\n", harness.source.as_str()));
    content.push_str(&format!("pack: {}\n", harness.pack));
    content.push_str("\n## Tool Routing\n\n");
    content.push_str("directly_callable_tools: ");
    content.push_str(&render_tools(&harness.allowed_tools));
    content.push('\n');
    content.push_str("requestable_tools: ");
    content.push_str(&render_tools(&harness.requestable_tools));
    content.push('\n');
    content.push_str("unavailable_tools: ");
    content.push_str(&render_tools(&harness.denied_tools));
    content.push('\n');
    if !harness.permission_request_templates.is_empty() {
        content.push_str("permission_request_templates:\n");
        for template in &harness.permission_request_templates {
            content.push_str(&format!(
                "- id: {}; tools: {}; max_operation_kind: {}; default_duration: {}; reason_template: {}\n",
                template.id,
                render_tools(&template.tools),
                template
                    .max_operation_kind
                    .map(|kind| kind.as_str())
                    .unwrap_or("unspecified"),
                template.default_duration,
                template.reason_template
            ));
        }
    }
    content.push_str(
        "Requestable tools are not callable unless they also appear in agentlibre_tool_context.\n",
    );
    content.push_str("\n## Skill Instructions\n\n");
    content.push_str(harness.body.trim());
    for reference in &harness.references {
        content.push_str("\n\n## Reference: ");
        content.push_str(&reference.path);
        content.push_str("\n\n");
        content.push_str(reference.content.trim());
    }
    content.push_str("\n</agentlibre_skill_context>\n");

    let budget_bytes = harness.context_budget_tokens as usize * APPROX_BYTES_PER_TOKEN;
    let mut truncated = false;
    if content.len() > budget_bytes {
        truncated = true;
        content.truncate(previous_char_boundary(&content, budget_bytes));
        content.push_str("\n[skill context truncated]\n");
    }

    let evidence = SkillContextEvidence {
        skill_id: harness.id.as_str().to_string(),
        source: harness.source.as_str().to_string(),
        pack: harness.pack.clone(),
        manifest_sha256: harness.manifest_sha256.clone(),
        tree_sha256: harness.tree_sha256.clone(),
        required_hooks: harness
            .required_hooks
            .iter()
            .map(HookId::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        allowed_tools: harness
            .allowed_tools
            .iter()
            .map(ToolId::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        requestable_tools: harness
            .requestable_tools
            .iter()
            .map(ToolId::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        denied_tools: harness
            .denied_tools
            .iter()
            .map(ToolId::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        permission_request_templates: harness
            .permission_request_templates
            .iter()
            .map(|template| SkillPermissionRequestTemplateEvidence {
                id: template.id.clone(),
                tools: template
                    .tools
                    .iter()
                    .map(ToolId::as_str)
                    .map(ToOwned::to_owned)
                    .collect(),
                max_operation_kind: template
                    .max_operation_kind
                    .map(|kind| kind.as_str().to_string()),
                state_effects: template
                    .state_effects
                    .iter()
                    .map(|effect| state_effect_as_str(*effect).to_string())
                    .collect(),
                default_duration: template.default_duration.clone(),
                reason_template: template.reason_template.clone(),
            })
            .collect(),
        memory_read_scopes: harness
            .permissions
            .memory
            .read
            .iter()
            .map(|scope| scope.as_str().to_string())
            .collect(),
        notes_read: harness.permissions.notes.read,
        notes_write: harness.permissions.notes.write,
        included_references: harness
            .references
            .iter()
            .map(|reference| SkillContextReferenceEvidence {
                path: reference.path.clone(),
                sha256: reference.sha256.clone(),
                bytes: reference.content.len(),
            })
            .collect(),
        context_budget_tokens: harness.context_budget_tokens,
        budget_bytes,
        context_bytes: content.len(),
        truncated,
    };

    SkillContextBlock { content, evidence }
}

fn render_tools(tools: &[ToolId]) -> String {
    if tools.is_empty() {
        "[]".to_string()
    } else {
        tools
            .iter()
            .map(ToolId::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn state_effect_as_str(effect: agl_tools::ToolStateEffect) -> &'static str {
    match effect {
        agl_tools::ToolStateEffect::RepoFiles => "repo_files",
        agl_tools::ToolStateEffect::RepoWorkspace => "repo_workspace",
        agl_tools::ToolStateEffect::RepoHooks => "repo_hooks",
        agl_tools::ToolStateEffect::StoreMemoryEntries => "store_memory_entries",
        agl_tools::ToolStateEffect::StoreMemorySuggestions => "store_memory_suggestions",
        agl_tools::ToolStateEffect::StoreNotes => "store_notes",
        agl_tools::ToolStateEffect::StoreNoteLinks => "store_note_links",
        agl_tools::ToolStateEffect::StoreCron => "store_cron",
        agl_tools::ToolStateEffect::MatrixOutbox => "matrix_outbox",
        agl_tools::ToolStateEffect::StoreIdempotency => "store_idempotency",
        agl_tools::ToolStateEffect::StorePermissionRequests => "store_permission_requests",
        agl_tools::ToolStateEffect::StorePermissionGrants => "store_permission_grants",
    }
}

fn previous_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use agl_tools::ToolCatalog;

    use super::*;

    #[test]
    fn verified_context_bundle_records_hashes_without_reference_text_in_evidence() {
        let registry = SkillRegistry::from_builtin_assets().unwrap();
        let mut tool_catalog = ToolCatalog::new();
        agl_tools::guards::register(&mut tool_catalog).unwrap();
        agl_tools::fs::register(&mut tool_catalog).unwrap();

        let bundle = build_verified_context_bundle(
            &registry,
            &tool_catalog,
            &[SkillId::new("task-spec").unwrap()],
        )
        .unwrap();

        assert!(bundle.content.contains("Use this skill"));
        assert!(bundle.content.contains("Task Spec Contract"));
        assert_eq!(bundle.evidence.len(), 1);
        assert_eq!(bundle.evidence[0].skill_id, "task-spec");
        assert_eq!(
            bundle.evidence[0].required_hooks,
            vec!["repo_path.validate", "task_spec.validate"]
        );
        assert_eq!(
            bundle.evidence[0].allowed_tools,
            vec!["fs.edit", "fs.list", "fs.read", "fs.search"]
        );
        assert!(
            bundle
                .content
                .contains("directly_callable_tools: fs.edit, fs.list, fs.read, fs.search")
        );
        assert!(bundle.content.contains("requestable_tools: []"));
        assert!(
            bundle
                .content
                .contains("Requestable tools are not callable unless they also appear")
        );
        assert!(bundle.evidence[0].requestable_tools.is_empty());
        assert!(bundle.evidence[0].denied_tools.is_empty());
        assert!(bundle.evidence[0].permission_request_templates.is_empty());
        assert_eq!(
            bundle.evidence[0].included_references[0].path,
            "references/task-spec-contract.md"
        );
        assert!(!format!("{:?}", bundle.evidence).contains("Task Spec Contract"));
    }

    #[test]
    fn context_distinguishes_callable_from_requestable_tools() {
        let registry = SkillRegistry::from_builtin_assets().unwrap();
        let mut tool_catalog = ToolCatalog::new();
        agl_tools::guards::register(&mut tool_catalog).unwrap();
        agl_tools::cron::register(&mut tool_catalog).unwrap();
        agl_tools::fs::register(&mut tool_catalog).unwrap();
        agl_tools::matrix::register(&mut tool_catalog).unwrap();

        let bundle = build_verified_context_bundle(
            &registry,
            &tool_catalog,
            &[SkillId::new("cron-planner").unwrap()],
        )
        .unwrap();

        assert!(
            bundle
                .content
                .contains("directly_callable_tools: cron.preflight, fs.read, fs.search")
        );
        assert!(
            bundle
                .content
                .contains("requestable_tools: cron.add, matrix.outbox.enqueue")
        );
        assert!(
            bundle
                .content
                .contains("unavailable_tools: matrix.outbox.deliver")
        );
        assert!(bundle.content.contains("id: schedule-matrix-cron"));
        assert_eq!(
            bundle.evidence[0].allowed_tools,
            vec!["cron.preflight", "fs.read", "fs.search"]
        );
        assert_eq!(
            bundle.evidence[0].requestable_tools,
            vec!["cron.add", "matrix.outbox.enqueue"]
        );
        assert_eq!(
            bundle.evidence[0].denied_tools,
            vec!["matrix.outbox.deliver"]
        );
        assert_eq!(
            bundle.evidence[0].permission_request_templates[0].tools,
            vec!["cron.add", "matrix.outbox.enqueue"]
        );
    }
}
