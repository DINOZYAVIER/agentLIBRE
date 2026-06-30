use crate::{
    ToolCapability, ToolCatalog, ToolCatalogError, ToolDeclaration, ToolId, ToolOperationKind,
    ToolProviderDeclaration, ToolProviderId, ToolStateEffect,
};

pub const PROVIDER_ID: &str = "skill-tools";
pub const SKILL_LIST_TOOL_ID: &str = "skill.list";
pub const SKILL_INSPECT_TOOL_ID: &str = "skill.inspect";
pub const SKILL_STATUS_TOOL_ID: &str = "skill.status";
pub const SKILL_VERIFY_TOOL_ID: &str = "skill.verify";
pub const SKILL_LOCK_TOOL_ID: &str = "skill.lock";
pub const SKILL_TRUST_TOOL_ID: &str = "skill.trust";
pub const SKILL_REVOKE_TOOL_ID: &str = "skill.revoke";

pub fn declaration() -> ToolProviderDeclaration {
    ToolProviderDeclaration::new(
        ToolProviderId::new(PROVIDER_ID).expect("builtin skill provider id is valid"),
        "Skill Host Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin skill provider declaration is valid")
    .with_tool(tool(
        SKILL_LIST_TOOL_ID,
        "List builtin and workspace skills with trust and routing summaries.",
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
        &[],
        true,
    ))
    .with_tool(tool(
        SKILL_INSPECT_TOOL_ID,
        "Inspect one skill manifest, trust state, hashes, and bounded optional body text.",
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
        &["id"],
        true,
    ))
    .with_tool(tool(
        SKILL_STATUS_TOOL_ID,
        "Report workspace skill component, lock, trust, and health status.",
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
        &[],
        true,
    ))
    .with_tool(tool(
        SKILL_VERIFY_TOOL_ID,
        "Verify workspace skills and lock state without writing trust.",
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
        &[],
        true,
    ))
    .with_tool(tool(
        SKILL_LOCK_TOOL_ID,
        "Write or preview .agl/skills.lock for workspace skills.",
        ToolCapability::Write,
        ToolOperationKind::Admin,
        &[ToolStateEffect::RepoWorkspace],
        &[],
        false,
    ))
    .with_tool(tool(
        SKILL_TRUST_TOOL_ID,
        "Approve local trust for one exact locked workspace skill identity.",
        ToolCapability::Write,
        ToolOperationKind::Approve,
        &[ToolStateEffect::SkillTrust],
        &["name"],
        false,
    ))
    .with_tool(tool(
        SKILL_REVOKE_TOOL_ID,
        "Revoke local trust for one exact workspace skill identity.",
        ToolCapability::Write,
        ToolOperationKind::Approve,
        &[ToolStateEffect::SkillTrust],
        &["name"],
        false,
    ))
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn tool(
    id: &str,
    description: &str,
    capability: ToolCapability,
    operation_kind: ToolOperationKind,
    state_effects: &[ToolStateEffect],
    required_arguments: &[&str],
    visible_in_read_only: bool,
) -> ToolDeclaration {
    ToolDeclaration::new(
        ToolId::new(id).expect("builtin skill tool id is valid"),
        description,
        capability,
        required_arguments.iter().copied(),
    )
    .with_operation_kind(operation_kind)
    .with_state_effects(state_effects.iter().copied())
    .visible_in_read_only(visible_in_read_only)
}
