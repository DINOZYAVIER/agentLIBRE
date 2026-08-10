use std::collections::{BTreeMap, BTreeSet};

use agl_extension::package::{ExtensionPackage, ExtensionPackageError};
use agl_extension::{
    ErasedBindError, ExtensionHost, StaticExtensionFactory, StaticExtensionFactoryKey,
};
use agl_kernel::{
    CatalogDigest, DeclarationDigest, ExtensionDescriptor, ExtensionId, ExtensionRegistration,
    ExtensionSource, ExtensionTrust, HookId, HostBindingId, ToolId, ToolRuntime,
};

#[derive(Default)]
pub struct StaticExtensionRegistry {
    factories: BTreeMap<StaticExtensionFactoryKey, StaticExtensionFactory>,
}

impl StaticExtensionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_factories(
        factories: impl IntoIterator<Item = StaticExtensionFactory>,
    ) -> Result<Self, ExtensionLoadError> {
        let mut registry = Self::new();
        for factory in factories {
            registry.register(factory)?;
        }
        Ok(registry)
    }

    pub fn register(&mut self, factory: StaticExtensionFactory) -> Result<(), ExtensionLoadError> {
        let key = factory.key().clone();
        if self.factories.insert(key.clone(), factory).is_some() {
            return Err(ExtensionLoadError::DuplicateFactoryKey {
                extension_id: key.extension_id,
                api_major: key.api_major,
                declaration_digest: key.declaration_digest,
            });
        }
        Ok(())
    }

    pub fn resolve(&self, key: &StaticExtensionFactoryKey) -> Option<&StaticExtensionFactory> {
        self.factories.get(key)
    }

    fn has_extension(&self, id: &ExtensionId) -> bool {
        self.factories.keys().any(|key| &key.extension_id == id)
    }

    fn keys(&self) -> impl Iterator<Item = &StaticExtensionFactoryKey> {
        self.factories.keys()
    }
}

pub struct ExtensionCompositionInput {
    registry: StaticExtensionRegistry,
    packages: Vec<ExtensionPackage>,
    selected: BTreeMap<ExtensionId, bool>,
    source: ExtensionSource,
    trust: ExtensionTrust,
    host: ExtensionHost,
}

impl ExtensionCompositionInput {
    pub fn builder() -> ExtensionCompositionInputBuilder {
        ExtensionCompositionInputBuilder::default()
    }

    pub fn with_runtime_identity(mut self, source: ExtensionSource, trust: ExtensionTrust) -> Self {
        self.source = source;
        self.trust = trust;
        self
    }
}

pub struct ExtensionCompositionInputBuilder {
    registry: Option<StaticExtensionRegistry>,
    packages: Vec<ExtensionPackage>,
    selected: BTreeMap<ExtensionId, bool>,
    source: ExtensionSource,
    trust: ExtensionTrust,
    host: Option<ExtensionHost>,
}

impl Default for ExtensionCompositionInputBuilder {
    fn default() -> Self {
        Self {
            registry: None,
            packages: Vec::new(),
            selected: BTreeMap::new(),
            source: ExtensionSource::Builtin,
            trust: ExtensionTrust::TrustedByBinary,
            host: None,
        }
    }
}

