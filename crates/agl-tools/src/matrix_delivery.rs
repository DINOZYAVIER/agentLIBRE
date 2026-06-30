use crate::{
    ToolCapability, ToolCatalog, ToolCatalogError, ToolDeclaration, ToolId, ToolOperationKind,
    ToolProviderDeclaration, ToolProviderId, ToolStateEffect,
};

pub const PROVIDER_ID: &str = "matrix-delivery-tools";
pub const MATRIX_OUTBOX_DELIVER_TOOL_ID: &str = "matrix.outbox.deliver";

pub fn declaration() -> ToolProviderDeclaration {
    ToolProviderDeclaration::new(
        ToolProviderId::new(PROVIDER_ID).expect("builtin Matrix delivery provider id is valid"),
        "Matrix Delivery Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin Matrix delivery provider declaration is valid")
    .with_tool(
        ToolDeclaration::new(
            ToolId::new(MATRIX_OUTBOX_DELIVER_TOOL_ID)
                .expect("builtin Matrix delivery tool id is valid"),
            "Deliver queued Matrix notification outbox rows through the bridge-owned Matrix client.",
            ToolCapability::Write,
            std::iter::empty::<&str>(),
        )
        .with_operation_kind(ToolOperationKind::Execute)
        .with_state_effects([ToolStateEffect::MatrixOutbox]),
    )
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}
