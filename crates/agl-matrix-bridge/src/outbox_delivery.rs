use std::path::{Path, PathBuf};

use agl_store::{AglStore, MatrixNotificationOutboxItem};
use agl_tools::{ToolHandler, ToolInput, ToolOutput};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_DELIVERY_LIMIT: usize = 10;
const MAX_DELIVERY_LIMIT: usize = 100;
pub const MATRIX_ROOM_NOTIFY_REF_PREFIX: &str = "matrix-room:";

pub trait MatrixOutboxTransport {
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
    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<String> {
        match name {
            agl_tools::MATRIX_OUTBOX_DELIVER_TOOL_ID => self.deliver(arguments),
            _ => anyhow::bail!("unknown Matrix outbox delivery tool `{name}`"),
        }
    }

    fn deliver(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<DeliverArgs>(agl_tools::MATRIX_OUTBOX_DELIVER_TOOL_ID, arguments)?;
        let limit = args
            .limit
            .unwrap_or(DEFAULT_DELIVERY_LIMIT)
            .min(MAX_DELIVERY_LIMIT);
        let dry_run = args.dry_run.unwrap_or(false);
        let store = if dry_run {
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
        let queued = store.queued_matrix_notifications(limit)?;
        let truncated = queued.len() >= limit;
        let mut sent = 0usize;
        let mut failed = 0usize;
        let mut output = format!(
            "tool=matrix.outbox.deliver\ndry_run={dry_run}\nlimit={limit}\nqueued={}\ntruncated={truncated}\n---",
            queued.len()
        );
        for item in queued {
            output.push('\n');
            if dry_run {
                output.push_str(&format!(
                    "notification id={} notify_ref={} action=would_deliver",
                    item.id, item.notify_ref
                ));
                continue;
            }
            match self.transport.deliver_notice(&item) {
                Ok(()) => {
                    let item = store.mark_matrix_notification_sent(&item.id)?;
                    sent += 1;
                    output.push_str(&format!(
                        "notification id={} notify_ref={} action=sent",
                        item.id, item.notify_ref
                    ));
                }
                Err(err) => {
                    let item = store.mark_matrix_notification_failed(&item.id, &err.to_string())?;
                    failed += 1;
                    output.push_str(&format!(
                        "notification id={} notify_ref={} action=failed error={}",
                        item.id,
                        item.notify_ref,
                        item.error.unwrap_or_default()
                    ));
                }
            }
        }
        output.push_str(&format!("\nsent={sent}\nfailed={failed}"));
        Ok(output)
    }
}

impl<T: MatrixOutboxTransport> ToolHandler for MatrixOutboxDeliveryTools<T> {
    fn dispatch(&self, input: ToolInput) -> Result<ToolOutput> {
        let observation = self.dispatch(input.id.as_str(), input.arguments)?;
        Ok(ToolOutput { observation })
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

fn parse_args<T: for<'de> Deserialize<'de>>(tool: &str, arguments: Value) -> Result<T> {
    serde_json::from_value(arguments).with_context(|| format!("{tool} arguments are invalid"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeliverArgs {
    limit: Option<usize>,
    dry_run: Option<bool>,
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
    fn delivery_tool_marks_sent_and_failed_items() {
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
            .dispatch(
                agl_tools::MATRIX_OUTBOX_DELIVER_TOOL_ID,
                json!({"limit": 10}),
            )
            .unwrap();

        let first = store.matrix_notification(&first.id).unwrap().unwrap();
        let second = store.matrix_notification(&second.id).unwrap().unwrap();
        assert!(output.contains("sent=1"));
        assert!(output.contains("failed=1"));
        assert_eq!(first.status.as_str(), "sent");
        assert_eq!(second.status.as_str(), "failed");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn dry_run_does_not_mark_items_delivered() {
        let root = temp_root("dry-run");
        AglStore::migrate_at(&root).unwrap();
        let store = AglStore::open_current_at(&root).unwrap();
        let item = store
            .enqueue_matrix_notification(draft("ok", "dry-run"))
            .unwrap();
        let tools = MatrixOutboxDeliveryTools::new(&root, TestTransport);

        let output = tools
            .dispatch(
                agl_tools::MATRIX_OUTBOX_DELIVER_TOOL_ID,
                json!({"dry_run": true}),
            )
            .unwrap();

        let item = store.matrix_notification(&item.id).unwrap().unwrap();
        assert!(output.contains("action=would_deliver"));
        assert_eq!(item.status.as_str(), "queued");

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
