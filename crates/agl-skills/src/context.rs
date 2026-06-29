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
        assert_eq!(
            bundle.evidence[0].included_references[0].path,
            "references/task-spec-contract.md"
        );
        assert!(!format!("{:?}", bundle.evidence).contains("Task Spec Contract"));
    }
}
