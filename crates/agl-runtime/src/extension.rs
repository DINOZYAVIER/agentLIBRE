use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};
use std::sync::Arc;

use agl_kernel::{DeclarationDigest, ExtensionDescriptor, ExtensionId, HookId, ToolId};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct StaticExtensionFactoryKey {
    pub extension_id: ExtensionId,
    pub api_major: u32,
    pub declaration_digest: DeclarationDigest,
}

impl StaticExtensionFactoryKey {
    pub fn new(
        extension_id: ExtensionId,
        api_major: u32,
        declaration_digest: DeclarationDigest,
    ) -> Self {
        Self {
            extension_id,
            api_major,
            declaration_digest,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StaticExtensionRegistryError {
    DuplicateKey,
    MissingKey,
    DescriptorIdentityMismatch,
    DuplicateToolBinding(ToolId),
    MissingToolBinding(ToolId),
    UndeclaredToolBinding(ToolId),
    DuplicateHookBinding(HookId),
    MissingHookBinding(HookId),
    UndeclaredHookBinding(HookId),
}

impl std::fmt::Display for StaticExtensionRegistryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "static Extension registry error: {self:?}")
    }
}

impl std::error::Error for StaticExtensionRegistryError {}

#[derive(Default)]
pub struct StaticExtensionRegistry<T> {
    factories: BTreeMap<StaticExtensionFactoryKey, T>,
}

impl<T> StaticExtensionRegistry<T> {
    pub fn new() -> Self {
        Self {
            factories: BTreeMap::new(),
        }
    }

    pub fn register(
        &mut self,
        key: StaticExtensionFactoryKey,
        factory: T,
    ) -> Result<(), StaticExtensionRegistryError> {
        if self.factories.contains_key(&key) {
            return Err(StaticExtensionRegistryError::DuplicateKey);
        }
        self.factories.insert(key, factory);
        Ok(())
    }

    pub fn resolve(
        &self,
        key: &StaticExtensionFactoryKey,
    ) -> Result<&T, StaticExtensionRegistryError> {
        self.factories
            .get(key)
            .ok_or(StaticExtensionRegistryError::MissingKey)
    }

    pub fn contains(&self, key: &StaticExtensionFactoryKey) -> bool {
        self.factories.contains_key(key)
    }
}

pub fn validate_static_bindings(
    descriptor: &ExtensionDescriptor,
    key: &StaticExtensionFactoryKey,
    tool_bindings: impl IntoIterator<Item = ToolId>,
    hook_bindings: impl IntoIterator<Item = HookId>,
) -> Result<(), StaticExtensionRegistryError> {
    if descriptor.id != key.extension_id {
        return Err(StaticExtensionRegistryError::DescriptorIdentityMismatch);
    }
    exact_tool_ids(
        descriptor.tools.iter().map(|tool| tool.id.clone()),
        tool_bindings,
    )?;
    exact_hook_ids(
        descriptor.hooks.iter().map(|hook| hook.id.clone()),
        hook_bindings,
    )
}

fn exact_tool_ids(
    declared: impl IntoIterator<Item = ToolId>,
    bound: impl IntoIterator<Item = ToolId>,
) -> Result<(), StaticExtensionRegistryError> {
    let declared = declared.into_iter().collect::<BTreeSet<_>>();
    let mut bindings = BTreeSet::new();
    for id in bound {
        if !bindings.insert(id.clone()) {
            return Err(StaticExtensionRegistryError::DuplicateToolBinding(id));
        }
    }
    if let Some(id) = declared.difference(&bindings).next() {
        return Err(StaticExtensionRegistryError::MissingToolBinding(id.clone()));
    }
    if let Some(id) = bindings.difference(&declared).next() {
        return Err(StaticExtensionRegistryError::UndeclaredToolBinding(
            id.clone(),
        ));
    }
    Ok(())
}

fn exact_hook_ids(
    declared: impl IntoIterator<Item = HookId>,
    bound: impl IntoIterator<Item = HookId>,
) -> Result<(), StaticExtensionRegistryError> {
    let declared = declared.into_iter().collect::<BTreeSet<_>>();
    let mut bindings = BTreeSet::new();
    for id in bound {
        if !bindings.insert(id.clone()) {
            return Err(StaticExtensionRegistryError::DuplicateHookBinding(id));
        }
    }
    if let Some(id) = declared.difference(&bindings).next() {
        return Err(StaticExtensionRegistryError::MissingHookBinding(id.clone()));
    }
    if let Some(id) = bindings.difference(&declared).next() {
        return Err(StaticExtensionRegistryError::UndeclaredHookBinding(
            id.clone(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedExtensionBinary {
    relative_path: String,
    digest: String,
    bytes: Arc<[u8]>,
}

impl VerifiedExtensionBinary {
    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

pub fn verify_extension_binary(
    relative_path: &str,
    expected_digest: &str,
    bytes: &[u8],
) -> Result<VerifiedExtensionBinary, String> {
    let path = Path::new(relative_path);
    if relative_path.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("Extension binary path must be a safe relative path".to_string());
    }
    DeclarationDigest::parse(expected_digest).map_err(|error| error.to_string())?;
    let actual = format!(
        "sha256:{}",
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    if actual != expected_digest {
        return Err("Extension binary digest mismatch".to_string());
    }
    Ok(VerifiedExtensionBinary {
        relative_path: relative_path.to_string(),
        digest: expected_digest.to_string(),
        bytes: Arc::from(bytes),
    })
}

pub fn verify_declared_extension_binary(
    relative_path: &str,
    expected_digest: &str,
    bytes: Option<&[u8]>,
) -> Result<VerifiedExtensionBinary, String> {
    let bytes = bytes.ok_or_else(|| "declared Extension binary is missing".to_string())?;
    verify_extension_binary(relative_path, expected_digest, bytes)
}
