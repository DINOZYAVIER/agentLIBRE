use agl_capabilities::{
    ActionDeclaration, CapabilityId, OperationKind, ProviderDeclaration, ProviderId, StateEffect,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{ToolCatalog, ToolCatalogError};

pub const PROVIDER_ID: &str = "matrix-delivery-tools";
pub const MATRIX_OUTBOX_DELIVER_TOOL_ID: &str = "matrix.outbox.deliver";

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MatrixOutboxDeliverArgs {
    #[serde(default)]
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<usize>,
    #[serde(default)]
    pub dry_run: bool,
}

pub fn declaration() -> ProviderDeclaration {
    ProviderDeclaration::builtin(
        ProviderId::new(PROVIDER_ID).expect("builtin Matrix delivery provider ID is valid"),
        "Matrix Delivery Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin Matrix delivery provider declaration is valid")
    .with_action(
        ActionDeclaration::from_schema::<MatrixOutboxDeliverArgs>(
            CapabilityId::new(MATRIX_OUTBOX_DELIVER_TOOL_ID)
                .expect("builtin Matrix delivery capability ID is valid"),
            "Deliver queued Matrix notification outbox rows through the bridge-owned Matrix client.",
            OperationKind::Execute,
        )
        .expect("builtin Matrix delivery action schema is valid")
        .with_state_effects([StateEffect::MatrixOutbox]),
    )
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn delivery_schema_is_complete_and_closed() {
        let provider = declaration();
        provider.validate().unwrap();
        let action = &provider.actions[0];
        assert_eq!(action.input_schema["additionalProperties"], false);
        let schema = action.compile_schema().unwrap();
        schema
            .validate(&json!({"limit": 10, "dry_run": true}))
            .unwrap();
        assert!(schema.validate(&json!({"limit": 0})).is_err());
        assert!(schema.validate(&json!({"extra": true})).is_err());
        assert_eq!(action.state_effects, [StateEffect::MatrixOutbox].into());
    }
}
