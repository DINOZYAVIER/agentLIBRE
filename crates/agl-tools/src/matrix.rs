use std::path::{Path, PathBuf};

use agl_capabilities::{
    ActionDeclaration, ActionHandler, ActionHandlerError, ActionInvocation, ActionResult,
    CapabilityId, OperationKind, ProviderDeclaration, ProviderId, StateEffect,
};
use agl_store::{AglStore, MatrixNotificationOutboxDraft};
use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ToolCatalog, ToolCatalogError, parse_action_args as parse_args};

pub const PROVIDER_ID: &str = "matrix-tools";
pub const MATRIX_OUTBOX_STATUS_TOOL_ID: &str = "matrix.outbox.status";
pub const MATRIX_OUTBOX_ENQUEUE_TOOL_ID: &str = "matrix.outbox.enqueue";

const DEFAULT_OUTBOX_LIMIT: usize = 10;
const MAX_OUTBOX_LIMIT: usize = 100;

#[derive(Clone, Debug)]
pub struct MatrixTools {
    store_root: PathBuf,
}

impl MatrixTools {
    pub fn new(store_root: impl AsRef<Path>) -> Self {
        Self {
            store_root: store_root.as_ref().to_path_buf(),
        }
    }

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<Value> {
        match name {
            MATRIX_OUTBOX_STATUS_TOOL_ID => self.status(arguments),
            MATRIX_OUTBOX_ENQUEUE_TOOL_ID => self.enqueue(arguments),
            _ => anyhow::bail!("unknown matrix tool `{name}`"),
        }
    }

