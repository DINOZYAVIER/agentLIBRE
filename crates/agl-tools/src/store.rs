use std::path::{Path, PathBuf};

use agl_store::{AglStore, StoreDomain, StoreExportOptions};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    ToolCapability, ToolCatalog, ToolCatalogError, ToolDeclaration, ToolHandler, ToolId, ToolInput,
    ToolOutput, ToolProviderDeclaration, ToolProviderId,
};

pub const PROVIDER_ID: &str = "store-tools";
pub const STORE_STATUS_TOOL_ID: &str = "store.status";
pub const STORE_EXPORT_TOOL_ID: &str = "store.export";

const DEFAULT_EXPORT_MAX_BYTES: usize = 16 * 1024;
const MAX_EXPORT_BYTES: usize = 128 * 1024;

#[derive(Clone, Debug)]
pub struct StoreTools {
    store_root: PathBuf,
}

impl StoreTools {
    pub fn new(store_root: impl AsRef<Path>) -> Self {
        Self {
            store_root: store_root.as_ref().to_path_buf(),
        }
    }

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<String> {
        match name {
            STORE_STATUS_TOOL_ID => self.status(arguments),
            STORE_EXPORT_TOOL_ID => self.export(arguments),
            _ => anyhow::bail!("unknown store tool `{name}`"),
        }
    }

    fn status(&self, arguments: Value) -> Result<String> {
        parse_args::<StatusArgs>(STORE_STATUS_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let status = store.status()?;
        let mut output = format!(
            "tool=store.status\nschema_version={}\ndatabase_path={}\nidempotency.in_progress={}\nidempotency.stale_in_progress={}\n---",
            status.schema_version,
            status.database_path.display(),
            status.idempotency.in_progress,
            status.idempotency.stale_in_progress.len()
        );
        for domain in status.domains {
            output.push('\n');
            output.push_str(&format!(
                "domain name={} status={} total_rows={} active_rows={}",
                domain.domain.as_str(),
                domain.status.as_str(),
                domain.total_rows,
                domain.active_rows
            ));
        }
        Ok(output)
    }

    fn export(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<ExportArgs>(STORE_EXPORT_TOOL_ID, arguments)?;
        let domain = parse_domain(&args.domain)?;
        let max_bytes = args
            .max_bytes
            .unwrap_or(DEFAULT_EXPORT_MAX_BYTES)
            .min(MAX_EXPORT_BYTES);
        let store = self.open_store()?;
        let mut bytes = Vec::new();
        let records = store.export_domain_jsonl(
            &StoreExportOptions {
                domain,
                include_deleted: args.include_deleted.unwrap_or(false),
            },
            &mut bytes,
        )?;
        let truncated = bytes.len() > max_bytes;
        if truncated {
            bytes.truncate(max_bytes);
            while !bytes.is_empty() && std::str::from_utf8(&bytes).is_err() {
                bytes.pop();
            }
        }
        let body = String::from_utf8(bytes).context("store export was not valid UTF-8")?;
        Ok(format!(
            "tool=store.export\ndomain={}\nrecords={records}\ntruncated={truncated}\nbytes={}\n---\n{}",
            domain.as_str(),
            body.len(),
            body
        ))
    }

    fn open_store(&self) -> Result<AglStore> {
        AglStore::open_at(&self.store_root)
            .with_context(|| format!("failed to open store {}", self.store_root.display()))
    }
}

impl ToolHandler for StoreTools {
    fn dispatch(&self, input: ToolInput) -> Result<ToolOutput> {
        let observation = self.dispatch(input.id.as_str(), input.arguments)?;
        Ok(ToolOutput { observation })
    }
}

pub fn declaration() -> ToolProviderDeclaration {
    ToolProviderDeclaration::new(
        ToolProviderId::new(PROVIDER_ID).expect("builtin store provider id is valid"),
        "Store Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin store provider declaration is valid")
    .with_tool(ToolDeclaration::new(
        ToolId::new(STORE_STATUS_TOOL_ID).expect("builtin store tool id is valid"),
        "Inspect store schema, domain health, and idempotency health.",
        ToolCapability::Read,
        std::iter::empty::<&str>(),
    ))
    .with_tool(ToolDeclaration::new(
        ToolId::new(STORE_EXPORT_TOOL_ID).expect("builtin store tool id is valid"),
        "Export one known store domain as bounded JSONL in the observation.",
        ToolCapability::Read,
        ["domain"],
    ))
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn parse_args<T: for<'de> Deserialize<'de>>(tool: &str, arguments: Value) -> Result<T> {
    serde_json::from_value(arguments).with_context(|| format!("{tool} arguments are invalid"))
}

fn parse_domain(value: &str) -> Result<StoreDomain> {
    match value {
        "memory" => Ok(StoreDomain::Memory),
        "notes" => Ok(StoreDomain::Notes),
        "cron" => Ok(StoreDomain::Cron),
        "permissions" => Ok(StoreDomain::Permissions),
        _ => anyhow::bail!("unknown store domain `{value}`"),
    }
}

#[derive(Deserialize)]
struct StatusArgs {}

#[derive(Deserialize)]
struct ExportArgs {
    domain: String,
    include_deleted: Option<bool>,
    max_bytes: Option<usize>,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use crate::memory::{MEMORY_ADD_TOOL_ID, MemoryTools};

    use super::*;

    #[test]
    fn store_tools_report_status_and_export_known_domains() {
        let root = temp_root("export");
        let memory = MemoryTools::new(&root);
        memory
            .dispatch(
                MEMORY_ADD_TOOL_ID,
                json!({
                    "scope": "user",
                    "kind": "fact",
                    "title": "Store export",
                    "body": "Exports are bounded JSONL."
                }),
            )
            .unwrap();

        let tools = StoreTools::new(&root);
        let status = tools.dispatch(STORE_STATUS_TOOL_ID, json!({})).unwrap();
        let export = tools
            .dispatch(
                STORE_EXPORT_TOOL_ID,
                json!({"domain": "memory", "max_bytes": 4096}),
            )
            .unwrap();

        assert!(status.contains("schema_version="));
        assert!(status.contains("domain name=memory"));
        assert!(export.contains("domain=memory"));
        assert!(export.contains("records=1"));
        assert!(export.contains("Store export"));

        cleanup(root);
    }

    fn temp_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "agl-store-tools-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn cleanup(root: PathBuf) {
        let _ = std::fs::remove_dir_all(root);
    }
}
