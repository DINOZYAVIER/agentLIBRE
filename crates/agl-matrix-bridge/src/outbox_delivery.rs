use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use agl_core_tools::matrix_delivery::MatrixOutboxDeliverArgs;
use agl_kernel::{ToolDispatchContext, ToolHandler, ToolId, ToolResult};
use agl_matrix::{
    MatrixDeliveryResult, MatrixOperationId, MatrixOutboxRecord, MatrixOutboxRepository,
    MatrixOutboxState,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

const DEFAULT_DELIVERY_LIMIT: usize = 10;
const MAX_DELIVERY_LIMIT: usize = 100;
pub const MATRIX_ROOM_NOTIFY_REF_PREFIX: &str = "matrix-room:";

pub trait MatrixOutboxTransport: Send + Sync {
    fn deliver_notice(&self, notification: &MatrixOutboxRecord) -> MatrixDeliveryResult;
}

#[derive(Clone)]
pub struct MatrixOutboxDeliveryTools<T> {
    repository: Arc<dyn MatrixOutboxRepository>,
    transport: T,
}

impl<T> MatrixOutboxDeliveryTools<T> {
    pub fn new(repository: Arc<dyn MatrixOutboxRepository>, transport: T) -> Self {
        Self {
            repository,
            transport,
        }
    }
}

impl<T: MatrixOutboxTransport> MatrixOutboxDeliveryTools<T> {
    fn dispatch_action(&self, id: &ToolId, arguments: Value) -> Result<ToolResult> {
        ensure!(
            id.as_str() == agl_core_tools::MATRIX_OUTBOX_DELIVER_TOOL_ID,
            "unknown Matrix outbox delivery tool `{id}`"
        );
        let args =
            serde_json::from_value::<MatrixOutboxDeliverArgs>(arguments).with_context(|| {
                format!(
                    "{} arguments are invalid",
                    agl_core_tools::MATRIX_OUTBOX_DELIVER_TOOL_ID
                )
            })?;
        Ok(ToolResult::new(self.deliver(args)?))
    }

    fn deliver(&self, args: MatrixOutboxDeliverArgs) -> Result<Value> {
        let limit = args
            .limit
            .unwrap_or(DEFAULT_DELIVERY_LIMIT)
            .clamp(1, MAX_DELIVERY_LIMIT);
        let now_ms = current_unix_millis();
        if !args.dry_run {
            self.repository.recover_expired(now_ms, limit)?;
        }
        let mut queued = self.repository.queued(now_ms, limit.saturating_add(1))?;
        let truncated = queued.len() > limit;
        queued.truncate(limit);
        let queued_count = queued.len();
        let mut sent = 0usize;
        let mut failed = 0usize;
        let mut retried = 0usize;
        let mut deliveries = Vec::with_capacity(queued_count);
        for item in queued {
            if args.dry_run {
                deliveries.push(json!({
                    "id": item.id,
                    "notify_ref": item.draft.notify_ref,
                    "transaction_id": item.transaction_id,
                    "status": "would_deliver",
                }));
                continue;
            }
            let claim = self.repository.claim(
                &item.id,
                operation_id("claim", &item),
                "matrix-delivery-tool",
                now_ms,
                now_ms.saturating_add(60_000),
            )?;
            let result = self.transport.deliver_notice(&claim);
            let completed = self.repository.complete(
                &claim.id,
                operation_id("complete", &claim),
                "matrix-delivery-tool",
                result,
            )?;
            match &completed.state {
                MatrixOutboxState::Sent => sent += 1,
                MatrixOutboxState::Failed { .. } => failed += 1,
                MatrixOutboxState::Queued { .. } => retried += 1,
                MatrixOutboxState::Delivering { .. } => {
                    anyhow::bail!("Matrix completion left item in delivering state")
                }
            }
            deliveries.push(json!({
                "id": completed.id,
                "notify_ref": completed.draft.notify_ref,
                "transaction_id": completed.transaction_id,
                "status": completed.state.as_str(),
                "error": completed.last_error,
            }));
        }
        Ok(json!({
            "tool_id": agl_core_tools::MATRIX_OUTBOX_DELIVER_TOOL_ID,
            "dry_run": args.dry_run,
            "limit": limit,
            "queued": queued_count,
            "truncated": truncated,
            "deliveries": deliveries,
            "sent": sent,
            "failed": failed,
            "retried": retried,
        }))
    }
}

pub(crate) fn operation_id(prefix: &str, item: &MatrixOutboxRecord) -> MatrixOperationId {
    MatrixOperationId::new(format!("{prefix}:{}:{}", item.id, item.revision.get()))
        .expect("stored Matrix identity yields a bounded operation ID")
}

pub(crate) fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

impl<T: MatrixOutboxTransport> ToolHandler for MatrixOutboxDeliveryTools<T> {
    fn dispatch(&self, context: ToolDispatchContext) -> agl_kernel::ToolHandlerFuture<'_> {
        Box::pin(async move {
            let invocation = context.into_invocation();
            self.dispatch_action(&invocation.tool_id, invocation.arguments)
                .map_err(Into::into)
        })
    }
}