impl ExtensionCompositionInputBuilder {
    pub fn registry(mut self, registry: StaticExtensionRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    pub fn package(mut self, package: ExtensionPackage) -> Self {
        self.packages.push(package);
        self
    }

    pub fn selected(mut self, extension_id: ExtensionId, selected: bool) -> Self {
        self.selected.insert(extension_id, selected);
        self
    }

    pub fn runtime_identity(mut self, source: ExtensionSource, trust: ExtensionTrust) -> Self {
        self.source = source;
        self.trust = trust;
        self
    }

    pub fn host(mut self, host: ExtensionHost) -> Self {
        self.host = Some(host);
        self
    }

    pub fn build(self) -> Result<ExtensionCompositionInput, ExtensionLoadError> {
        Ok(ExtensionCompositionInput {
            registry: self
                .registry
                .ok_or(ExtensionLoadError::InvalidCompositionInput { field: "registry" })?,
            packages: self.packages,
            selected: self.selected,
            source: self.source,
            trust: self.trust,
            host: self.host.unwrap_or_else(ExtensionHost::empty),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExtensionAvailability {
    Compiled,
    Selected,
    Admitted,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeExtensionState {
    pub id: ExtensionId,
    pub declaration_digest: DeclarationDigest,
    pub compiled: bool,
    pub selected: bool,
    pub admitted: bool,
    pub availability: ExtensionAvailability,
    pub unavailable_reason: Option<String>,
}

pub struct RuntimeExtensionCatalog {
    runtime: ToolRuntime,
    query: BTreeMap<ExtensionId, RuntimeExtensionState>,
    catalog_digest: CatalogDigest,
}

impl std::fmt::Debug for RuntimeExtensionCatalog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeExtensionCatalog")
            .field("admitted", &self.runtime.catalog().extensions())
            .field("query", &self.query)
            .field("catalog_digest", &self.catalog_digest)
            .finish()
    }
}

impl RuntimeExtensionCatalog {
    pub fn admitted(&self) -> &[ExtensionDescriptor] {
        self.runtime.catalog().extensions()
    }

    pub fn query(&self) -> &BTreeMap<ExtensionId, RuntimeExtensionState> {
        &self.query
    }

    pub fn catalog_digest(&self) -> &CatalogDigest {
        &self.catalog_digest
    }

    pub fn runtime(&self) -> &ToolRuntime {
        &self.runtime
    }

    pub fn into_runtime(self) -> ToolRuntime {
        self.runtime
    }

    pub fn snapshot_for_turn(&self) -> RuntimeExtensionSnapshot {
        RuntimeExtensionSnapshot {
            extensions: self.admitted().to_vec(),
            catalog_digest: self.catalog_digest.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeExtensionSnapshot {
    extensions: Vec<ExtensionDescriptor>,
    catalog_digest: CatalogDigest,
}

impl RuntimeExtensionSnapshot {
    pub fn extensions(&self) -> &[ExtensionDescriptor] {
        &self.extensions
    }

    pub fn extension_ids(&self) -> Vec<ExtensionId> {
        self.extensions
            .iter()
            .map(|extension| extension.id.clone())
            .collect()
    }

    pub fn catalog_digest(&self) -> &CatalogDigest {
        &self.catalog_digest
    }
}

pub fn compose_extension_catalog(
    input: ExtensionCompositionInput,
) -> Result<RuntimeExtensionCatalog, ExtensionLoadError> {
    let ExtensionCompositionInput {
        registry,
        packages,
        selected,
        source,
        trust,
        host,
    } = input;

    let mut package_ids = BTreeSet::new();
    let mut package_by_id = BTreeMap::new();
    for package in packages {
        let definition = package.definition().map_err(ExtensionLoadError::Package)?;
        if !package_ids.insert(definition.id.clone()) {
            return Err(ExtensionLoadError::DuplicatePackage {
                extension_id: definition.id.clone(),
            });
        }
        package_by_id.insert(definition.id.clone(), package);
    }

    let mut query = BTreeMap::new();
    for key in registry.keys() {
        query.insert(
            key.extension_id.clone(),
            RuntimeExtensionState {
                id: key.extension_id.clone(),
                declaration_digest: key.declaration_digest.clone(),
                compiled: true,
                selected: selected.get(&key.extension_id).copied().unwrap_or(false),
                admitted: false,
                availability: ExtensionAvailability::Compiled,
                unavailable_reason: None,
            },
        );
    }

    let mut runtime = ToolRuntime::new();
    for (extension_id, package) in package_by_id {
        let definition = package.definition().map_err(ExtensionLoadError::Package)?;
        let is_selected = selected.get(&extension_id).copied().unwrap_or(false);
        let key = StaticExtensionFactoryKey {
            extension_id: definition.id.clone(),
            api_major: definition.api_major,
            declaration_digest: definition.digest(),
        };
        let Some(factory) = registry.resolve(&key) else {
            let error = ExtensionLoadError::FactoryKeyMismatch {
                extension_id: definition.id.clone(),
                api_major: definition.api_major,
                declaration_digest: definition.digest(),
            };
            if is_selected || registry.has_extension(&definition.id) {
                return Err(error);
            }
            query.insert(
                definition.id.clone(),
                unavailable_state(definition, is_selected, error.to_string()),
            );
            continue;
        };

        let host_error = definition
            .descriptor()
            .host_bindings
            .iter()
            .find_map(|required| {
                let Some(actual) = host.binding(&required.id) else {
                    return Some(ExtensionLoadError::MissingHostBinding {
                        extension_id: definition.id.clone(),
                        binding_id: required.id.clone(),
                        required_api_major: required.api_major,
                    });
                };
                (actual.api_major() != required.api_major).then(|| {
                    ExtensionLoadError::HostBindingApiMajorMismatch {
                        extension_id: definition.id.clone(),
                        binding_id: required.id.clone(),
                        required_api_major: required.api_major,
                        actual_api_major: actual.api_major(),
                    }
                })
            });
        if let Some(error) = host_error {
            if is_selected {
                return Err(error);
            }
            query.insert(
                definition.id.clone(),
                unavailable_state(definition, false, error.to_string()),
            );
            continue;
        }

        let artifact_error = definition
            .descriptor()
            .artifacts
            .iter()
            .find_map(|declared| {
                let Some(handle) = host.artifact(&declared.id) else {
                    return Some(ExtensionLoadError::MissingArtifactBinding {
                        extension_id: definition.id.clone(),
                        artifact_id: declared.id.clone(),
                    });
                };
                (handle.declaration() != declared).then(|| {
                    ExtensionLoadError::ArtifactBindingMismatch {
                        extension_id: definition.id.clone(),
                        artifact_id: declared.id.clone(),
                    }
                })
            });
        if let Some(error) = artifact_error {
            if is_selected {
                return Err(error);
            }
            query.insert(
                definition.id.clone(),
                unavailable_state(definition, false, error.to_string()),
            );
            continue;
        }

        if !is_selected {
            query.insert(
                definition.id.clone(),
                RuntimeExtensionState {
                    id: definition.id.clone(),
                    declaration_digest: definition.digest(),
                    compiled: true,
                    selected: false,
                    admitted: false,
                    availability: ExtensionAvailability::Compiled,
                    unavailable_reason: None,
                },
            );
            continue;
        }

        let bindings = factory
            .bind(&host)
            .map_err(|source| ExtensionLoadError::Factory {
                extension_id: definition.id.clone(),
                source,
            })?;
        validate_bindings(definition.descriptor(), &bindings)?;
        let (tools, hooks) = bindings.into_parts();
        runtime.register_extension(
            ExtensionRegistration::new(definition.runtime_descriptor(source, trust), tools)
                .with_hook_bindings(hooks),
        )?;
        query.insert(
            definition.id.clone(),
            RuntimeExtensionState {
                id: definition.id.clone(),
                declaration_digest: definition.digest(),
                compiled: true,
                selected: true,
                admitted: true,
                availability: ExtensionAvailability::Admitted,
                unavailable_reason: None,
            },
        );
    }
    runtime.catalog().validate_artifact_links()?;
    let catalog_digest = CatalogDigest::from_admitted(runtime.catalog().extensions());
    Ok(RuntimeExtensionCatalog {
        runtime,
        query,
        catalog_digest,
    })
}

fn unavailable_state(
    definition: &agl_extension::ExtensionDefinition,
    selected: bool,
    reason: String,
) -> RuntimeExtensionState {
    RuntimeExtensionState {
        id: definition.id.clone(),
        declaration_digest: definition.digest(),
        compiled: true,
        selected,
        admitted: false,
        availability: ExtensionAvailability::Unavailable,
        unavailable_reason: Some(reason),
    }
}

fn validate_bindings(
    descriptor: &ExtensionDescriptor,
    bindings: &agl_extension::ExtensionBindings,
) -> Result<(), ExtensionLoadError> {
    exact_tools(
        &descriptor.id,
        descriptor.tools.iter().map(|tool| tool.id.clone()),
        bindings
            .tools()
            .iter()
            .map(|binding| binding.tool_id().clone()),
    )?;
    exact_hooks(
        &descriptor.id,
        descriptor.hooks.iter().map(|hook| hook.id.clone()),
        bindings
            .hooks()
            .iter()
            .map(|binding| binding.hook_id().clone()),
    )
}

fn exact_tools(
    extension_id: &ExtensionId,
    declared: impl IntoIterator<Item = ToolId>,
    bound: impl IntoIterator<Item = ToolId>,
) -> Result<(), ExtensionLoadError> {
    let declared = declared.into_iter().collect::<BTreeSet<_>>();
    let mut bindings = BTreeSet::new();
    for tool_id in bound {
        if !bindings.insert(tool_id.clone()) {
            return Err(ExtensionLoadError::DuplicateToolBinding {
                extension_id: extension_id.clone(),
                tool_id,
            });
        }
    }
    if let Some(tool_id) = declared.difference(&bindings).next() {
        return Err(ExtensionLoadError::MissingToolBinding {
            extension_id: extension_id.clone(),
            tool_id: tool_id.clone(),
        });
    }
    if let Some(tool_id) = bindings.difference(&declared).next() {
        return Err(ExtensionLoadError::UndeclaredToolBinding {
            extension_id: extension_id.clone(),
            tool_id: tool_id.clone(),
        });
    }
    Ok(())
}

fn exact_hooks(
    extension_id: &ExtensionId,
    declared: impl IntoIterator<Item = HookId>,
    bound: impl IntoIterator<Item = HookId>,
) -> Result<(), ExtensionLoadError> {
    let declared = declared.into_iter().collect::<BTreeSet<_>>();
    let mut bindings = BTreeSet::new();
    for hook_id in bound {
        if !bindings.insert(hook_id.clone()) {
            return Err(ExtensionLoadError::DuplicateHookBinding {
                extension_id: extension_id.clone(),
                hook_id,
            });
        }
    }
    if let Some(hook_id) = declared.difference(&bindings).next() {
        return Err(ExtensionLoadError::MissingHookBinding {
            extension_id: extension_id.clone(),
            hook_id: hook_id.clone(),
        });
    }
    if let Some(hook_id) = bindings.difference(&declared).next() {
        return Err(ExtensionLoadError::UndeclaredHookBinding {
            extension_id: extension_id.clone(),
            hook_id: hook_id.clone(),
        });
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ExtensionLoadError {
    #[error("Extension composition input is missing `{field}`")]
    InvalidCompositionInput { field: &'static str },
    #[error(
        "duplicate static factory key for Extension `{extension_id}` API {api_major} / {declaration_digest}"
    )]
    DuplicateFactoryKey {
        extension_id: ExtensionId,
        api_major: u32,
        declaration_digest: DeclarationDigest,
    },
    #[error("duplicate generated package for Extension `{extension_id}`")]
    DuplicatePackage { extension_id: ExtensionId },
    #[error(
        "no exact factory key for Extension `{extension_id}` API {api_major} / {declaration_digest}"
    )]
    FactoryKeyMismatch {
        extension_id: ExtensionId,
        api_major: u32,
        declaration_digest: DeclarationDigest,
    },
    #[error(
        "Extension `{extension_id}` requires host binding `{binding_id}` API {required_api_major}"
    )]
    MissingHostBinding {
        extension_id: ExtensionId,
        binding_id: HostBindingId,
        required_api_major: u32,
    },
    #[error(
        "Extension `{extension_id}` requires host binding `{binding_id}` API {required_api_major}, got {actual_api_major}"
    )]
    HostBindingApiMajorMismatch {
        extension_id: ExtensionId,
        binding_id: HostBindingId,
        required_api_major: u32,
        actual_api_major: u32,
    },
    #[error("Extension `{extension_id}` requires Artifact binding `{artifact_id}`")]
    MissingArtifactBinding {
        extension_id: ExtensionId,
        artifact_id: agl_kernel::ArtifactId,
    },
    #[error(
        "Extension `{extension_id}` Artifact binding `{artifact_id}` does not match its declaration"
    )]
    ArtifactBindingMismatch {
        extension_id: ExtensionId,
        artifact_id: agl_kernel::ArtifactId,
    },
    #[error("Extension `{extension_id}` factory bind failed: {source}")]
    Factory {
        extension_id: ExtensionId,
        #[source]
        source: ErasedBindError,
    },
    #[error("Extension `{extension_id}` has duplicate Tool binding `{tool_id}`")]
    DuplicateToolBinding {
        extension_id: ExtensionId,
        tool_id: ToolId,
    },
    #[error("Extension `{extension_id}` is missing Tool binding `{tool_id}`")]
    MissingToolBinding {
        extension_id: ExtensionId,
        tool_id: ToolId,
    },
    #[error("Extension `{extension_id}` returned undeclared Tool binding `{tool_id}`")]
    UndeclaredToolBinding {
        extension_id: ExtensionId,
        tool_id: ToolId,
    },
    #[error("Extension `{extension_id}` has duplicate Hook binding `{hook_id}`")]
    DuplicateHookBinding {
        extension_id: ExtensionId,
        hook_id: HookId,
    },
    #[error("Extension `{extension_id}` is missing Hook binding `{hook_id}`")]
    MissingHookBinding {
        extension_id: ExtensionId,
        hook_id: HookId,
    },
    #[error("Extension `{extension_id}` returned undeclared Hook binding `{hook_id}`")]
    UndeclaredHookBinding {
        extension_id: ExtensionId,
        hook_id: HookId,
    },
    #[error(transparent)]
    Package(#[from] ExtensionPackageError),
    #[error(transparent)]
    Kernel(#[from] agl_kernel::ToolCatalogError),
}
