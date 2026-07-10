use agl_capabilities::{
    ActionDeclaration, CapabilityId, OperationKind, ProviderDeclaration, ProviderId, StateEffect,
};
use schemars::JsonSchema;

use crate::{ToolCatalog, ToolCatalogError};

use super::{
    CRON_ADD_TOOL_ID, CRON_DELETE_TOOL_ID, CRON_DISABLE_TOOL_ID, CRON_ENABLE_TOOL_ID,
    CRON_HISTORY_TOOL_ID, CRON_LIST_TOOL_ID, CRON_PREFLIGHT_TOOL_ID, CRON_RUN_TOOL_ID,
    CRON_SHOW_TOOL_ID, CRON_TICK_TOOL_ID, CRON_UPDATE_TOOL_ID, HistoryArgs, IdArgs, JobDraftArgs,
    ListArgs, PROVIDER_ID, RunArgs, TickArgs, UpdateArgs,
};

pub fn declaration() -> ProviderDeclaration {
    ProviderDeclaration::builtin(
        ProviderId::new(PROVIDER_ID).expect("builtin cron provider id is valid"),
        "Cron Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin cron provider declaration is valid")
    .with_action(action::<ListArgs>(
        CRON_LIST_TOOL_ID,
        "List cron jobs.",
        OperationKind::Read,
    ))
    .with_action(action::<IdArgs>(
        CRON_SHOW_TOOL_ID,
        "Show one cron job.",
        OperationKind::Read,
    ))
    .with_action(action::<HistoryArgs>(
        CRON_HISTORY_TOOL_ID,
        "Show recorded runs for one cron job.",
        OperationKind::Read,
    ))
    .with_action(action::<JobDraftArgs>(
        CRON_PREFLIGHT_TOOL_ID,
        "Validate a cron job draft without writing it.",
        OperationKind::Read,
    ))
    .with_action(
        action::<JobDraftArgs>(
            CRON_ADD_TOOL_ID,
            "Create a local cron job.",
            OperationKind::Write,
        )
        .with_state_effects([StateEffect::StoreCron]),
    )
    .with_action(
        action::<UpdateArgs>(
            CRON_UPDATE_TOOL_ID,
            "Update a local cron job.",
            OperationKind::Write,
        )
        .with_state_effects([StateEffect::StoreCron]),
    )
    .with_action(
        action::<IdArgs>(
            CRON_DELETE_TOOL_ID,
            "Tombstone a local cron job.",
            OperationKind::Write,
        )
        .with_state_effects([StateEffect::StoreCron]),
    )
    .with_action(
        action::<IdArgs>(
            CRON_ENABLE_TOOL_ID,
            "Enable a local cron job.",
            OperationKind::Write,
        )
        .with_state_effects([StateEffect::StoreCron]),
    )
    .with_action(
        action::<IdArgs>(
            CRON_DISABLE_TOOL_ID,
            "Disable a local cron job.",
            OperationKind::Write,
        )
        .with_state_effects([StateEffect::StoreCron]),
    )
    .with_action(
        action::<RunArgs>(
            CRON_RUN_TOOL_ID,
            "Record a manual cron run for an exact job and optional scheduled timestamp.",
            OperationKind::Execute,
        )
        .with_state_effects([StateEffect::StoreCron, StateEffect::StoreIdempotency]),
    )
    .with_action(
        action::<TickArgs>(
            CRON_TICK_TOOL_ID,
            "Run one local scheduler tick and enqueue Matrix notifications locally.",
            OperationKind::Execute,
        )
        .with_state_effects([
            StateEffect::StoreCron,
            StateEffect::StoreIdempotency,
            StateEffect::MatrixOutbox,
        ]),
    )
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn action<T: JsonSchema>(
    id: &str,
    description: &str,
    operation_kind: OperationKind,
) -> ActionDeclaration {
    ActionDeclaration::from_schema::<T>(
        CapabilityId::new(id).expect("builtin cron action id is valid"),
        description,
        operation_kind,
    )
    .expect("builtin cron action schema is valid")
}
