use std::path::{Path, PathBuf};

use agl_store::{AglStore, MatrixNotificationOutboxDraft};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    ToolCapability, ToolCatalog, ToolCatalogError, ToolDeclaration, ToolHandler, ToolId, ToolInput,
    ToolOutput, ToolProviderDeclaration, ToolProviderId, ToolStateEffect,
};

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

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<String> {
        match name {
            MATRIX_OUTBOX_STATUS_TOOL_ID => self.status(arguments),
            MATRIX_OUTBOX_ENQUEUE_TOOL_ID => self.enqueue(arguments),
            _ => anyhow::bail!("unknown matrix tool `{name}`"),
        }
    }

    fn status(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<StatusArgs>(MATRIX_OUTBOX_STATUS_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let limit = args
            .limit
            .unwrap_or(DEFAULT_OUTBOX_LIMIT)
            .min(MAX_OUTBOX_LIMIT);
        let queued = store.queued_matrix_notifications(limit)?;
        let mut output = format!(
            "tool=matrix.outbox.status\nqueued={}\ntruncated={}\n---",
            queued.len(),
            queued.len() >= limit
        );
        for item in queued {
            output.push('\n');
            output.push_str(&format!(
                "notification id={} notify_ref={} source={}:{} status={} delivered={}",
                item.id,
                item.notify_ref,
                item.source_kind,
                item.source_id,
                item.status.as_str(),
                item.delivered_at.is_some()
            ));
        }
        Ok(output)
    }

    fn enqueue(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<EnqueueArgs>(MATRIX_OUTBOX_ENQUEUE_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let item = store.enqueue_matrix_notification(MatrixNotificationOutboxDraft::new(
            args.notify_ref,
            args.source_kind,
            args.source_id,
            args.dedupe_key,
            args.body,
        ))?;
        Ok(format!(
            "tool=matrix.outbox.enqueue\nnotification_id={}\nstatus={}\ndedupe_key={}",
            item.id,
            item.status.as_str(),
            item.dedupe_key
        ))
    }

    fn open_store(&self) -> Result<AglStore> {
        AglStore::open_at(&self.store_root).with_context(|| {
            format!(
                "failed to open Matrix outbox store {}",
                self.store_root.display()
            )
        })
    }
}

impl ToolHandler for MatrixTools {
    fn dispatch(&self, input: ToolInput) -> Result<ToolOutput> {
        let observation = self.dispatch(input.id.as_str(), input.arguments)?;
        Ok(ToolOutput { observation })
    }
}

pub fn declaration() -> ToolProviderDeclaration {
    ToolProviderDeclaration::new(
        ToolProviderId::new(PROVIDER_ID).expect("builtin Matrix provider id is valid"),
        "Matrix Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin Matrix provider declaration is valid")
    .with_tool(ToolDeclaration::new(
        ToolId::new(MATRIX_OUTBOX_STATUS_TOOL_ID).expect("builtin Matrix tool id is valid"),
        "Inspect queued local Matrix notification outbox rows.",
        ToolCapability::Read,
        std::iter::empty::<&str>(),
    ))
    .with_tool(
        ToolDeclaration::new(
            ToolId::new(MATRIX_OUTBOX_ENQUEUE_TOOL_ID).expect("builtin Matrix tool id is valid"),
            "Queue a Matrix notification in the local outbox without external delivery.",
            ToolCapability::Write,
            [
                "notify_ref",
                "source_kind",
                "source_id",
                "dedupe_key",
                "body",
            ],
        )
        .with_state_effects([ToolStateEffect::MatrixOutbox]),
    )
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn parse_args<T: for<'de> Deserialize<'de>>(tool: &str, arguments: Value) -> Result<T> {
    serde_json::from_value(arguments).with_context(|| format!("{tool} arguments are invalid"))
}

#[derive(Deserialize)]
struct StatusArgs {
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct EnqueueArgs {
    notify_ref: String,
    source_kind: String,
    source_id: String,
    dedupe_key: String,
    body: String,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use super::*;

    #[test]
    fn matrix_tools_enqueue_and_report_outbox_status() {
        let root = temp_root("outbox");
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

        assert!(enqueue.contains("status=queued"));
        assert!(status.contains("queued=1"));
        assert!(status.contains("notify_ref=matrix-room:!room:example.org"));

        cleanup(root);
    }

    fn temp_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "agl-matrix-tools-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn cleanup(root: PathBuf) {
        let _ = std::fs::remove_dir_all(root);
    }
}
