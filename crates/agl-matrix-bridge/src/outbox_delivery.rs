use std::path::{Path, PathBuf};

use agl_capabilities::{
    ActionHandler, ActionHandlerError, ActionInvocation, ActionResult, CapabilityId,
};
use agl_store::{AglStore, MatrixNotificationOutboxItem};
use agl_tools::matrix_delivery::MatrixOutboxDeliverArgs;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

const DEFAULT_DELIVERY_LIMIT: usize = 10;
const MAX_DELIVERY_LIMIT: usize = 100;
pub const MATRIX_ROOM_NOTIFY_REF_PREFIX: &str = "matrix-room:";

pub trait MatrixOutboxTransport: Send + Sync {
    fn deliver_notice(&self, notification: &MatrixNotificationOutboxItem) -> Result<()>;
}

#[derive(Clone, Debug)]
pub struct MatrixOutboxDeliveryTools<T> {
    store_root: PathBuf,
    transport: T,
}

impl<T> MatrixOutboxDeliveryTools<T> {
    pub fn new(store_root: impl AsRef<Path>, transport: T) -> Self {
        Self {
            store_root: store_root.as_ref().to_path_buf(),
            transport,
        }
    }
}

impl<T: MatrixOutboxTransport> MatrixOutboxDeliveryTools<T> {
    fn dispatch_action(&self, id: &CapabilityId, arguments: Value) -> Result<ActionResult> {
        ensure!(
            id.as_str() == agl_tools::MATRIX_OUTBOX_DELIVER_TOOL_ID,
            "unknown Matrix outbox delivery capability `{id}`"
        );
        let args =
            serde_json::from_value::<MatrixOutboxDeliverArgs>(arguments).with_context(|| {
                format!(
                    "{} arguments are invalid",
                    agl_tools::MATRIX_OUTBOX_DELIVER_TOOL_ID
                )
            })?;
        Ok(ActionResult::new(self.deliver(args)?))
    }

    fn deliver(&self, args: MatrixOutboxDeliverArgs) -> Result<Value> {
        let limit = args
            .limit
            .unwrap_or(DEFAULT_DELIVERY_LIMIT)
            .clamp(1, MAX_DELIVERY_LIMIT);
        let store = if args.dry_run {
            AglStore::open_current_read_only_at(&self.store_root)
        } else {
            AglStore::open_current_at(&self.store_root)
        }
        .with_context(|| {
            format!(
                "failed to open current Matrix outbox store {}",
                self.store_root.display()
            )
        })?;
        let (queued, truncated) = store.queued_matrix_notifications_page(limit)?;
        let queued_count = queued.len();
        let mut sent = 0usize;
        let mut failed = 0usize;
        let mut deliveries = Vec::with_capacity(queued_count);
        for item in queued {
            if args.dry_run {
                deliveries.push(json!({
                    "id": item.id,
                    "notify_ref": item.notify_ref,
                    "status": "would_deliver",
                }));
                continue;
            }
            match self.transport.deliver_notice(&item) {
                Ok(()) => {
                    let item = store.mark_matrix_notification_sent(&item.id)?;
                    sent += 1;
                    deliveries.push(json!({
                        "id": item.id,
                        "notify_ref": item.notify_ref,
                        "status": "sent",
                    }));
                }
                Err(error) => {
                    let item =
                        store.mark_matrix_notification_failed(&item.id, &error.to_string())?;
                    failed += 1;
                    deliveries.push(json!({
                        "id": item.id,
                        "notify_ref": item.notify_ref,
                        "status": "failed",
                        "error": item.error,
                    }));
                }
            }
        }
        Ok(json!({
            "capability_id": agl_tools::MATRIX_OUTBOX_DELIVER_TOOL_ID,
            "dry_run": args.dry_run,
            "limit": limit,
            "queued": queued_count,
            "truncated": truncated,
            "deliveries": deliveries,
            "sent": sent,
            "failed": failed,
        }))
    }
}