pub fn parse_matrix_room_notify_ref(notify_ref: &str) -> Result<&str> {
    let room = notify_ref
        .strip_prefix(MATRIX_ROOM_NOTIFY_REF_PREFIX)
        .with_context(|| {
            format!(
                "unsupported Matrix notify_ref `{notify_ref}`; expected {MATRIX_ROOM_NOTIFY_REF_PREFIX}<room-id>"
            )
        })?;
    ensure!(
        !room.trim().is_empty(),
        "Matrix notify_ref room id is empty"
    );
    Ok(room)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use agl_matrix::{MatrixOutboxDraft, MatrixOutboxRepository};
    use serde_json::json;

    use super::*;

    #[derive(Clone)]
    struct TestTransport;

    impl MatrixOutboxTransport for TestTransport {
        fn deliver_notice(&self, notification: &MatrixOutboxRecord) -> MatrixDeliveryResult {
            if notification.draft.source_id == "fail" {
                MatrixDeliveryResult::Permanent {
                    error: "simulated delivery failure".to_owned(),
                }
            } else {
                MatrixDeliveryResult::Delivered
            }
        }
    }

    #[test]
    fn delivery_action_returns_structured_sent_and_failed_items() {
        let root = temp_root("deliver");
        let store = Arc::new(agl_store::StoreHandle::open_at(&root).unwrap());
        let first = store.enqueue(draft("ok", "first")).unwrap();
        let second = store.enqueue(draft("fail", "second")).unwrap();
        let tools = MatrixOutboxDeliveryTools::new(store.clone(), TestTransport);

        let output = tools
            .dispatch_action(
                &ToolId::new(agl_core_tools::MATRIX_OUTBOX_DELIVER_TOOL_ID).unwrap(),
                json!({"limit": 10}),
            )
            .unwrap();

        let first = store.get(&first.id).unwrap().unwrap();
        let second = store.get(&second.id).unwrap().unwrap();
        assert_eq!(output.data["sent"], 1);
        assert_eq!(output.data["failed"], 1);
        let deliveries = output.data["deliveries"].as_array().unwrap();
        assert_eq!(deliveries[0]["status"], "sent");
        assert_eq!(deliveries[1]["status"], "failed");
        assert!(deliveries[1]["error"].is_string());
        assert_eq!(first.state.as_str(), "sent");
        assert_eq!(second.state.as_str(), "failed");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn dry_run_returns_would_deliver_without_mutation() {
        let root = temp_root("dry-run");
        let store = Arc::new(agl_store::StoreHandle::open_at(&root).unwrap());
        let item = store.enqueue(draft("ok", "dry-run")).unwrap();
        let tools = MatrixOutboxDeliveryTools::new(store.clone(), TestTransport);

        let output = tools
            .dispatch_action(
                &ToolId::new(agl_core_tools::MATRIX_OUTBOX_DELIVER_TOOL_ID).unwrap(),
                json!({"dry_run": true}),
            )
            .unwrap();

        let item = store.get(&item.id).unwrap().unwrap();
        assert_eq!(output.data["deliveries"][0]["status"], "would_deliver");
        assert_eq!(output.data["sent"], 0);
        assert_eq!(item.state.as_str(), "queued");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn handler_rejects_unknown_argument_fields() {
        let root = temp_root("unknown");
        let store = Arc::new(agl_store::StoreHandle::open_at(&root).unwrap());
        let tools = MatrixOutboxDeliveryTools::new(store, TestTransport);
        let error = tools
            .dispatch_action(
                &ToolId::new(agl_core_tools::MATRIX_OUTBOX_DELIVER_TOOL_ID).unwrap(),
                json!({"unknown": true}),
            )
            .unwrap_err();
        assert!(error.to_string().contains("arguments are invalid"));
        let _ = std::fs::remove_dir_all(root);
    }

    fn draft(source_id: &str, dedupe: &str) -> MatrixOutboxDraft {
        MatrixOutboxDraft::new(
            "matrix-room:!room:example.org",
            "test",
            source_id,
            dedupe,
            "hello",
        )
        .unwrap()
    }

    fn temp_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "agl-matrix-outbox-delivery-{label}-{}-{nanos}",
            std::process::id()
        ))
    }
}
