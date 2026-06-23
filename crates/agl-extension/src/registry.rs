use std::collections::BTreeMap;

use crate::{
    BundledSkillDeclaration, HookDeclaration, HookId, SkillId, StaticExtensionDeclaration,
    StaticExtensionDeclarationError, ToolDeclaration, ToolId,
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StaticExtensionRegistry {
    extensions: Vec<StaticExtensionDeclaration>,
    hook_index: BTreeMap<HookId, usize>,
    skill_index: BTreeMap<SkillId, usize>,
    tool_index: BTreeMap<ToolId, usize>,
}

impl StaticExtensionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        declaration: StaticExtensionDeclaration,
    ) -> Result<(), StaticExtensionRegistryError> {
        declaration
            .validate()
            .map_err(StaticExtensionRegistryError::InvalidDeclaration)?;
        let extension_index = self.extensions.len();
        for existing in &self.extensions {
            if existing.id == declaration.id {
                return Err(StaticExtensionRegistryError::DuplicateExtension {
                    id: declaration.id.as_str().to_string(),
                });
            }
        }
        for hook in &declaration.hooks {
            if self.hook_index.contains_key(&hook.id) {
                return Err(StaticExtensionRegistryError::DuplicateHook {
                    id: hook.id.as_str().to_string(),
                });
            }
        }
        for tool in &declaration.tools {
            if self.tool_index.contains_key(&tool.id) {
                return Err(StaticExtensionRegistryError::DuplicateTool {
                    id: tool.id.as_str().to_string(),
                });
            }
        }
        for skill in &declaration.bundled_skills {
            if self.skill_index.contains_key(&skill.id) {
                return Err(StaticExtensionRegistryError::DuplicateBundledSkill {
                    id: skill.id.as_str().to_string(),
                });
            }
        }
        for hook in &declaration.hooks {
            self.hook_index.insert(hook.id.clone(), extension_index);
        }
        for tool in &declaration.tools {
            self.tool_index.insert(tool.id.clone(), extension_index);
        }
        for skill in &declaration.bundled_skills {
            self.skill_index.insert(skill.id.clone(), extension_index);
        }
        self.extensions.push(declaration);
        Ok(())
    }

    pub fn extensions(&self) -> &[StaticExtensionDeclaration] {
        &self.extensions
    }

    pub fn hook(&self, id: &HookId) -> Option<&HookDeclaration> {
        let extension_index = *self.hook_index.get(id)?;
        self.extensions[extension_index]
            .hooks
            .iter()
            .find(|hook| &hook.id == id)
    }

    pub fn tool(&self, id: &ToolId) -> Option<&ToolDeclaration> {
        let extension_index = *self.tool_index.get(id)?;
        self.extensions[extension_index]
            .tools
            .iter()
            .find(|tool| &tool.id == id)
    }

    pub fn bundled_skill(&self, id: &SkillId) -> Option<&BundledSkillDeclaration> {
        let extension_index = *self.skill_index.get(id)?;
        self.extensions[extension_index]
            .bundled_skills
            .iter()
            .find(|skill| &skill.id == id)
    }

    pub fn has_hook(&self, id: &HookId) -> bool {
        self.hook_index.contains_key(id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StaticExtensionRegistryError {
    InvalidDeclaration(StaticExtensionDeclarationError),
    DuplicateExtension { id: String },
    DuplicateHook { id: String },
    DuplicateBundledSkill { id: String },
    DuplicateTool { id: String },
}

impl std::fmt::Display for StaticExtensionRegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDeclaration(err) => write!(f, "{err}"),
            Self::DuplicateExtension { id } => write!(f, "duplicate extension id `{id}`"),
            Self::DuplicateHook { id } => write!(f, "duplicate hook id `{id}`"),
            Self::DuplicateBundledSkill { id } => {
                write!(f, "duplicate bundled skill id `{id}`")
            }
            Self::DuplicateTool { id } => write!(f, "duplicate tool id `{id}`"),
        }
    }
}

impl std::error::Error for StaticExtensionRegistryError {}

#[cfg(test)]
mod tests {
    use crate::{
        BundledSkillDeclaration, ExtensionId, HookDeclaration, HookEvent, HookId, SkillId,
        StaticExtensionDeclaration, StaticExtensionRegistry, StaticExtensionRegistryError,
        ToolDeclaration, ToolId,
    };

    #[test]
    fn registry_registers_hooks_and_tools() {
        let hook_id = HookId::new("json.validate").unwrap();
        let tool_id = ToolId::new("file_read").unwrap();
        let declaration = StaticExtensionDeclaration::new(
            ExtensionId::new("core-guards").unwrap(),
            "Core Guards",
            "1",
        )
        .unwrap()
        .with_hook(HookDeclaration {
            id: hook_id.clone(),
            event: HookEvent::ModelResponse,
            required: true,
        })
        .with_tool(ToolDeclaration {
            id: tool_id.clone(),
            description: "Read a file".to_string(),
            required_arguments: vec!["path".to_string()],
        });
        let mut registry = StaticExtensionRegistry::new();

        registry.register(declaration).unwrap();

        assert!(registry.has_hook(&hook_id));
        assert_eq!(
            registry.hook(&hook_id).unwrap().event,
            HookEvent::ModelResponse
        );
        assert_eq!(registry.tool(&tool_id).unwrap().description, "Read a file");
        assert_eq!(
            registry.tool(&tool_id).unwrap().required_arguments,
            vec!["path"]
        );
    }

    #[test]
    fn registry_rejects_duplicate_hooks_across_extensions() {
        let first = StaticExtensionDeclaration::new(
            ExtensionId::new("core-guards").unwrap(),
            "Core Guards",
            "1",
        )
        .unwrap()
        .with_hook(HookDeclaration {
            id: HookId::new("json.validate").unwrap(),
            event: HookEvent::ModelResponse,
            required: true,
        });
        let second = StaticExtensionDeclaration::new(
            ExtensionId::new("other-guards").unwrap(),
            "Other Guards",
            "1",
        )
        .unwrap()
        .with_hook(HookDeclaration {
            id: HookId::new("json.validate").unwrap(),
            event: HookEvent::ArtifactWrite,
            required: true,
        });
        let mut registry = StaticExtensionRegistry::new();
        registry.register(first).unwrap();

        assert_eq!(
            registry.register(second).unwrap_err(),
            StaticExtensionRegistryError::DuplicateHook {
                id: "json.validate".to_string(),
            }
        );
    }

    #[test]
    fn registry_indexes_bundled_skills() {
        let skill_id = SkillId::new("core:task-spec").unwrap();
        let declaration = StaticExtensionDeclaration::new(
            ExtensionId::new("core-guards").unwrap(),
            "Core Guards",
            "1",
        )
        .unwrap()
        .with_bundled_skill(BundledSkillDeclaration {
            id: skill_id.clone(),
        });
        let mut registry = StaticExtensionRegistry::new();

        registry.register(declaration).unwrap();

        assert_eq!(registry.bundled_skill(&skill_id).unwrap().id, skill_id);
    }
}
