use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};

use agl_extension::{
    Extension, ExtensionBindings, ExtensionDefinition, ExtensionHost, StaticExtensionFactory,
};
use agl_kernel::{ExtensionId, HostBindingId};

static BINDS: AtomicUsize = AtomicUsize::new(0);

struct EchoExtension;

impl Extension for EchoExtension {
    type BindError = Infallible;

    fn definition() -> ExtensionDefinition {
        ExtensionDefinition::builder(
            ExtensionId::new("example.echo").unwrap(),
            "Echo",
            "1.0.0",
            1,
        )
        .require_host_binding(HostBindingId::new("host.clock").unwrap(), 1)
        .build()
        .unwrap()
    }

    fn bind(host: &ExtensionHost) -> Result<ExtensionBindings, Self::BindError> {
        BINDS.fetch_add(1, Ordering::SeqCst);
        assert!(
            host.binding(&HostBindingId::new("host.clock").unwrap())
                .is_some()
        );
        Ok(ExtensionBindings::empty())
    }
}

// AGL171-001, AGL171-003, AGL171-004 and AGL171-011.
#[test]
fn author_factory_is_stateless_binding_only_and_keyed_from_definition() {
    let factory = StaticExtensionFactory::for_extension::<EchoExtension>();
    let definition = EchoExtension::definition();

    assert_eq!(factory.key().extension_id, definition.id);
    assert_eq!(factory.key().api_major, definition.api_major);
    assert_eq!(factory.key().declaration_digest, definition.digest());

    let host = ExtensionHost::builder()
        .binding(HostBindingId::new("host.clock").unwrap(), 1, ())
        .build();
    let bindings = factory.bind(&host).unwrap();
    assert!(bindings.tools().is_empty());
    assert!(bindings.hooks().is_empty());
    assert_eq!(BINDS.load(Ordering::SeqCst), 1);
}

// AGL171-001 and AGL171-011. This compile contract intentionally has no
// mutable ToolCatalog and no Session/Run/Turn/deadline/cancellation inputs.
#[test]
fn factory_cannot_publish_or_mutate_the_admitted_catalog() {
    fn selected_api(
        factory: &StaticExtensionFactory,
        host: &ExtensionHost,
    ) -> Result<ExtensionBindings, agl_extension::ErasedBindError> {
        factory.bind(host)
    }

    let _: fn(
        &StaticExtensionFactory,
        &ExtensionHost,
    ) -> Result<ExtensionBindings, agl_extension::ErasedBindError> = selected_api;
}

// AGL171-009. Hook invocation receives only the kernel invocation contract;
// resource mutation handles are not an invocation parameter.
#[test]
fn hook_binding_has_no_resource_mutation_context() {
    fn selected_api(
        binding: &agl_kernel::HookBinding,
        declaration: &agl_kernel::HookDeclaration,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, agl_kernel::HookInvocationError> {
        binding.invoke(declaration, payload)
    }

    let _: fn(
        &agl_kernel::HookBinding,
        &agl_kernel::HookDeclaration,
        serde_json::Value,
    ) -> Result<serde_json::Value, agl_kernel::HookInvocationError> = selected_api;
}
