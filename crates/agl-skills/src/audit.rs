use std::collections::{BTreeMap, BTreeSet};

use agl_tools::{
    HookId, ToolCapability, ToolCatalog, ToolId, ToolOperationKind, ToolProviderId, ToolStateEffect,
};
use serde::Deserialize;

use crate::SkillRegistry;

const TOOL_LENS_AUDIT: &str = include_str!("../../../assets/audits/tool-lens.toml");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolLensAudit {
    version: u32,
    tools: Vec<AuditTool>,
    missing_tools: Vec<MissingTool>,
    skills: Vec<AuditSkill>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditTool {
    id: ToolId,
    provider: ToolProviderId,
    capability: ToolCapability,
    operation_kind: ToolOperationKind,
    state_effects: Vec<ToolStateEffect>,
    visible_in_read_only: bool,
    policy_owner: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MissingTool {
    id: ToolId,
    domain: String,
    operation_kind: ToolOperationKind,
    target_task: String,
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditSkill {
    id: agl_tools::SkillId,
    pack: String,
    classification: String,
    allowed_tools: Vec<ToolId>,
    required_hooks: Vec<HookId>,
    state_effects: Vec<ToolStateEffect>,
    memory_read_scopes: Vec<String>,
    notes_read: bool,
    notes_write: bool,
    risks: Vec<String>,
}

#[test]
fn tool_lens_audit_fixture_covers_builtin_tools() {
    let audit = read_audit();
    let catalog = builtin_tool_catalog();
    let audit_tools = audit
        .tools
        .iter()
        .map(|tool| (tool.id.clone(), tool))
        .collect::<BTreeMap<_, _>>();
    let actual_tools = catalog
        .providers()
        .iter()
        .flat_map(|provider| provider.tools.iter().map(|tool| tool.id.clone()))
        .collect::<BTreeSet<_>>();

    assert_eq!(audit.version, 1);
    assert_eq!(
        audit_tools.keys().cloned().collect::<BTreeSet<_>>(),
        actual_tools
    );

    for provider in catalog.providers() {
        for tool in &provider.tools {
            let expected = audit_tools.get(&tool.id).unwrap();
            assert_eq!(expected.provider, provider.id, "{}", tool.id);
            assert_eq!(expected.capability, tool.capability, "{}", tool.id);
            assert_eq!(expected.operation_kind, tool.operation_kind, "{}", tool.id);
            assert_eq!(expected.state_effects, tool.state_effects, "{}", tool.id);
            assert_eq!(
                expected.visible_in_read_only, tool.visible_in_read_only,
                "{}",
                tool.id
            );
            assert!(
                !expected.policy_owner.trim().is_empty(),
                "{} must name a policy owner",
                tool.id
            );
        }
    }
}

#[test]
fn tool_lens_audit_fixture_covers_builtin_skills() {
    let audit = read_audit();
    let catalog = builtin_tool_catalog();
    let registry = SkillRegistry::from_builtin_assets().unwrap();
    let audit_skills = audit
        .skills
        .iter()
        .map(|skill| (skill.id.clone(), skill))
        .collect::<BTreeMap<_, _>>();
    let actual_skills = registry
        .skills()
        .iter()
        .map(|skill| skill.harness.id.clone())
        .collect::<BTreeSet<_>>();

    assert_eq!(
        audit_skills.keys().cloned().collect::<BTreeSet<_>>(),
        actual_skills
    );

    for registered in registry.skills() {
        let skill = &registered.harness;
        let expected = audit_skills.get(&skill.id).unwrap();
        assert_eq!(expected.pack, skill.pack, "{}", skill.id);
        assert_eq!(
            expected
                .allowed_tools
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            skill.allowed_tools.iter().cloned().collect::<BTreeSet<_>>(),
            "{}",
            skill.id
        );
        assert_eq!(
            expected
                .required_hooks
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            skill
                .required_hooks
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            "{}",
            skill.id
        );
        assert_eq!(
            expected.state_effects,
            state_effects_for_skill(skill.allowed_tools.iter(), &catalog),
            "{}",
            skill.id
        );
        assert_eq!(
            expected.memory_read_scopes,
            skill
                .permissions
                .memory_read_scopes()
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>(),
            "{}",
            skill.id
        );
        assert_eq!(
            expected.notes_read, skill.permissions.notes.read,
            "{}",
            skill.id
        );
        assert_eq!(
            expected.notes_write, skill.permissions.notes.write,
            "{}",
            skill.id
        );
        assert!(
            !expected.classification.trim().is_empty(),
            "{} must be classified",
            skill.id
        );
        if !expected.state_effects.is_empty() {
            assert!(
                !expected.risks.is_empty(),
                "{} mutates state and must record audit risks",
                skill.id
            );
        }
    }
}

#[test]
fn tool_lens_missing_tools_are_future_work_not_current_tools() {
    let audit = read_audit();
    let catalog = builtin_tool_catalog();
    let current_tools = catalog
        .providers()
        .iter()
        .flat_map(|provider| provider.tools.iter().map(|tool| tool.id.clone()))
        .collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();

    for missing in audit.missing_tools {
        assert!(
            seen.insert(missing.id.clone()),
            "duplicate missing tool {}",
            missing.id
        );
        assert!(
            !current_tools.contains(&missing.id),
            "missing tool {} already exists",
            missing.id
        );
        assert!(
            !missing.domain.trim().is_empty(),
            "{} must name a domain",
            missing.id
        );
        assert!(
            !missing.target_task.trim().is_empty(),
            "{} must name a target task",
            missing.id
        );
        assert!(
            !missing.reason.trim().is_empty(),
            "{} must explain why it is missing",
            missing.id
        );
        assert!(
            !matches!(missing.operation_kind, ToolOperationKind::Read)
                || missing.id.as_str().contains(".status")
                || missing.id.as_str().contains(".list")
                || missing.id.as_str().contains(".show")
                || missing.id.as_str().contains(".search")
                || missing.id.as_str().contains(".history")
                || missing.id.as_str().contains(".inspect")
                || missing.id.as_str().contains(".export"),
            "{} read missing tools should be status/list/show/search/history/inspect/export surfaces",
            missing.id
        );
    }
}

fn read_audit() -> ToolLensAudit {
    toml::from_str(TOOL_LENS_AUDIT).expect("tool lens audit fixture must parse")
}

fn builtin_tool_catalog() -> ToolCatalog {
    let mut catalog = ToolCatalog::new();
    agl_tools::guards::register(&mut catalog).unwrap();
    agl_tools::fs::register(&mut catalog).unwrap();
    agl_tools::memory::register(&mut catalog).unwrap();
    agl_tools::notes::register(&mut catalog).unwrap();
    agl_tools::permissions::register(&mut catalog).unwrap();
    catalog
}

fn state_effects_for_skill<'a>(
    tools: impl IntoIterator<Item = &'a ToolId>,
    catalog: &ToolCatalog,
) -> Vec<ToolStateEffect> {
    tools
        .into_iter()
        .flat_map(|tool_id| {
            catalog
                .tool(tool_id)
                .unwrap_or_else(|| panic!("missing audited tool {tool_id}"))
                .state_effects
                .iter()
                .copied()
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
