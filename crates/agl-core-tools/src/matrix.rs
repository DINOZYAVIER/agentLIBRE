use std::sync::Arc;

use agl_kernel::{
    EffectDeclaration, EffectId, ExtensionDescriptor, ExtensionId, OperationKind, ToolDeclaration,
    ToolDispatchContext, ToolHandler, ToolId, ToolResult,
};
use agl_matrix::{MatrixOutboxDraft, MatrixOutboxRepository};
use anyhow::Result;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::parse_tool_args as parse_args;

pub const EXTENSION_ID: &str = "matrix.outbox";
pub const MATRIX_OUTBOX_STATUS_TOOL_ID: &str = "matrix.outbox:status";
pub const MATRIX_OUTBOX_ENQUEUE_TOOL_ID: &str = "matrix.outbox:enqueue";

const DEFAULT_OUTBOX_LIMIT: usize = 10;
const MAX_OUTBOX_LIMIT: usize = 100;

#[derive(Clone)]
pub struct MatrixTools {
    repository: Arc<dyn MatrixOutboxRepository>,
}

impl MatrixTools {
    pub fn new(repository: Arc<dyn MatrixOutboxRepository>) -> Self {
        Self { repository }
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
        let limit = args
            .limit
            .unwrap_or(DEFAULT_OUTBOX_LIMIT)
            .clamp(1, MAX_OUTBOX_LIMIT);
        let mut queued = self.repository.queued_page(limit.saturating_add(1))?;
        let truncated = queued.len() > limit;
        queued.truncate(limit);
        let notifications = queued
            .into_iter()
            .map(|item| {
                json!({
                    "id": item.id,
                    "notify_ref": item.draft.notify_ref,
                    "source_kind": item.draft.source_kind,
                    "source_id": item.draft.source_id,
                    "status": item.state.as_str(),
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
        let draft = MatrixOutboxDraft::new(
            args.notify_ref,
            args.source_kind,
            args.source_id,
            args.dedupe_key,
            args.body,
        )?;
        let item = self.repository.enqueue(draft)?;
        Ok(json!({
            "tool": MATRIX_OUTBOX_ENQUEUE_TOOL_ID,
            "status": item.state.as_str(),
            "notification_id": item.id,
            "dedupe_key": item.draft.dedupe_key,
        }))
    }
}

impl ToolHandler for MatrixTools {
    fn dispatch(&self, context: ToolDispatchContext) -> agl_kernel::ToolHandlerFuture<'_> {
        Box::pin(async move {
            let invocation = context.into_invocation();
            self.dispatch(invocation.tool_id.as_str(), invocation.arguments)
                .map(ToolResult::new)
                .map_err(Into::into)
        })
    }
}

pub fn declaration() -> ExtensionDescriptor {
    ExtensionDescriptor::builtin(
        ExtensionId::new(EXTENSION_ID).expect("builtin Matrix extension id is valid"),
        "Matrix Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin Matrix extension declaration is valid")
    .with_tool(
        ToolDeclaration::from_schema::<StatusArgs>(
            ToolId::new(MATRIX_OUTBOX_STATUS_TOOL_ID).expect("builtin Matrix action id is valid"),
            "Inspect queued local Matrix notification outbox rows.",
            OperationKind::Read,
        )
        .expect("builtin Matrix status schema is valid"),
    )
    .with_tool(
        ToolDeclaration::from_schema::<EnqueueArgs>(
            ToolId::new(MATRIX_OUTBOX_ENQUEUE_TOOL_ID).expect("builtin Matrix action id is valid"),
            "Queue a Matrix notification in the local outbox without external delivery.",
            OperationKind::Write,
        )
        .expect("builtin Matrix enqueue schema is valid")
        .with_state_effects([EffectId::matrix_outbox()]),
    )
    .with_effect(EffectDeclaration::for_standard(EffectId::matrix_outbox()).unwrap())
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

    use crate::test_support::migrated_temp_store;

    use super::*;

    #[test]
    fn matrix_tools_enqueue_and_report_outbox_status() {
        let (_root, store) = migrated_temp_store("outbox");
        let tools = MatrixTools::new(store);

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
        let (_root, store) = migrated_temp_store("outbox-limit");
        let tools = MatrixTools::new(store);

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
            .tools
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
