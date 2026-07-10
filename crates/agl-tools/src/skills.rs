use agl_capabilities::{
    ActionDeclaration, CapabilityId, OperationKind, ProviderDeclaration, ProviderId, StateEffect,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{ToolCatalog, ToolCatalogError};

pub const PROVIDER_ID: &str = "skill-tools";
pub const SKILL_LIST_TOOL_ID: &str = "skill.list";
pub const SKILL_INSPECT_TOOL_ID: &str = "skill.inspect";
pub const SKILL_STATUS_TOOL_ID: &str = "skill.status";
pub const SKILL_VERIFY_TOOL_ID: &str = "skill.verify";
pub const SKILL_LOCK_TOOL_ID: &str = "skill.lock";
pub const SKILL_TRUST_TOOL_ID: &str = "skill.trust";
pub const SKILL_REVOKE_TOOL_ID: &str = "skill.revoke";

#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillListSource {
    #[default]
    All,
    Workspace,
    Core,
    Community,
    Local,
}

impl SkillListSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Workspace => "workspace",
            Self::Core => "core",
            Self::Community => "community",
            Self::Local => "local",
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillListArgs {
    #[serde(default)]
    pub source: SkillListSource,
    #[serde(default)]
    pub trusted_only: bool,
    #[serde(default)]
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillInspectArgs {
    #[schemars(length(min = 1))]
    pub id: String,
    #[serde(default)]
    pub include_body: bool,
    #[serde(default)]
    pub include_references: bool,
    #[serde(default)]
    #[schemars(range(min = 1, max = 16384))]
    pub max_bytes: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillStatusArgs {}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillVerifyArgs {}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillLockArgs {
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillTrustArgs {
    #[schemars(length(min = 1))]
    pub name: String,
    #[serde(default = "default_true")]
    pub approve: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillRevokeArgs {
    #[schemars(length(min = 1))]
    pub name: String,
}

fn default_true() -> bool {
    true
}

pub fn declaration() -> ProviderDeclaration {
    ProviderDeclaration::builtin(
        ProviderId::new(PROVIDER_ID).expect("builtin skill provider ID is valid"),
        "Skill Host Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin skill provider declaration is valid")
    .with_action(action::<SkillListArgs>(
        SKILL_LIST_TOOL_ID,
        "List core and workspace skills with trust and routing summaries.",
        OperationKind::Read,
        [],
    ))
    .with_action(action::<SkillInspectArgs>(
        SKILL_INSPECT_TOOL_ID,
        "Inspect one skill manifest, trust state, hashes, and bounded optional body text.",
        OperationKind::Read,
        [],
    ))
    .with_action(action::<SkillStatusArgs>(
        SKILL_STATUS_TOOL_ID,
        "Report workspace skill component, lock, trust, and health status.",
        OperationKind::Read,
        [],
    ))
    .with_action(action::<SkillVerifyArgs>(
        SKILL_VERIFY_TOOL_ID,
        "Verify workspace skills and lock state without writing trust.",
        OperationKind::Read,
        [],
    ))
    .with_action(action::<SkillLockArgs>(
        SKILL_LOCK_TOOL_ID,
        "Write or preview .agl/skills.lock for workspace skills.",
        OperationKind::Admin,
        [StateEffect::RepoWorkspace],
    ))
    .with_action(action::<SkillTrustArgs>(
        SKILL_TRUST_TOOL_ID,
        "Approve local trust for one exact locked workspace skill identity.",
        OperationKind::Approve,
        [StateEffect::SkillTrust],
    ))
    .with_action(action::<SkillRevokeArgs>(
        SKILL_REVOKE_TOOL_ID,
        "Revoke local trust for one exact workspace skill identity.",
        OperationKind::Approve,
        [StateEffect::SkillTrust],
    ))
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn action<T: JsonSchema>(
    id: &str,
    description: &str,
    operation_kind: OperationKind,
    state_effects: impl IntoIterator<Item = StateEffect>,
) -> ActionDeclaration {
    ActionDeclaration::from_schema::<T>(
        CapabilityId::new(id).expect("builtin skill capability ID is valid"),
        description,
        operation_kind,
    )
    .expect("builtin skill action schema is valid")
    .with_state_effects(state_effects)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn all_skill_action_schemas_compile_and_reject_unknown_fields() {
        let declaration = declaration();
        declaration.validate().unwrap();
        assert_eq!(declaration.actions.len(), 7);
        for action in &declaration.actions {
            action.compile_schema().unwrap();
            assert_eq!(action.input_schema["additionalProperties"], false);
        }

        let list = declaration
            .action(&CapabilityId::new(SKILL_LIST_TOOL_ID).unwrap())
            .unwrap()
            .compile_schema()
            .unwrap();
        list.validate(&json!({"source": "core", "limit": 10}))
            .unwrap();
        assert!(list.validate(&json!({"source": "invalid"})).is_err());
        assert!(list.validate(&json!({"unknown": true})).is_err());
    }

    #[test]
    fn mutating_skill_actions_declare_precise_effects() {
        let declaration = declaration();
        let effects = |id| {
            declaration
                .action(&CapabilityId::new(id).unwrap())
                .unwrap()
                .state_effects
                .clone()
        };
        assert_eq!(
            effects(SKILL_LOCK_TOOL_ID),
            [StateEffect::RepoWorkspace].into_iter().collect()
        );
        assert_eq!(
            effects(SKILL_TRUST_TOOL_ID),
            [StateEffect::SkillTrust].into_iter().collect()
        );
        assert_eq!(
            effects(SKILL_REVOKE_TOOL_ID),
            [StateEffect::SkillTrust].into_iter().collect()
        );
    }
}
