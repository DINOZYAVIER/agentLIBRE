//! Authoring and generated-package SDK for statically linked Extensions.

#![forbid(unsafe_code)]

pub mod package;

use std::any::Any;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::sync::Arc;

use agl_artifact::ArtifactHandle;
use agl_kernel::{
    ArtifactDeclaration, DeclarationDigest, EffectDeclaration, ExtensionDescriptor, ExtensionId,
    ExtensionRequirement, ExtensionSource, ExtensionTrust, ExtensionWorkflowFragment, HookBinding,
    HookDeclaration, HostBindingId, HostBindingRequirement, ToolBinding, ToolDeclaration,
};

pub trait Extension: Send + Sync + 'static {
    type BindError: Error + Send + Sync + 'static;

    fn definition() -> ExtensionDefinition;

    fn bind(host: &ExtensionHost) -> Result<ExtensionBindings, Self::BindError>;
}

pub fn export_extension_package<E: Extension>(
    source: impl Into<std::path::PathBuf>,
    output: impl AsRef<std::path::Path>,
) -> Result<package::ExtensionPackageReport, package::ExtensionPackageError> {
    let input = package::ExtensionPackageBuildInput::new(E::definition(), source)?;
    package::ExtensionPackageBuilder::build(&input, output)
}

#[macro_export]
macro_rules! export_main {
    ($extension:ty) => {
        fn main() {
            let mut arguments = std::env::args_os().skip(1);
            let source = arguments
                .next()
                .expect("Extension source directory argument");
            let output = arguments
                .next()
                .expect("Extension output directory argument");
            if arguments.next().is_some() {
                panic!("Extension exporter accepts exactly source and output directories");
            }
            let report = $crate::export_extension_package::<$extension>(source, output)
                .expect("Extension package export failed");
            println!(
                "{} {} {}",
                report.extension_id, report.declaration_digest, report.package_tree_digest
            );
        }
    };
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtensionDefinition {
    pub id: ExtensionId,
    pub name: String,
    pub version: String,
    pub api_major: u32,
    descriptor: ExtensionDescriptor,
}

impl ExtensionDefinition {
    pub fn from_descriptor(
        api_major: u32,
        descriptor: ExtensionDescriptor,
    ) -> Result<Self, agl_kernel::DeclarationError> {
        descriptor.validate()?;
        if api_major == 0 {
            return Err(agl_kernel::DeclarationError::InvalidExtensionApiMajor {
                extension_id: descriptor.id,
            });
        }
        Ok(Self {
            id: descriptor.id.clone(),
            name: descriptor.name.clone(),
            version: descriptor.version.clone(),
            api_major,
            descriptor,
        })
    }

    pub fn builder(
        id: ExtensionId,
        name: impl Into<String>,
        version: impl Into<String>,
        api_major: u32,
    ) -> ExtensionDefinitionBuilder {
        ExtensionDefinitionBuilder {
            id,
            name: name.into(),
            version: version.into(),
            api_major,
            hooks: Vec::new(),
            effects: Vec::new(),
            tools: Vec::new(),
            requirements: Vec::new(),
            artifacts: Vec::new(),
            host_bindings: Vec::new(),
            workflow: None,
        }
    }

    pub fn descriptor(&self) -> &ExtensionDescriptor {
        &self.descriptor
    }

    pub fn runtime_descriptor(
        &self,
        source: ExtensionSource,
        trust: ExtensionTrust,
    ) -> ExtensionDescriptor {
        self.descriptor.clone().with_runtime_identity(source, trust)
    }

    pub fn digest(&self) -> DeclarationDigest {
        self.descriptor.digest()
    }
}

pub struct ExtensionDefinitionBuilder {
    id: ExtensionId,
    name: String,
    version: String,
    api_major: u32,
    hooks: Vec<HookDeclaration>,
    effects: Vec<EffectDeclaration>,
    tools: Vec<ToolDeclaration>,
    requirements: Vec<ExtensionRequirement>,
    artifacts: Vec<ArtifactDeclaration>,
    host_bindings: Vec<HostBindingRequirement>,
    workflow: Option<ExtensionWorkflowFragment>,
}

impl ExtensionDefinitionBuilder {
    pub fn hook(mut self, hook: HookDeclaration) -> Self {
        self.hooks.push(hook);
        self
    }

    pub fn effect(mut self, effect: EffectDeclaration) -> Self {
        self.effects.push(effect);
        self
    }

    pub fn tool(mut self, tool: ToolDeclaration) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn require_extension(mut self, extension_id: ExtensionId, api_major: u32) -> Self {
        self.requirements
            .push(ExtensionRequirement::new(extension_id, api_major));
        self
    }

    pub fn artifact(mut self, artifact: ArtifactDeclaration) -> Self {
        self.artifacts.push(artifact);
        self
    }

    pub fn require_host_binding(mut self, id: HostBindingId, api_major: u32) -> Self {
        self.host_bindings
            .push(HostBindingRequirement::new(id, api_major));
        self
    }

    pub fn workflow(mut self, workflow: ExtensionWorkflowFragment) -> Self {
        self.workflow = Some(workflow);
        self
    }

    pub fn build(self) -> Result<ExtensionDefinition, agl_kernel::DeclarationError> {
        let mut descriptor = ExtensionDescriptor::new(
            self.id.clone(),
            self.name.clone(),
            self.version.clone(),
            ExtensionSource::Builtin,
            ExtensionTrust::TrustedByBinary,
        )?;
        for hook in self.hooks {
            descriptor = descriptor.with_hook(hook);
        }
        for effect in self.effects {
            descriptor = descriptor.with_effect(effect);
        }
        for tool in self.tools {
            descriptor = descriptor.with_tool(tool);
        }
        for requirement in self.requirements {
            descriptor = descriptor.with_requirement(requirement);
        }
        for artifact in self.artifacts {
            descriptor = descriptor.with_artifact(artifact);
        }
        for binding in self.host_bindings {
            descriptor = descriptor.with_host_binding(binding);
        }
        if let Some(workflow) = self.workflow {
            descriptor = descriptor.with_workflow(workflow);
        }
        descriptor.validate()?;
        if self.api_major == 0 {
            return Err(agl_kernel::DeclarationError::InvalidExtensionApiMajor {
                extension_id: self.id,
            });
        }
        Ok(ExtensionDefinition {
            id: self.id,
            name: self.name,
            version: self.version,
            api_major: self.api_major,
            descriptor,
        })
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct StaticExtensionFactoryKey {
    pub extension_id: ExtensionId,
    pub api_major: u32,
    pub declaration_digest: DeclarationDigest,
}

pub struct StaticExtensionFactory {
    key: StaticExtensionFactoryKey,
    definition: fn() -> ExtensionDefinition,
    bind: fn(&ExtensionHost) -> Result<ExtensionBindings, ErasedBindError>,
}

impl StaticExtensionFactory {
    pub fn for_extension<E: Extension>() -> Self {
        fn definition<E: Extension>() -> ExtensionDefinition {
            E::definition()
        }
        fn bind<E: Extension>(host: &ExtensionHost) -> Result<ExtensionBindings, ErasedBindError> {
            E::bind(host).map_err(ErasedBindError::new)
        }

        let authored = E::definition();
        Self {
            key: StaticExtensionFactoryKey {
                extension_id: authored.id.clone(),
                api_major: authored.api_major,
                declaration_digest: authored.digest(),
            },
            definition: definition::<E>,
            bind: bind::<E>,
        }
    }

    pub fn key(&self) -> &StaticExtensionFactoryKey {
        &self.key
    }

    pub fn definition(&self) -> ExtensionDefinition {
        (self.definition)()
    }

    pub fn bind(&self, host: &ExtensionHost) -> Result<ExtensionBindings, ErasedBindError> {
        (self.bind)(host)
    }
}

#[derive(Debug)]
pub struct ErasedBindError(Box<dyn Error + Send + Sync>);

impl ErasedBindError {
    fn new(error: impl Error + Send + Sync + 'static) -> Self {
        Self(Box::new(error))
    }
}

impl Display for ErasedBindError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, formatter)
    }
}

