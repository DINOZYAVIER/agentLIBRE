use agl_kernel::{
    EffectDeclaration, EffectId, ExtensionDescriptor, ExtensionId, OperationKind, ToolDeclaration,
    ToolId,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{ToolCatalog, ToolCatalogError};

pub const PROVIDER_ID: &str = "matrix.bridge";
pub const MATRIX_OUTBOX_DELIVER_TOOL_ID: &str = "matrix.bridge:outbox.deliver";

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MatrixOutboxDeliverArgs {
    #[serde(default)]
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<usize>,
    #[serde(default)]
    pub dry_run: bool,
}

pub fn declaration() -> ExtensionDescriptor {
    ExtensionDescriptor::builtin(
        ExtensionId::new(PROVIDER_ID).expect("builtin Matrix delivery extension ID is valid"),
        "Matrix Delivery Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin Matrix delivery extension declaration is valid")
    .with_tool(
        ToolDeclaration::from_schema::<MatrixOutboxDeliverArgs>(
            ToolId::new(MATRIX_OUTBOX_DELIVER_TOOL_ID)
                .expect("builtin Matrix delivery tool ID is valid"),
            "Deliver queued Matrix notification outbox rows through the bridge-owned Matrix client.",
            OperationKind::Execute,
        )
        .expect("builtin Matrix delivery action schema is valid")
        .with_state_effects([EffectId::matrix_outbox()]),
    )
    .with_effect(EffectDeclaration::for_standard(EffectId::matrix_outbox()).unwrap())
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
        let extension = declaration();
        extension.validate().unwrap();
        let action = &extension.tools[0];
        assert_eq!(action.input_schema["additionalProperties"], false);
        let schema = action.compile_schema().unwrap();
        schema
            .validate(&json!({"limit": 10, "dry_run": true}))
            .unwrap();
        assert!(schema.validate(&json!({"limit": 0})).is_err());
        assert!(schema.validate(&json!({"extra": true})).is_err());
        assert_eq!(action.state_effects, [EffectId::matrix_outbox()].into());
    }
}
