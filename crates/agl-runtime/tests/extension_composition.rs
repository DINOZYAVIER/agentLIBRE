use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};

use agl_extension::{
    Extension, ExtensionBindings, ExtensionDefinition, ExtensionHost, StaticExtensionFactory,
};
use agl_kernel::{
    ExtensionId, ExtensionSource, ExtensionTrust, HostBindingId, OperationKind, ToolDeclaration,
    ToolId,
};
use agl_runtime::{
    ExtensionAvailability, ExtensionCompositionInput, ExtensionLoadError, StaticExtensionRegistry,
    compose_extension_catalog,
};
use serde_json::json;

static BINDS: AtomicUsize = AtomicUsize::new(0);

struct ClockExtension;

impl Extension for ClockExtension {
    type BindError = Infallible;

    fn definition() -> ExtensionDefinition {
        ExtensionDefinition::builder(
            ExtensionId::new("example.clock").unwrap(),
            "Clock",
            "1.0.0",
            1,
        )
        .require_host_binding(HostBindingId::new("host.clock").unwrap(), 1)
        .build()
        .unwrap()
    }

    fn bind(_host: &ExtensionHost) -> Result<ExtensionBindings, Self::BindError> {
        BINDS.fetch_add(1, Ordering::SeqCst);
        Ok(ExtensionBindings::empty())
    }
}

struct MissingToolExtension;

impl Extension for MissingToolExtension {
    type BindError = Infallible;

    fn definition() -> ExtensionDefinition {
        ExtensionDefinition::builder(
            ExtensionId::new("example.missing-tool").unwrap(),
            "Missing tool binding",
            "1.0.0",
            1,
        )
        .tool(
            ToolDeclaration::new(
                ToolId::new("example.missing-tool:run").unwrap(),
                "Run",
                json!({"type": "object", "additionalProperties": false}),
                OperationKind::Read,
            )
            .unwrap(),
        )
        .build()
        .unwrap()
    }

    fn bind(_host: &ExtensionHost) -> Result<ExtensionBindings, Self::BindError> {
        Ok(ExtensionBindings::empty())
    }
}

struct WrongMajorClockExtension;

impl Extension for WrongMajorClockExtension {
    type BindError = Infallible;

    fn definition() -> ExtensionDefinition {
        ExtensionDefinition::builder(
            ExtensionId::new("example.clock").unwrap(),
            "Clock",
            "1.0.0",
            2,
        )
        .build()
        .unwrap()
    }

    fn bind(_host: &ExtensionHost) -> Result<ExtensionBindings, Self::BindError> {
        Ok(ExtensionBindings::empty())
    }
}

fn package<E: Extension>() -> agl_extension::package::ExtensionPackage {
    agl_extension::package::ExtensionPackageBuilder::build_to_memory(E::definition()).unwrap()
}

fn input<E: Extension>(selected: bool, host: ExtensionHost) -> ExtensionCompositionInput {
    ExtensionCompositionInput::builder()
        .registry(
            StaticExtensionRegistry::from_factories([StaticExtensionFactory::for_extension::<E>()])
                .unwrap(),
        )
        .package(package::<E>())
        .selected(E::definition().id, selected)
        .runtime_identity(ExtensionSource::Builtin, ExtensionTrust::TrustedRegistered)
        .host(host)
        .build()
        .unwrap()
}

// AGL171-003, AGL171-004 and AGL171-011.
#[test]
fn production_composer_matches_exact_factory_key_and_binds_once_per_generation() {
    BINDS.store(0, Ordering::SeqCst);
    let host = ExtensionHost::builder()
        .binding(HostBindingId::new("host.clock").unwrap(), 1, ())
        .build();
    let composed = compose_extension_catalog(input::<ClockExtension>(true, host)).unwrap();

    assert_eq!(BINDS.load(Ordering::SeqCst), 1);
    assert_eq!(composed.admitted().len(), 1);
    assert_eq!(composed.admitted()[0].id.as_str(), "example.clock");

    let error = compose_extension_catalog(
        ExtensionCompositionInput::builder()
            .registry(
                StaticExtensionRegistry::from_factories([StaticExtensionFactory::for_extension::<
                    ClockExtension,
                >()])
                .unwrap(),
            )
            .package(package::<WrongMajorClockExtension>())
            .selected(ExtensionId::new("example.clock").unwrap(), true)
            .host(ExtensionHost::empty())
            .build()
            .unwrap(),
    )
    .unwrap_err();
    assert!(
        matches!(error, ExtensionLoadError::FactoryKeyMismatch { extension_id, .. } if extension_id == ExtensionId::new("example.clock").unwrap())
    );
}

// AGL171-012 and AGL171-013.
#[test]
fn missing_required_host_binding_is_error_only_when_selected() {
    let selected = compose_extension_catalog(input::<ClockExtension>(true, ExtensionHost::empty()))
        .unwrap_err();
    assert!(
        matches!(selected, ExtensionLoadError::MissingHostBinding { extension_id, binding_id, required_api_major: 1 } if extension_id == ExtensionId::new("example.clock").unwrap() && binding_id == HostBindingId::new("host.clock").unwrap())
    );

    let unselected =
        compose_extension_catalog(input::<ClockExtension>(false, ExtensionHost::empty())).unwrap();
    assert!(unselected.admitted().is_empty());
    assert_eq!(
        unselected
            .query()
            .get(&ExtensionId::new("example.clock").unwrap())
            .unwrap()
            .availability,
        ExtensionAvailability::Unavailable
    );
}

// AGL171-003 and AGL171-013.
#[test]
fn exact_binding_set_is_checked_before_kernel_registration() {
    let error =
        compose_extension_catalog(input::<MissingToolExtension>(true, ExtensionHost::empty()))
            .unwrap_err();
    assert!(
        matches!(error, ExtensionLoadError::MissingToolBinding { extension_id, tool_id } if extension_id == ExtensionId::new("example.missing-tool").unwrap() && tool_id == ToolId::new("example.missing-tool:run").unwrap())
    );
}

// AGL171-007 and AGL171-014.
#[test]
fn turn_snapshot_is_frozen_and_catalog_digest_includes_runtime_identity() {
    let host = ExtensionHost::builder()
        .binding(HostBindingId::new("host.clock").unwrap(), 1, ())
        .build();
    let first = compose_extension_catalog(input::<ClockExtension>(true, host.clone())).unwrap();
    let turn = first.snapshot_for_turn();

    let second = compose_extension_catalog(
        input::<ClockExtension>(true, host)
            .with_runtime_identity(ExtensionSource::TestFixture, ExtensionTrust::Changed),
    )
    .unwrap();
    assert_eq!(
        turn.extension_ids(),
        [ExtensionId::new("example.clock").unwrap()]
    );
    assert_ne!(turn.catalog_digest(), second.catalog_digest());
    assert_eq!(
        turn.extensions()[0].declaration_digest,
        second.admitted()[0].declaration_digest
    );
}
