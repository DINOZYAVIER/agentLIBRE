use std::collections::{BTreeMap, BTreeSet};

use agl_capabilities::{CapabilityId, OperationKind, SkillId, StateEffect};
use agl_tools::ToolCatalog;
use serde::Deserialize;

use crate::SkillRegistry;

const TOOL_LENS_AUDIT: &str = include_str!("../../../assets/audits/tool-lens.toml");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolLensAudit {
    version: u32,
    tool_policy_owners: BTreeMap<CapabilityId, String>,
    #[serde(default)]
    missing_tools: Vec<MissingTool>,
    skill_classifications: BTreeMap<SkillId, String>,
    #[serde(default)]
    skill_risks: BTreeMap<SkillId, Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MissingTool {
    id: CapabilityId,
    domain: String,
    operation_kind: OperationKind,
    target_task: String,
    reason: String,
}

#[test]
fn tool_lens_audit_fixture_covers_builtin_tools() {
    let audit = read_audit();
    let catalog = agl_tools::builtin_tool_catalog().unwrap();
    let actual_tools = catalog
        .providers()
        .iter()
        .flat_map(|provider| provider.actions.iter().map(|action| action.id.clone()))
        .collect::<BTreeSet<_>>();

    assert_eq!(audit.version, 1);
    assert_eq!(
        audit
            .tool_policy_owners
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>(),
        actual_tools
    );

    for provider in catalog.providers() {
        for action in &provider.actions {
            let policy_owner = audit.tool_policy_owners.get(&action.id).unwrap();
            assert!(
                !policy_owner.trim().is_empty(),
                "{} must name a policy owner",
                action.id
            );
            assert!(
                !provider.id.as_str().trim().is_empty(),
                "{} must come from a named provider",
                action.id
            );
        }
    }
}

#[test]
fn tool_lens_audit_fixture_covers_builtin_skills() {
    let audit = read_audit();
    let catalog = agl_tools::builtin_tool_catalog().unwrap();
    let registry = SkillRegistry::from_builtin_assets().unwrap();
    let actual_skills = registry
        .skills()
        .iter()
        .map(|skill| skill.harness.id.clone())
        .collect::<BTreeSet<_>>();

    assert_eq!(
        audit
            .skill_classifications
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>(),
        actual_skills
    );
    assert!(
        audit
            .skill_risks
            .keys()
            .all(|skill| audit.skill_classifications.contains_key(skill)),
        "skill risks must only reference audited skills"
    );

    for registered in registry.skills() {
        let skill = &registered.harness;
        for tool in skill
            .allowed_tools
            .iter()
            .chain(skill.requestable_tools.iter())
            .chain(skill.denied_tools.iter())
            .chain(
                skill
                    .permission_request_templates
                    .iter()
                    .flat_map(|template| template.tools.iter()),
            )
        {
            catalog
                .action(tool)
                .unwrap_or_else(|| panic!("{} references unknown tool {tool}", skill.id));
        }
        let classification = audit.skill_classifications.get(&skill.id).unwrap();
        assert!(
            !classification.trim().is_empty(),
            "{} must be classified",
            skill.id
        );
        let state_effects = state_effects_for_skill(skill.allowed_tools.iter(), &catalog);
        let risks = audit
            .skill_risks
            .get(&skill.id)
            .cloned()
            .unwrap_or_default();
        for risk in &risks {
            assert!(!risk.trim().is_empty(), "{} has a blank risk", skill.id);
        }
        if !state_effects.is_empty() {
            assert!(
                !risks.is_empty(),
                "{} mutates state and must record audit risks",
                skill.id
            );
        }
    }
}

#[test]
fn tool_lens_missing_tools_are_future_work_not_current_tools() {
    let audit = read_audit();
    let catalog = agl_tools::builtin_tool_catalog().unwrap();
    let current_tools = catalog
        .providers()
        .iter()
        .flat_map(|provider| provider.actions.iter().map(|action| action.id.clone()))
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
            !matches!(missing.operation_kind, OperationKind::Read)
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

fn state_effects_for_skill<'a>(
    tools: impl IntoIterator<Item = &'a CapabilityId>,
    catalog: &ToolCatalog,
) -> Vec<StateEffect> {
    tools
        .into_iter()
        .flat_map(|tool_id| {
            catalog
                .action(tool_id)
                .unwrap_or_else(|| panic!("missing audited tool {tool_id}"))
                .state_effects
                .iter()
                .copied()
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
