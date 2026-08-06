#![allow(dead_code)]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct FactoryKey {
    pub extension_id: String,
    pub api_major: u32,
    pub declaration_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistrationView {
    pub descriptor_extension_id: String,
    pub descriptor_digest: String,
    pub tool_ids: BTreeSet<String>,
    pub hook_ids: BTreeSet<String>,
}

pub struct RegistrationFixture<'a> {
    pub artifact_extension_id: &'a str,
    pub artifact_digest: &'a str,
    pub factory_key: FactoryKey,
    pub declared_tools: &'a [&'a str],
    pub bound_tools: &'a [&'a str],
    pub declared_hooks: &'a [&'a str],
    pub bound_hooks: &'a [&'a str],
}

#[derive(Clone)]
pub struct ExecutionProbe(Arc<AtomicUsize>);

impl ExecutionProbe {
    pub fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    pub fn record_execution(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    pub fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedBinaryView {
    pub relative_path: String,
    pub digest: String,
    pub bytes: Vec<u8>,
}

/// Test-only wiring point. AGL-170 replaces these methods with calls to
/// StaticExtensionRegistry and runtime artifact composition. No selected
/// behavior may be implemented in this adapter.
pub struct ProductionRuntimeHarness {
    registry: agl_runtime::StaticExtensionRegistry<()>,
    _probe: ExecutionProbe,
}

fn production_key(key: &FactoryKey) -> Result<agl_runtime::StaticExtensionFactoryKey, String> {
    Ok(agl_runtime::StaticExtensionFactoryKey::new(
        agl_kernel::ExtensionId::new(&key.extension_id).map_err(|error| error.to_string())?,
        key.api_major,
        agl_kernel::DeclarationDigest::parse(&key.declaration_digest)
            .map_err(|error| error.to_string())?,
    ))
}

impl ProductionRuntimeHarness {
    pub fn new(probe: ExecutionProbe) -> Self {
        Self {
            registry: agl_runtime::StaticExtensionRegistry::new(),
            _probe: probe,
        }
    }

    pub fn register_factory(&mut self, key: FactoryKey) -> Result<(), String> {
        self.registry
            .register(production_key(&key)?, ())
            .map_err(|error| error.to_string())
    }

    pub fn resolve_factory(&self, key: &FactoryKey) -> Result<(), String> {
        self.registry
            .resolve(&production_key(key)?)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub fn contains_factory(&self, key: &FactoryKey) -> bool {
        production_key(key).is_ok_and(|key| self.registry.contains(&key))
    }

    pub fn compose_registration(
        &self,
        fixture: RegistrationFixture<'_>,
    ) -> Result<RegistrationView, String> {
        let key = production_key(&fixture.factory_key)?;
        self.registry
            .resolve(&key)
            .map_err(|error| error.to_string())?;
        if fixture.artifact_extension_id != fixture.factory_key.extension_id
            || fixture.artifact_digest != fixture.factory_key.declaration_digest
        {
            return Err("artifact identity does not match the static factory key".to_string());
        }
        let descriptor = fixture.declared_tools.iter().try_fold(
            agl_kernel::ExtensionDescriptor::new(
                agl_kernel::ExtensionId::new(fixture.artifact_extension_id)
                    .map_err(|error| error.to_string())?,
                "Runtime test Extension",
                "1.0.0",
                agl_kernel::ExtensionSource::TestFixture,
                agl_kernel::ExtensionTrust::TrustedRegistered,
            )
            .map_err(|error| error.to_string())?,
            |descriptor, id| {
                let declaration = agl_kernel::ToolDeclaration::new(
                    agl_kernel::ToolId::new(*id).map_err(|error| error.to_string())?,
                    "Runtime test Tool",
                    serde_json::json!({
                        "$schema": "https://json-schema.org/draft/2020-12/schema",
                        "type": "object",
                        "additionalProperties": false
                    }),
                    agl_kernel::OperationKind::Read,
                )
                .map_err(|error| error.to_string())?;
                Ok::<_, String>(descriptor.with_tool(declaration))
            },
        )?;
        let descriptor = fixture
            .declared_hooks
            .iter()
            .try_fold(descriptor, |descriptor, id| {
                let declaration = agl_kernel::HookDeclaration::new(
                    agl_kernel::HookId::new(*id).map_err(|error| error.to_string())?,
                    agl_kernel::HookEvent::ContextPrepare,
                );
                Ok::<_, String>(descriptor.with_hook(declaration))
            })?;
        agl_runtime::validate_static_bindings(
            &descriptor,
            &key,
            fixture
                .bound_tools
                .iter()
                .map(|id| agl_kernel::ToolId::new(*id).map_err(|error| error.to_string()))
                .collect::<Result<Vec<_>, _>>()?,
            fixture
                .bound_hooks
                .iter()
                .map(|id| agl_kernel::HookId::new(*id).map_err(|error| error.to_string()))
                .collect::<Result<Vec<_>, _>>()?,
        )
        .map_err(|error| error.to_string())?;
        Ok(RegistrationView {
            descriptor_extension_id: descriptor.id.to_string(),
            descriptor_digest: fixture.artifact_digest.to_string(),
            tool_ids: descriptor
                .tools
                .iter()
                .map(|tool| tool.id.to_string())
                .collect(),
            hook_ids: descriptor
                .hooks
                .iter()
                .map(|hook| hook.id.to_string())
                .collect(),
        })
    }

    pub fn verify_binary(
        &self,
        relative_path: &str,
        expected_digest: &str,
        bytes: &[u8],
    ) -> Result<VerifiedBinaryView, String> {
        let verified = agl_runtime::verify_extension_binary(relative_path, expected_digest, bytes)?;
        Ok(VerifiedBinaryView {
            relative_path: verified.relative_path().to_string(),
            digest: verified.digest().to_string(),
            bytes: verified.bytes().to_vec(),
        })
    }

    pub fn verify_optional_binary(
        &self,
        relative_path: &str,
        expected_digest: &str,
        bytes: Option<&[u8]>,
    ) -> Result<VerifiedBinaryView, String> {
        let verified =
            agl_runtime::verify_declared_extension_binary(relative_path, expected_digest, bytes)?;
        Ok(VerifiedBinaryView {
            relative_path: verified.relative_path().to_string(),
            digest: verified.digest().to_string(),
            bytes: verified.bytes().to_vec(),
        })
    }
}
