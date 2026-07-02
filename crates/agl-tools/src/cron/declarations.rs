use anyhow::Result;

use crate::{
    ToolCapability, ToolCatalog, ToolCatalogError, ToolDeclaration, ToolId, ToolOperationKind,
    ToolProviderDeclaration, ToolProviderId, ToolStateEffect,
};

use super::{
    CRON_ADD_TOOL_ID, CRON_DELETE_TOOL_ID, CRON_DISABLE_TOOL_ID, CRON_ENABLE_TOOL_ID,
    CRON_HISTORY_TOOL_ID, CRON_LIST_TOOL_ID, CRON_PREFLIGHT_TOOL_ID, CRON_RUN_TOOL_ID,
    CRON_SHOW_TOOL_ID, CRON_TICK_TOOL_ID, CRON_UPDATE_TOOL_ID, PROVIDER_ID,
};

pub fn declaration() -> ToolProviderDeclaration {
    ToolProviderDeclaration::new(
        ToolProviderId::new(PROVIDER_ID).expect("builtin cron provider id is valid"),
        "Cron Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin cron provider declaration is valid")
    .with_tool(tool(
        CRON_LIST_TOOL_ID,
        "List cron jobs.",
        ToolCapability::Read,
        &[],
    ))
    .with_tool(tool(
        CRON_SHOW_TOOL_ID,
        "Show one cron job.",
        ToolCapability::Read,
        &["id"],
    ))
    .with_tool(tool(
        CRON_HISTORY_TOOL_ID,
        "Show recorded runs for one cron job.",
        ToolCapability::Read,
        &["job_id"],
    ))
    .with_tool(tool(
        CRON_PREFLIGHT_TOOL_ID,
        "Validate a cron job draft without writing it.",
        ToolCapability::Read,
        &["name", "target_kind", "target_ref", "schedule_expr"],
    ))
    .with_tool(tool(
        CRON_ADD_TOOL_ID,
        "Create a local cron job.",
        ToolCapability::Write,
        &["name", "target_kind", "target_ref", "schedule_expr"],
    ))
    .with_tool(tool(
        CRON_UPDATE_TOOL_ID,
        "Update a local cron job.",
        ToolCapability::Write,
        &["id"],
    ))
    .with_tool(tool(
        CRON_DELETE_TOOL_ID,
        "Tombstone a local cron job.",
        ToolCapability::Write,
        &["id"],
    ))
    .with_tool(tool(
        CRON_ENABLE_TOOL_ID,
        "Enable a local cron job.",
        ToolCapability::Write,
        &["id"],
    ))
    .with_tool(tool(
        CRON_DISABLE_TOOL_ID,
        "Disable a local cron job.",
        ToolCapability::Write,
        &["id"],
    ))
    .with_tool(
        tool(
            CRON_RUN_TOOL_ID,
            "Record a manual cron run for an exact job and optional scheduled timestamp.",
            ToolCapability::Write,
            &["job_id"],
        )
        .with_operation_kind(ToolOperationKind::Execute),
    )
    .with_tool(
        tool(
            CRON_TICK_TOOL_ID,
            "Run one local scheduler tick and enqueue Matrix notifications locally.",
            ToolCapability::Write,
            &[],
        )
        .with_operation_kind(ToolOperationKind::Execute)
        .with_state_effects([ToolStateEffect::StoreCron, ToolStateEffect::MatrixOutbox]),
    )
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn tool(
    id: &str,
    description: &str,
    capability: ToolCapability,
    required_arguments: &[&str],
) -> ToolDeclaration {
    let declaration = ToolDeclaration::new(
        ToolId::new(id).expect("builtin cron tool id is valid"),
        description,
        capability,
        required_arguments.iter().copied(),
    );
    match id {
        CRON_ADD_TOOL_ID | CRON_UPDATE_TOOL_ID | CRON_DELETE_TOOL_ID | CRON_ENABLE_TOOL_ID
        | CRON_DISABLE_TOOL_ID | CRON_RUN_TOOL_ID => {
            declaration.with_state_effects([ToolStateEffect::StoreCron])
        }
        _ => declaration,
    }
}