    fn status(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<StatusArgs>(MATRIX_OUTBOX_STATUS_TOOL_ID, arguments)?;
        let store = self.open_store_read_only()?;
        let limit = args
            .limit
            .unwrap_or(DEFAULT_OUTBOX_LIMIT)
            .clamp(1, MAX_OUTBOX_LIMIT);
        let (queued, truncated) = store.queued_matrix_notifications_page(limit)?;
        let notifications = queued
            .into_iter()
            .map(|item| {
                json!({
                    "id": item.id,
                    "notify_ref": item.notify_ref,
                    "source_kind": item.source_kind,
                    "source_id": item.source_id,
                    "status": item.status.as_str(),
                    "delivered": item.delivered_at.is_some(),
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "tool": MATRIX_OUTBOX_STATUS_TOOL_ID,
            "status": "ok",
            "queued_count": notifications.len(),
            "truncated": truncated,
            "notifications": notifications,
        }))
    }

    fn enqueue(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<EnqueueArgs>(MATRIX_OUTBOX_ENQUEUE_TOOL_ID, arguments)?;
        let store = self.open_store_writable()?;
        let item = store.enqueue_matrix_notification(MatrixNotificationOutboxDraft::new(
            args.notify_ref,
            args.source_kind,
            args.source_id,
            args.dedupe_key,
            args.body,
        ))?;
        Ok(json!({
            "tool": MATRIX_OUTBOX_ENQUEUE_TOOL_ID,
            "status": item.status.as_str(),
            "notification_id": item.id,
            "dedupe_key": item.dedupe_key,
        }))
    }

    fn open_store_read_only(&self) -> Result<AglStore> {
        AglStore::open_current_read_only_at(&self.store_root).with_context(|| {
            format!(
                "failed to open Matrix outbox store {}",
                self.store_root.display()
            )
        })
    }

    fn open_store_writable(&self) -> Result<AglStore> {
        AglStore::open_current_at(&self.store_root).with_context(|| {
            format!(
                "failed to open Matrix outbox store {}",
                self.store_root.display()
            )
        })
    }
}

impl ActionHandler for MatrixTools {
    fn dispatch(&self, invocation: ActionInvocation) -> Result<ActionResult, ActionHandlerError> {
        self.dispatch(invocation.capability_id.as_str(), invocation.arguments)
            .map(ActionResult::new)
            .map_err(Into::into)
    }
}

pub fn declaration() -> ProviderDeclaration {
    ProviderDeclaration::builtin(
        ProviderId::new(PROVIDER_ID).expect("builtin Matrix provider id is valid"),
        "Matrix Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin Matrix provider declaration is valid")
    .with_action(
        ActionDeclaration::from_schema::<StatusArgs>(
            CapabilityId::new(MATRIX_OUTBOX_STATUS_TOOL_ID)
                .expect("builtin Matrix action id is valid"),
            "Inspect queued local Matrix notification outbox rows.",
            OperationKind::Read,
        )
        .expect("builtin Matrix status schema is valid"),
    )
    .with_action(
        ActionDeclaration::from_schema::<EnqueueArgs>(
            CapabilityId::new(MATRIX_OUTBOX_ENQUEUE_TOOL_ID)
                .expect("builtin Matrix action id is valid"),
            "Queue a Matrix notification in the local outbox without external delivery.",
            OperationKind::Write,
        )
        .expect("builtin Matrix enqueue schema is valid")
        .with_state_effects([StateEffect::MatrixOutbox]),
    )
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EnqueueArgs {
    notify_ref: String,
    source_kind: String,
    source_id: String,
    dedupe_key: String,
    body: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::test_support::migrated_temp_root;

    use super::*;

    #[test]
    fn matrix_tools_enqueue_and_report_outbox_status() {
        let root = migrated_temp_root("outbox");
        let tools = MatrixTools::new(&root);

        let enqueue = tools
            .dispatch(
                MATRIX_OUTBOX_ENQUEUE_TOOL_ID,
                json!({
                    "notify_ref": "matrix-room:!room:example.org",
                    "source_kind": "test",
                    "source_id": "source-1",
                    "dedupe_key": "test:source-1",
                    "body": "hello"
                }),
            )
            .unwrap();
        let status = tools
            .dispatch(MATRIX_OUTBOX_STATUS_TOOL_ID, json!({"limit": 10}))
            .unwrap();

        assert_eq!(enqueue["status"], "queued");
        assert_eq!(status["queued_count"], 1);
        assert_eq!(
            status["notifications"][0]["notify_ref"],
            "matrix-room:!room:example.org"
        );
    }

    #[test]
    fn matrix_tools_status_truncates_only_when_extra_rows_exist() {
        let root = migrated_temp_root("outbox-limit");
        let tools = MatrixTools::new(&root);

        for index in 0..2 {
            tools
                .dispatch(
                    MATRIX_OUTBOX_ENQUEUE_TOOL_ID,
                    json!({
                        "notify_ref": "matrix-room:!room:example.org",
                        "source_kind": "test",
                        "source_id": format!("source-{index}"),
                        "dedupe_key": format!("test:source-{index}"),
                        "body": "hello"
                    }),
                )
                .unwrap();
        }

        let exact = tools
            .dispatch(MATRIX_OUTBOX_STATUS_TOOL_ID, json!({"limit": 2}))
            .unwrap();
        let truncated = tools
            .dispatch(MATRIX_OUTBOX_STATUS_TOOL_ID, json!({"limit": 1}))
            .unwrap();

        assert_eq!(exact["queued_count"], 2);
        assert_eq!(exact["truncated"], false);
        assert_eq!(truncated["queued_count"], 1);
        assert_eq!(truncated["truncated"], true);
    }

    #[test]
    fn matrix_declarations_expose_closed_schemas() {
        let declaration = declaration();
        let enqueue = declaration
            .actions
            .iter()
            .find(|action| action.id.as_str() == MATRIX_OUTBOX_ENQUEUE_TOOL_ID)
            .unwrap();

        assert_eq!(enqueue.input_schema["additionalProperties"], false);
        assert_eq!(
            enqueue.input_schema["required"],
            json!([
                "notify_ref",
                "source_kind",
                "source_id",
                "dedupe_key",
                "body"
            ])
        );
        assert!(
            enqueue
                .compile_schema()
                .unwrap()
                .validate(&json!({
                    "notify_ref": "matrix-room:!room:example.org",
                    "source_kind": "test",
                    "source_id": "source-1",
                    "dedupe_key": "test:source-1",
                    "body": "hello",
                    "unexpected": true
                }))
                .is_err()
        );
    }
}
