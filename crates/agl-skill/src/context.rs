use std::collections::{BTreeMap, BTreeSet};

use agl_kernel::ToolCatalog;
use agl_kernel::ToolExclusionReason;
use agl_kernel::{EffectId, HookId, SkillId, ToolId};
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

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct SkillToolRoutingView {
    routes: BTreeMap<SkillId, SkillToolRouting>,
}

impl SkillToolRoutingView {
    pub fn new(
        routes: impl IntoIterator<Item = (SkillId, SkillToolRouting)>,
    ) -> Result<Self, SkillContextError> {
        let mut indexed = BTreeMap::new();
        for (skill_id, routing) in routes {
            if indexed.insert(skill_id.clone(), routing).is_some() {
                return Err(SkillContextError::InvalidRouting {
                    skill_id: skill_id.as_str().to_string(),
                    message: "duplicate skill routing entry",
                });
            }
        }
        Ok(Self { routes: indexed })
    }

    pub fn route(&self, skill_id: &SkillId) -> Option<&SkillToolRouting> {
        self.routes.get(skill_id)
    }

    pub fn routes(&self) -> impl ExactSizeIterator<Item = (&SkillId, &SkillToolRouting)> {
        self.routes.iter()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct SkillToolRouting {
    callable_tools: BTreeSet<ToolId>,
    requestable_tools: BTreeSet<ToolId>,
    unavailable_tools: BTreeMap<ToolId, ToolExclusionReason>,
}

impl SkillToolRouting {
    pub fn new(
        callable_tools: impl IntoIterator<Item = ToolId>,
        requestable_tools: impl IntoIterator<Item = ToolId>,
        unavailable_tools: impl IntoIterator<Item = (ToolId, ToolExclusionReason)>,
    ) -> Self {
        Self {
            callable_tools: callable_tools.into_iter().collect(),
            requestable_tools: requestable_tools.into_iter().collect(),
            unavailable_tools: unavailable_tools.into_iter().collect(),
        }
    }

    pub fn callable_tools(&self) -> &BTreeSet<ToolId> {
        &self.callable_tools
    }

    pub fn requestable_tools(&self) -> &BTreeSet<ToolId> {
        &self.requestable_tools
    }

    pub fn unavailable_tools(&self) -> &BTreeMap<ToolId, ToolExclusionReason> {
        &self.unavailable_tools
    }

    pub fn declared_tools(&self) -> BTreeSet<ToolId> {
        self.callable_tools
            .iter()
            .chain(&self.requestable_tools)
            .chain(self.unavailable_tools.keys())
            .cloned()
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkillContextEvidence {
    pub skill_id: String,
    pub source: String,
    pub pack: String,
    pub manifest_sha256: String,
    pub tree_sha256: String,
    pub required_hooks: Vec<String>,
    pub manifest_allowed_tools: Vec<String>,
    pub manifest_requestable_tools: Vec<String>,
    pub manifest_denied_tools: Vec<String>,
    pub callable_tools: Vec<String>,
    pub requestable_tools: Vec<String>,
    pub unavailable_tools: Vec<SkillUnavailableToolEvidence>,
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
pub struct SkillUnavailableToolEvidence {
    pub tool_id: String,
    pub reason: String,
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
    InvalidRouting {
        skill_id: String,
        message: &'static str,
    },
}

impl std::fmt::Display for SkillContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Registry(err) => write!(f, "{err}"),
            Self::InvalidRouting { skill_id, message } => {
                write!(f, "invalid tool routing for skill `{skill_id}`: {message}")
            }
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
    routing: &SkillToolRoutingView,
) -> Result<SkillContextBundle, SkillContextError> {
    let selected = selections.iter().cloned().collect::<BTreeSet<_>>();
    let routed = routing
        .routes()
        .map(|(skill_id, _)| skill_id.clone())
        .collect::<BTreeSet<_>>();
    if selected != routed {
        return Err(SkillContextError::InvalidRouting {
            skill_id: "<selection>".to_string(),
            message: "routing skill IDs do not match selected skill IDs",
        });
    }
    let mut blocks = Vec::with_capacity(selections.len());
    for skill_id in selections {
        registry.verify_required_hooks(skill_id, tool_catalog)?;
        let skill = registry.resolve_for_context_injection(skill_id)?;
        let route = routing
            .route(skill_id)
            .ok_or_else(|| SkillContextError::InvalidRouting {
                skill_id: skill_id.as_str().to_string(),
                message: "selected skill has no routing entry",
            })?;
        blocks.push(build_context_block(skill, route)?);
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

fn build_context_block(
    skill: &crate::RegisteredSkill,
    routing: &SkillToolRouting,
) -> Result<SkillContextBlock, SkillContextError> {
    let harness = &skill.harness;
    validate_routing(harness, routing)?;
    let mut content = String::new();
    content.push_str("<agentlibre_skill_context>\n");
    content.push_str(&format!("skill_id: {}\n", harness.id));
    content.push_str(&format!("source: {}\n", harness.source.as_str()));
    content.push_str(&format!("pack: {}\n", harness.pack));
    content.push_str("\n## Tool Routing\n\n");
    content.push_str("directly_callable_tools: ");
    content.push_str(&render_tools(routing.callable_tools.iter()));
    content.push('\n');
    content.push_str("requestable_tools: ");
    content.push_str(&render_tools(routing.requestable_tools.iter()));
    content.push('\n');
    content.push_str("unavailable_tools: ");
    content.push_str(&render_unavailable_tools(&routing.unavailable_tools));
    content.push('\n');
    let request_templates = harness
        .permission_request_templates
        .iter()
        .filter(|template| {
            template
                .tools
                .iter()
                .all(|tool| routing.requestable_tools.contains(tool))
        })
        .collect::<Vec<_>>();
    if !request_templates.is_empty() {
        content.push_str("permission_request_templates:\n");
        for template in &request_templates {
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
        manifest_allowed_tools: harness
            .allowed_tools
            .iter()
            .map(ToolId::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        manifest_requestable_tools: harness
            .requestable_tools
            .iter()
            .map(ToolId::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        manifest_denied_tools: harness
            .denied_tools
            .iter()
            .map(ToolId::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        callable_tools: routing
            .callable_tools
            .iter()
            .map(ToolId::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        requestable_tools: routing
            .requestable_tools
            .iter()
            .map(ToolId::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        unavailable_tools: routing
            .unavailable_tools
            .iter()
            .map(|(tool_id, reason)| SkillUnavailableToolEvidence {
                tool_id: tool_id.as_str().to_string(),
                reason: reason.code().to_string(),
            })
            .collect(),
        permission_request_templates: request_templates
            .into_iter()
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
                    .map(EffectId::as_str)
                    .map(ToOwned::to_owned)
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

    Ok(SkillContextBlock { content, evidence })
}

fn validate_routing(
    harness: &crate::SkillHarness,
    routing: &SkillToolRouting,
) -> Result<(), SkillContextError> {
    let declared_allowed = harness
        .allowed_tools
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let declared_requestable = harness
        .requestable_tools
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let declared_denied = harness
        .denied_tools
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let declared = declared_allowed
        .union(&declared_requestable)
        .cloned()
        .chain(declared_denied.iter().cloned())
        .collect::<BTreeSet<_>>();
    let invalid = |message| SkillContextError::InvalidRouting {
        skill_id: harness.id.as_str().to_string(),
        message,
    };
    if routing.declared_tools() != declared {
        return Err(invalid(
            "routing does not partition every declared tool exactly once",
        ));
    }
    if !routing
        .callable_tools
        .is_disjoint(&routing.requestable_tools)
        || routing
            .callable_tools
            .iter()
            .any(|tool| routing.unavailable_tools.contains_key(tool))
        || routing
            .requestable_tools
            .iter()
            .any(|tool| routing.unavailable_tools.contains_key(tool))
    {
        return Err(invalid(
            "callable, requestable, and unavailable sets overlap",
        ));
    }
    let callable_candidates = declared_allowed
        .union(&declared_requestable)
        .cloned()
        .collect::<BTreeSet<_>>();
    if !routing.callable_tools.is_subset(&callable_candidates) {
        return Err(invalid("callable set contains a denied or undeclared tool"));
    }
    if !routing.requestable_tools.is_subset(&declared_requestable) {
        return Err(invalid(
            "requestable set contains a tool not declared requestable",
        ));
    }
    if !declared_denied
        .iter()
        .all(|tool| routing.unavailable_tools.contains_key(tool))
    {
        return Err(invalid("a manifest-denied tool is not unavailable"));
    }
    Ok(())
}

fn render_tools<'a>(tools: impl IntoIterator<Item = &'a ToolId>) -> String {
    let tools = tools.into_iter().map(ToolId::as_str).collect::<Vec<_>>();
    if tools.is_empty() {
        "[]".to_string()
    } else {
        tools.join(", ")
    }
}

fn render_unavailable_tools(tools: &BTreeMap<ToolId, ToolExclusionReason>) -> String {
    if tools.is_empty() {
        "[]".to_string()
    } else {
        tools
            .iter()
            .map(|(tool, reason)| format!("{} [{}]", tool.as_str(), reason.code()))
            .collect::<Vec<_>>()
            .join(", ")
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
    use agl_kernel::OperationKind;
    use agl_kernel::ToolCatalog;

    use super::*;
    use crate::{
        RegisteredSkill, SkillHarness, SkillPermissionRequestTemplate, SkillPermissions,
        SkillReferencePolicy, SkillSource,
    };

    #[test]
    fn verified_context_bundle_records_hashes_for_minimal_builtin_skill() {
        let registry = SkillRegistry::from_builtin_assets().unwrap();
        let mut tool_catalog = ToolCatalog::new();
        tool_catalog
            .register(agl_core_tools::guards::declaration())
            .unwrap();
        tool_catalog
            .register(agl_core_tools::fs::declaration())
            .unwrap();
        tool_catalog
            .register(agl_core_tools::repo::declaration())
            .unwrap();

        let skill_id = SkillId::new("repo-status").unwrap();
        let routing = SkillToolRoutingView::new([(
            skill_id.clone(),
            SkillToolRouting::new(
                tool_ids([
                    "core.workspace:fs.list",
                    "core.workspace:fs.read",
                    "core.workspace:fs.search",
                ]),
                [],
                [],
            ),
        )])
        .unwrap();
        let bundle =
            build_verified_context_bundle(&registry, &tool_catalog, &[skill_id], &routing).unwrap();

        assert!(bundle.content.contains("Use this skill"));
        assert!(bundle.content.contains("repository state picture"));
        assert_eq!(bundle.evidence.len(), 1);
        assert_eq!(bundle.evidence[0].skill_id, "repo-status");
        assert_eq!(bundle.evidence[0].source, "core");
        assert_eq!(
            bundle.evidence[0].required_hooks,
            vec!["core:repo_path.validate", "core:verification.validate"]
        );
        assert_eq!(
            bundle.evidence[0].callable_tools,
            vec![
                "core.workspace:fs.list",
                "core.workspace:fs.read",
                "core.workspace:fs.search"
            ]
        );
        assert!(
            bundle
                .content
                .contains("directly_callable_tools: core.workspace:fs.list, core.workspace:fs.read, core.workspace:fs.search")
        );
        assert!(bundle.content.contains("requestable_tools: []"));
        assert!(
            bundle
                .content
                .contains("Requestable tools are not callable unless they also appear")
        );
        assert!(bundle.evidence[0].requestable_tools.is_empty());
        assert!(bundle.evidence[0].unavailable_tools.is_empty());
        assert!(bundle.evidence[0].permission_request_templates.is_empty());
        assert!(bundle.evidence[0].included_references.is_empty());
        assert!(!format!("{:?}", bundle.evidence).contains("repository state picture"));
    }

    #[test]
    fn context_distinguishes_callable_from_requestable_tools() {
        let registry = registry_with_requestable_fixture();
        let mut tool_catalog = ToolCatalog::new();
        tool_catalog
            .register(agl_core_tools::guards::declaration())
            .unwrap();
        tool_catalog
            .register(agl_core_tools::cron::declaration())
            .unwrap();
        tool_catalog
            .register(agl_core_tools::fs::declaration())
            .unwrap();
        tool_catalog
            .register(agl_core_tools::matrix::declaration())
            .unwrap();
        tool_catalog
            .register(agl_core_tools::permissions::declaration())
            .unwrap();

        let skill_id = SkillId::new("requestable-test").unwrap();
        let routing = SkillToolRoutingView::new([(
            skill_id.clone(),
            SkillToolRouting::new(
                tool_ids([
                    "core.cron:preflight",
                    "core.workspace:fs.read",
                    "core.workspace:fs.search",
                    "core.permission:request",
                    "core.permission:status",
                ]),
                tool_ids(["core.cron:add", "matrix.outbox:enqueue"]),
                [(
                    ToolId::new("matrix.bridge:outbox.deliver").unwrap(),
                    ToolExclusionReason::SkillDenied,
                )],
            ),
        )])
        .unwrap();
        let bundle =
            build_verified_context_bundle(&registry, &tool_catalog, &[skill_id], &routing).unwrap();

        assert!(
            bundle
                .content
                .contains("directly_callable_tools: core.cron:preflight, core.permission:request, core.permission:status, core.workspace:fs.read, core.workspace:fs.search")
        );
        assert!(
            bundle
                .content
                .contains("requestable_tools: core.cron:add, matrix.outbox:enqueue")
        );
        assert!(
            bundle
                .content
                .contains("unavailable_tools: matrix.bridge:outbox.deliver [skill_denied]")
        );
        assert!(bundle.content.contains("id: schedule-matrix-cron"));
        assert_eq!(
            bundle.evidence[0].callable_tools,
            vec![
                "core.cron:preflight",
                "core.permission:request",
                "core.permission:status",
                "core.workspace:fs.read",
                "core.workspace:fs.search"
            ]
        );
        assert_eq!(
            bundle.evidence[0].requestable_tools,
            vec!["core.cron:add", "matrix.outbox:enqueue"]
        );
        assert_eq!(
            bundle.evidence[0].unavailable_tools,
            vec![SkillUnavailableToolEvidence {
                tool_id: "matrix.bridge:outbox.deliver".to_string(),
                reason: "skill_denied".to_string(),
            }]
        );
        assert_eq!(
            bundle.evidence[0].permission_request_templates[0].tools,
            vec!["core.cron:add", "matrix.outbox:enqueue"]
        );
    }

    fn registry_with_requestable_fixture() -> SkillRegistry {
        let mut registry = SkillRegistry::new();
        registry
            .register(RegisteredSkill::trusted_builtin(SkillHarness {
                package: test_package("requestable-test"),
                id: SkillId::new("requestable-test").unwrap(),
                name: "requestable-test".to_string(),
                description: "Test-only requestable tool routing skill.".to_string(),
                version: agl_package::PackageVersion::new("1.0.0").unwrap(),
                source: SkillSource::Core,
                pack: "test".to_string(),
                required_hooks: vec![HookId::new("core:repo_path.validate").unwrap()],
                allowed_tools: tool_ids([
                    "core.cron:preflight",
                    "core.workspace:fs.read",
                    "core.workspace:fs.search",
                    "core.permission:request",
                    "core.permission:status",
                ]),
                requestable_tools: tool_ids(["core.cron:add", "matrix.outbox:enqueue"]),
                denied_tools: tool_ids(["matrix.bridge:outbox.deliver"]),
                permission_request_templates: vec![SkillPermissionRequestTemplate {
                    id: "schedule-matrix-cron".to_string(),
                    tools: tool_ids(["core.cron:add", "matrix.outbox:enqueue"]),
                    max_operation_kind: Some(OperationKind::Write),
                    state_effects: vec![EffectId::store_cron(), EffectId::matrix_outbox()],
                    default_duration: "one_turn".to_string(),
                    reason_template: "Schedule a Matrix notification cron job.".to_string(),
                }],
                permissions: SkillPermissions::default(),
                context_budget_tokens: 512,
                reference_policy: SkillReferencePolicy {
                    include: Vec::new(),
                },
                references: Vec::new(),
                guarantees: vec!["test fixture is trusted by construction".to_string()],
                body: "Use this skill to test requestable tool context rendering.".to_string(),
                source_path: "test/requestable-test/SKILL.md".to_string(),
                manifest_sha256: "0".repeat(64),
                tree_sha256: "1".repeat(64),
            }))
            .unwrap();
        registry
    }

    fn test_package(id: &str) -> agl_package::PackageEnvelope {
        agl_package::PackageEnvelope::new(
            agl_package::PackageTypeId::skill(),
            agl_package::PackageId::new(id).unwrap(),
            agl_package::PackageVersion::new("1.0.0").unwrap(),
            agl_package::PackageSchemaId::new("agentlibre.skill/v2").unwrap(),
            agl_package::AglCompatibility::new(
                agl_package::PackageVersionReq::new(">=1.0.0-alpha.12").unwrap(),
                [agl_package::PackageVersion::new("1.0.0-alpha.12").unwrap()],
            )
            .unwrap(),
            Vec::new(),
        )
        .unwrap()
    }

    fn tool_ids<const N: usize>(values: [&str; N]) -> Vec<ToolId> {
        values
            .into_iter()
            .map(|value| ToolId::new(value).unwrap())
            .collect()
    }
}
