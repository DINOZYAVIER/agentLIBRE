use std::collections::BTreeMap;

use agl_tools::{HookId, SkillId, ToolCatalog, ToolCatalogError, ToolId};

use crate::manifest::{SkillHarness, SkillManifestError, SkillSource};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillTrustState {
    TrustedByBinary,
    Unsupported,
    Unknown,
    Changed,
    RemoteMismatch,
    RevMismatch,
    DirtyWorkingTree,
    UntrackedContent,
    Revoked,
    TrustedLocal,
    Invalid,
}

impl SkillTrustState {
    pub fn permits_context_injection(self) -> bool {
        matches!(self, Self::TrustedByBinary | Self::TrustedLocal)
    }
}

impl SkillSource {
    pub fn default_trust_state(self) -> SkillTrustState {
        match self {
            Self::Builtin => SkillTrustState::TrustedByBinary,
            Self::Workspace => SkillTrustState::Unknown,
            Self::User | Self::ThirdParty => SkillTrustState::Unsupported,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisteredSkill {
    pub harness: SkillHarness,
    pub trust: SkillTrustState,
}

impl RegisteredSkill {
    pub fn trusted_builtin(harness: SkillHarness) -> Self {
        Self {
            harness,
            trust: SkillTrustState::TrustedByBinary,
        }
    }

    pub fn permits_context_injection(&self) -> bool {
        matches!(
            (self.trust, self.harness.source),
            (SkillTrustState::TrustedByBinary, SkillSource::Builtin)
                | (SkillTrustState::TrustedLocal, SkillSource::Workspace)
        )
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SkillRegistry {
    skills: Vec<RegisteredSkill>,
    skill_index: BTreeMap<SkillId, usize>,
    pack_index: BTreeMap<String, Vec<usize>>,
    hook_index: BTreeMap<HookId, Vec<usize>>,
    tool_index: BTreeMap<ToolId, Vec<usize>>,
}

pub fn builtin_registry() -> Result<SkillRegistry, SkillRegistryError> {
    SkillRegistry::from_builtin_assets()
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_builtin_assets() -> Result<Self, SkillRegistryError> {
        let mut registry = Self::new();
        for skill in agl_assets::BUILTIN_SKILLS {
            let harness =
                SkillHarness::parse_builtin(skill).map_err(SkillRegistryError::Manifest)?;
            registry.register(RegisteredSkill::trusted_builtin(harness))?;
        }
        Ok(registry)
    }

    pub fn register(&mut self, skill: RegisteredSkill) -> Result<(), SkillRegistryError> {
        let skill_id = skill.harness.id.clone();
        if self.skill_index.contains_key(&skill_id) {
            return Err(SkillRegistryError::DuplicateSkill {
                id: skill_id.as_str().to_string(),
            });
        }

        let index = self.skills.len();
        self.pack_index
            .entry(skill.harness.pack.clone())
            .or_default()
            .push(index);
        for hook in &skill.harness.required_hooks {
            self.hook_index.entry(hook.clone()).or_default().push(index);
        }
        for tool in &skill.harness.allowed_tools {
            self.tool_index.entry(tool.clone()).or_default().push(index);
        }
        self.skill_index.insert(skill_id, index);
        self.skills.push(skill);
        Ok(())
    }

    pub fn skills(&self) -> &[RegisteredSkill] {
        &self.skills
    }

    pub fn get(&self, id: &SkillId) -> Option<&RegisteredSkill> {
        self.skill_index.get(id).map(|index| &self.skills[*index])
    }

    pub fn by_pack(&self, pack: &str) -> impl Iterator<Item = &RegisteredSkill> {
        self.pack_index
            .get(pack)
            .into_iter()
            .flat_map(|indices| indices.iter())
            .map(|index| &self.skills[*index])
    }

    pub fn requiring_hook(&self, hook_id: &HookId) -> impl Iterator<Item = &RegisteredSkill> {
        self.hook_index
            .get(hook_id)
            .into_iter()
            .flat_map(|indices| indices.iter())
            .map(|index| &self.skills[*index])
    }

    pub fn allowing_tool(&self, tool_id: &ToolId) -> impl Iterator<Item = &RegisteredSkill> {
        self.tool_index
            .get(tool_id)
            .into_iter()
            .flat_map(|indices| indices.iter())
            .map(|index| &self.skills[*index])
    }

    pub fn resolve_for_context_injection(
        &self,
        id: &SkillId,
    ) -> Result<&RegisteredSkill, SkillRegistryError> {
        let skill = self
            .get(id)
            .ok_or_else(|| SkillRegistryError::UnknownSkill {
                id: id.as_str().to_string(),
            })?;
        if skill.permits_context_injection() {
            Ok(skill)
        } else {
            Err(SkillRegistryError::UntrustedSkill {
                id: id.as_str().to_string(),
                source: skill.harness.source,
                trust: skill.trust,
            })
        }
    }

    pub fn verify_required_hooks(
        &self,
        id: &SkillId,
        tool_catalog: &ToolCatalog,
    ) -> Result<(), SkillRegistryError> {
        let skill = self.resolve_for_context_injection(id)?;
        let missing = skill
            .harness
            .required_hooks
            .iter()
            .filter(|hook| !tool_catalog.has_hook(hook))
            .cloned()
            .collect::<Vec<_>>();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(SkillRegistryError::MissingRequiredHooks {
                id: id.as_str().to_string(),
                hooks: missing,
            })
        }
    }

    pub fn verify_allowed_tools(
        &self,
        id: &SkillId,
        tool_catalog: &ToolCatalog,
    ) -> Result<(), SkillRegistryError> {
        let skill = self.resolve_for_context_injection(id)?;
        let missing = skill
            .harness
            .allowed_tools
            .iter()
            .filter(|tool| tool_catalog.tool(tool).is_none())
            .cloned()
            .collect::<Vec<_>>();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(SkillRegistryError::MissingAllowedTools {
                id: id.as_str().to_string(),
                tools: missing,
            })
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum SkillRegistryError {
    Manifest(SkillManifestError),
    DuplicateSkill {
        id: String,
    },
    UnknownSkill {
        id: String,
    },
    UntrustedSkill {
        id: String,
        source: SkillSource,
        trust: SkillTrustState,
    },
    MissingRequiredHooks {
        id: String,
        hooks: Vec<HookId>,
    },
    MissingAllowedTools {
        id: String,
        tools: Vec<ToolId>,
    },
    ToolCatalog(ToolCatalogError),
}

impl std::fmt::Display for SkillRegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manifest(err) => write!(f, "{err}"),
            Self::DuplicateSkill { id } => write!(f, "duplicate skill id `{id}`"),
            Self::UnknownSkill { id } => write!(f, "unknown skill `{id}`"),
            Self::UntrustedSkill { id, source, trust } => write!(
                f,
                "skill `{id}` cannot be injected with source {source:?} and trust {trust:?}"
            ),
            Self::MissingRequiredHooks { id, hooks } => {
                let hooks = hooks
                    .iter()
                    .map(HookId::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "skill `{id}` is missing required hooks: {hooks}")
            }
            Self::MissingAllowedTools { id, tools } => {
                let tools = tools
                    .iter()
                    .map(ToolId::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "skill `{id}` is missing allowed tools: {tools}")
            }
            Self::ToolCatalog(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for SkillRegistryError {}

#[cfg(test)]
mod tests {
    use agl_tools::{
        HookDeclaration, HookEvent, ToolCatalog, ToolProviderDeclaration, ToolProviderId,
    };

    use super::*;

    #[test]
    fn builtin_registry_loads_trusted_core_skill() {
        let registry = SkillRegistry::from_builtin_assets().unwrap();
        let id = SkillId::new("task-spec").unwrap();
        let skill = registry.resolve_for_context_injection(&id).unwrap();

        assert_eq!(skill.trust, SkillTrustState::TrustedByBinary);
        assert_eq!(skill.harness.manifest_sha256.len(), 64);
        assert_eq!(skill.harness.tree_sha256.len(), 64);
        assert_eq!(
            registry
                .by_pack("agl")
                .map(|skill| skill.harness.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "change",
                "commit",
                "commit-hygiene",
                "cron-planner",
                "memory-capture",
                "notes-capture",
                "repo-review",
                "repo-status",
                "review-pack",
                "rust",
                "security-review",
                "skill",
                "smoke-test",
                "task-spec",
                "test-triage",
                "tool-smoke",
            ]
        );
    }

    #[test]
    fn registry_indexes_required_hooks() {
        let registry = SkillRegistry::from_builtin_assets().unwrap();
        let hook_id = HookId::new("task_spec.validate").unwrap();

        let skills = registry
            .requiring_hook(&hook_id)
            .map(|skill| skill.harness.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(skills, vec!["task-spec"]);
    }

    #[test]
    fn registry_indexes_allowed_tools() {
        let registry = SkillRegistry::from_builtin_assets().unwrap();
        let tool_id = ToolId::new("fs.read").unwrap();

        let skills = registry
            .allowing_tool(&tool_id)
            .map(|skill| skill.harness.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            skills,
            vec![
                "change",
                "commit",
                "commit-hygiene",
                "cron-planner",
                "memory-capture",
                "notes-capture",
                "repo-review",
                "repo-status",
                "review-pack",
                "rust",
                "security-review",
                "skill",
                "smoke-test",
                "task-spec",
                "test-triage",
                "tool-smoke",
            ]
        );
    }

    #[test]
    fn non_builtin_sources_default_to_non_injectable_states() {
        assert_eq!(
            SkillSource::Workspace.default_trust_state(),
            SkillTrustState::Unknown
        );
        assert_eq!(
            SkillSource::ThirdParty.default_trust_state(),
            SkillTrustState::Unsupported
        );
        assert!(!SkillTrustState::Unknown.permits_context_injection());
        assert!(!SkillTrustState::Unsupported.permits_context_injection());
        assert!(SkillTrustState::TrustedLocal.permits_context_injection());
    }

    #[test]
    fn unknown_skill_is_rejected_before_context_injection() {
        let registry = SkillRegistry::from_builtin_assets().unwrap();
        let err = registry
            .resolve_for_context_injection(&SkillId::new("missing").unwrap())
            .unwrap_err();

        assert_eq!(
            err,
            SkillRegistryError::UnknownSkill {
                id: "missing".to_string(),
            }
        );
    }

    #[test]
    fn missing_required_hooks_fail_preflight() {
        let registry = SkillRegistry::from_builtin_assets().unwrap();
        let extensions = ToolCatalog::new();
        let err = registry
            .verify_required_hooks(&SkillId::new("task-spec").unwrap(), &extensions)
            .unwrap_err();

        assert_eq!(
            err,
            SkillRegistryError::MissingRequiredHooks {
                id: "task-spec".to_string(),
                hooks: vec![
                    HookId::new("repo_path.validate").unwrap(),
                    HookId::new("task_spec.validate").unwrap(),
                ],
            }
        );
    }

    #[test]
    fn present_required_hooks_pass_preflight() {
        let registry = SkillRegistry::from_builtin_assets().unwrap();
        let mut extensions = ToolCatalog::new();
        extensions.register(core_guard_declaration()).unwrap();

        registry
            .verify_required_hooks(&SkillId::new("task-spec").unwrap(), &extensions)
            .unwrap();
    }

    #[test]
    fn missing_allowed_tools_fail_preflight() {
        let registry = SkillRegistry::from_builtin_assets().unwrap();
        let extensions = ToolCatalog::new();
        let err = registry
            .verify_allowed_tools(&SkillId::new("task-spec").unwrap(), &extensions)
            .unwrap_err();

        assert_eq!(
            err,
            SkillRegistryError::MissingAllowedTools {
                id: "task-spec".to_string(),
                tools: vec![
                    ToolId::new("fs.edit").unwrap(),
                    ToolId::new("fs.list").unwrap(),
                    ToolId::new("fs.read").unwrap(),
                    ToolId::new("fs.search").unwrap(),
                ],
            }
        );
    }

    fn core_guard_declaration() -> ToolProviderDeclaration {
        ToolProviderDeclaration::new(
            ToolProviderId::new("core-guards").unwrap(),
            "Core Guards",
            "1",
        )
        .unwrap()
        .with_hook(HookDeclaration {
            id: HookId::new("repo_path.validate").unwrap(),
            event: HookEvent::ArtifactWrite,
            required: true,
        })
        .with_hook(HookDeclaration {
            id: HookId::new("task_spec.validate").unwrap(),
            event: HookEvent::ArtifactWrite,
            required: true,
        })
    }
}