impl<T: MatrixOutboxTransport> ActionHandler for MatrixOutboxDeliveryTools<T> {
    fn dispatch(&self, invocation: ActionInvocation) -> Result<ActionResult, ActionHandlerError> {
        self.dispatch_action(&invocation.capability_id, invocation.arguments)
            .map_err(Into::into)
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
    use std::time::{SystemTime, UNIX_EPOCH};

    use agl_store::MatrixNotificationOutboxDraft;
    use serde_json::json;

    use super::*;

    #[derive(Clone)]
    struct TestTransport;

    impl MatrixOutboxTransport for TestTransport {
        fn deliver_notice(&self, notification: &MatrixNotificationOutboxItem) -> Result<()> {
            if notification.source_id == "fail" {
                anyhow::bail!("simulated delivery failure");
            }
            Ok(())
        }
    }

    #[test]
    fn delivery_action_returns_structured_sent_and_failed_items() {
        let root = temp_root("deliver");
        let store = AglStore::migrate_at(&root).unwrap();
        assert_eq!(
            store.after_schema_version,
            agl_store::CURRENT_SCHEMA_VERSION
        );
        let store = AglStore::open_current_at(&root).unwrap();
        let first = store
            .enqueue_matrix_notification(draft("ok", "first"))
            .unwrap();
        let second = store
            .enqueue_matrix_notification(draft("fail", "second"))
            .unwrap();
        let tools = MatrixOutboxDeliveryTools::new(&root, TestTransport);

        let output = tools
            .dispatch_action(
                &CapabilityId::new(agl_tools::MATRIX_OUTBOX_DELIVER_TOOL_ID).unwrap(),
                json!({"limit": 10}),
            )
            .unwrap();

        let first = store.matrix_notification(&first.id).unwrap().unwrap();
        let second = store.matrix_notification(&second.id).unwrap().unwrap();
        assert_eq!(output.data["sent"], 1);
        assert_eq!(output.data["failed"], 1);
        let deliveries = output.data["deliveries"].as_array().unwrap();
        assert_eq!(deliveries[0]["status"], "sent");
        assert_eq!(deliveries[1]["status"], "failed");
        assert!(deliveries[1]["error"].is_string());
        assert_eq!(first.status.as_str(), "sent");
        assert_eq!(second.status.as_str(), "failed");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn dry_run_returns_would_deliver_without_mutation() {
        let root = temp_root("dry-run");
        AglStore::migrate_at(&root).unwrap();
        let store = AglStore::open_current_at(&root).unwrap();
        let item = store
            .enqueue_matrix_notification(draft("ok", "dry-run"))
            .unwrap();
        let tools = MatrixOutboxDeliveryTools::new(&root, TestTransport);

        let output = tools
            .dispatch_action(
                &CapabilityId::new(agl_tools::MATRIX_OUTBOX_DELIVER_TOOL_ID).unwrap(),
                json!({"dry_run": true}),
            )
            .unwrap();

        let item = store.matrix_notification(&item.id).unwrap().unwrap();
        assert_eq!(output.data["deliveries"][0]["status"], "would_deliver");
        assert_eq!(output.data["sent"], 0);
        assert_eq!(item.status.as_str(), "queued");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn handler_rejects_unknown_argument_fields() {
        let root = temp_root("unknown");
        AglStore::migrate_at(&root).unwrap();
        let tools = MatrixOutboxDeliveryTools::new(&root, TestTransport);
        let error = tools
            .dispatch_action(
                &CapabilityId::new(agl_tools::MATRIX_OUTBOX_DELIVER_TOOL_ID).unwrap(),
                json!({"unknown": true}),
            )
            .unwrap_err();
        assert!(error.to_string().contains("arguments are invalid"));
        let _ = std::fs::remove_dir_all(root);
    }

    fn draft(source_id: &str, dedupe: &str) -> MatrixNotificationOutboxDraft {
        MatrixNotificationOutboxDraft::new(
            "matrix-room:!room:example.org",
            "test",
            source_id,
            dedupe,
            "hello",
        )
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