impl Error for ErasedBindError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.0.as_ref())
    }
}

pub struct ExtensionBindings {
    tools: Vec<ToolBinding>,
    hooks: Vec<HookBinding>,
}

impl ExtensionBindings {
    pub fn new(
        tools: impl IntoIterator<Item = ToolBinding>,
        hooks: impl IntoIterator<Item = HookBinding>,
    ) -> Self {
        Self {
            tools: tools.into_iter().collect(),
            hooks: hooks.into_iter().collect(),
        }
    }

    pub fn empty() -> Self {
        Self::new([], [])
    }

    pub fn tools(&self) -> &[ToolBinding] {
        &self.tools
    }

    pub fn hooks(&self) -> &[HookBinding] {
        &self.hooks
    }

    pub fn into_parts(self) -> (Vec<ToolBinding>, Vec<HookBinding>) {
        (self.tools, self.hooks)
    }
}

#[derive(Clone)]
pub struct ExtensionHost {
    bindings: BTreeMap<HostBindingId, HostBinding>,
    artifacts: BTreeMap<agl_kernel::ArtifactId, ArtifactHandle>,
}

impl ExtensionHost {
    pub fn builder() -> ExtensionHostBuilder {
        ExtensionHostBuilder::default()
    }

    pub fn empty() -> Self {
        Self::builder().build()
    }

    pub fn binding(&self, id: &HostBindingId) -> Option<&HostBinding> {
        self.bindings.get(id)
    }

    pub fn shared_tool_handler(
        &self,
        id: &HostBindingId,
    ) -> Option<&Arc<dyn agl_kernel::ToolHandler>> {
        self.binding(id)?.downcast_ref()
    }

    pub fn artifact(&self, id: &agl_kernel::ArtifactId) -> Option<&ArtifactHandle> {
        self.artifacts.get(id)
    }
}

#[derive(Clone)]
pub struct HostBinding {
    api_major: u32,
    value: Arc<dyn Any + Send + Sync>,
}

impl HostBinding {
    pub fn api_major(&self) -> u32 {
        self.api_major
    }

    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        self.value.downcast_ref()
    }
}

#[derive(Default)]
pub struct ExtensionHostBuilder {
    bindings: BTreeMap<HostBindingId, HostBinding>,
    artifacts: BTreeMap<agl_kernel::ArtifactId, ArtifactHandle>,
}

impl ExtensionHostBuilder {
    pub fn shared_tool_handler(
        self,
        id: HostBindingId,
        api_major: u32,
        handler: Arc<dyn agl_kernel::ToolHandler>,
    ) -> Self {
        self.binding(id, api_major, handler)
    }

    pub fn binding(
        mut self,
        id: HostBindingId,
        api_major: u32,
        value: impl Any + Send + Sync + 'static,
    ) -> Self {
        self.bindings.insert(
            id,
            HostBinding {
                api_major,
                value: Arc::new(value),
            },
        );
        self
    }

    pub fn artifact(mut self, handle: ArtifactHandle) -> Self {
        self.artifacts.insert(handle.id().clone(), handle);
        self
    }

    pub fn build(self) -> ExtensionHost {
        ExtensionHost {
            bindings: self.bindings,
            artifacts: self.artifacts,
        }
    }
}
