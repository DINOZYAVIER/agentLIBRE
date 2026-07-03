use std::path::{Path, PathBuf};

use agl_store::{AglStore, StoreDomain, StoreExportOptions};
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    ToolCapability, ToolCatalog, ToolCatalogError, ToolDeclaration, ToolHandler, ToolId, ToolInput,
    ToolOperationKind, ToolOutput, ToolProviderDeclaration, ToolProviderId, ToolStateEffect,
    parse_tool_args as parse_args,
};

pub const PROVIDER_ID: &str = "store-tools";
pub const STORE_STATUS_TOOL_ID: &str = "store.status";
pub const STORE_EXPORT_TOOL_ID: &str = "store.export";
pub const STORE_MIGRATE_TOOL_ID: &str = "store.migrate";

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
            STORE_MIGRATE_TOOL_ID => self.migrate(arguments),
            _ => anyhow::bail!("unknown store tool `{name}`"),
        }
    }

    fn status(&self, arguments: Value) -> Result<String> {
        parse_args::<StatusArgs>(STORE_STATUS_TOOL_ID, arguments)?;
        let schema = AglStore::schema_status_at(&self.store_root)?;
        let mut output = format!(
            "tool=store.status\nschema_version={}\ncurrent_schema_version={}\ndatabase_path={}\ndatabase_exists={}\nmigration_required={}\napplied_migrations={}\n---",
            schema
                .schema_version
                .map(|version| version.to_string())
                .unwrap_or_else(|| "none".to_string()),
            schema.current_schema_version,
            schema.database_path.display(),
            schema.database_exists,
            schema.migration_required,
            schema
                .applied_migrations
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );
        if !schema.migration_required {
            let store = self.open_current_read_only_store()?;
            let status = store.status()?;
            output.push_str(&format!(
                "\nidempotency.in_progress={}\nidempotency.stale_in_progress={}",
                status.idempotency.in_progress,
                status.idempotency.stale_in_progress.len()
            ));
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
        let store = self.open_current_read_only_store()?;
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

    fn migrate(&self, arguments: Value) -> Result<String> {
        parse_args::<MigrateArgs>(STORE_MIGRATE_TOOL_ID, arguments)?;
        let report = AglStore::migrate_at(&self.store_root)
            .with_context(|| format!("failed to migrate store {}", self.store_root.display()))?;
        Ok(format!(
            "tool=store.migrate\ndatabase_path={}\nbefore_schema_version={}\nafter_schema_version={}\napplied_migrations={}\nstatus=ok",
            report.database_path.display(),
            report.before_schema_version,
            report.after_schema_version,
            report
                .applied_migrations
                .iter()
                .map(|migration| format!("{}:{}", migration.version, migration.name))
                .collect::<Vec<_>>()
                .join(",")
        ))
    }

    fn open_current_read_only_store(&self) -> Result<AglStore> {
        AglStore::open_current_read_only_at(&self.store_root).with_context(|| {
            format!(
                "failed to open current read-only store {}",
                self.store_root.display()
            )
        })
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
    .with_tool(
        ToolDeclaration::new(
            ToolId::new(STORE_MIGRATE_TOOL_ID).expect("builtin store tool id is valid"),
            "Run AgentLIBRE store migrations through an explicit admin boundary.",
            ToolCapability::Write,
            std::iter::empty::<&str>(),
        )
        .with_operation_kind(ToolOperationKind::Admin)
        .with_state_effects([ToolStateEffect::StoreSchema]),
    )
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
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
#[serde(deny_unknown_fields)]
struct StatusArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportArgs {
    domain: String,
    include_deleted: Option<bool>,
    max_bytes: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrateArgs {}

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
        assert!(status.contains("migration_required=false"));
        assert!(status.contains("domain name=memory"));
        assert!(export.contains("domain=memory"));
        assert!(export.contains("records=1"));
        assert!(export.contains("Store export"));

        cleanup(root);
    }

    #[test]
    fn store_tools_status_does_not_create_database_and_migrate_is_explicit() {
        let root = temp_root("migrate");
        std::fs::create_dir_all(&root).unwrap();
        let tools = StoreTools::new(&root);

        let status = tools.dispatch(STORE_STATUS_TOOL_ID, json!({})).unwrap();
        assert!(status.contains("database_exists=false"));
        assert!(status.contains("migration_required=true"));
        assert!(!root.join(agl_store::DEFAULT_DATABASE_FILE).exists());

        let migrated = tools.dispatch(STORE_MIGRATE_TOOL_ID, json!({})).unwrap();
        let current = tools.dispatch(STORE_STATUS_TOOL_ID, json!({})).unwrap();

        assert!(migrated.contains("tool=store.migrate"));
        assert!(migrated.contains("status=ok"));
        assert!(current.contains("database_exists=true"));
        assert!(current.contains("migration_required=false"));

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
