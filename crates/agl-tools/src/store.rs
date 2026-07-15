use std::path::{Path, PathBuf};

use agl_capabilities::{
    ActionDeclaration, ActionHandler, ActionHandlerError, ActionInvocation, ActionResult,
    CapabilityId, OperationKind, ProviderDeclaration, ProviderId, StateEffect,
};
use agl_store::{AglStore, StoreDomain, StoreExportOptions};
use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ToolCatalog, ToolCatalogError, parse_action_args as parse_args};

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

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<Value> {
        match name {
            STORE_STATUS_TOOL_ID => self.status(arguments),
            STORE_EXPORT_TOOL_ID => self.export(arguments),
            STORE_MIGRATE_TOOL_ID => self.migrate(arguments),
            _ => anyhow::bail!("unknown store tool `{name}`"),
        }
    }

    fn status(&self, arguments: Value) -> Result<Value> {
        parse_args::<StatusArgs>(STORE_STATUS_TOOL_ID, arguments)?;
        let schema = AglStore::schema_status_at(&self.store_root)?;
        let (idempotency, domains) = if schema.migration_required {
            (Value::Null, Vec::new())
        } else {
            let store = self.open_current_read_only_store()?;
            let status = store.status()?;
            let idempotency = json!({
                "in_progress": status.idempotency.in_progress,
                "stale_in_progress": status.idempotency.stale_in_progress.len(),
            });
            let domains = status
                .domains
                .into_iter()
                .map(|domain| {
                    json!({
                        "name": domain.domain.as_str(),
                        "status": domain.status.as_str(),
                        "total_rows": domain.total_rows,
                        "active_rows": domain.active_rows,
                    })
                })
                .collect();
            (idempotency, domains)
        };
        Ok(json!({
            "tool": STORE_STATUS_TOOL_ID,
            "status": "ok",
            "schema_version": schema.schema_version,
            "current_schema_version": schema.current_schema_version,
            "database_path": schema.database_path,
            "database_exists": schema.database_exists,
            "migration_required": schema.migration_required,
            "applied_migrations": schema.applied_migrations,
            "idempotency": idempotency,
            "domains": domains,
        }))
    }

    fn export(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<ExportArgs>(STORE_EXPORT_TOOL_ID, arguments)?;
        let domain = StoreDomain::from(args.domain);
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
        let body = String::from_utf8(bytes).context("store export was not valid UTF-8")?;
        let mut exported_bytes = 0usize;
        let mut exported_records = Vec::new();
        for line in body.lines() {
            let line_bytes = line.len().saturating_add(1);
            if exported_bytes.saturating_add(line_bytes) > max_bytes {
                break;
            }
            exported_records.push(
                serde_json::from_str::<Value>(line)
                    .context("store export contained an invalid JSONL record")?,
            );
            exported_bytes += line_bytes;
        }
        let truncated = exported_records.len() < records;
        Ok(json!({
            "tool": STORE_EXPORT_TOOL_ID,
            "status": "ok",
            "domain": domain.as_str(),
            "record_count": records,
            "returned_count": exported_records.len(),
            "truncated": truncated,
            "bytes": exported_bytes,
            "records": exported_records,
        }))
    }

    fn migrate(&self, arguments: Value) -> Result<Value> {
        parse_args::<MigrateArgs>(STORE_MIGRATE_TOOL_ID, arguments)?;
        let report = AglStore::migrate_at(&self.store_root)
            .with_context(|| format!("failed to migrate store {}", self.store_root.display()))?;
        let migrations = report
            .applied_migrations
            .into_iter()
            .map(|migration| {
                json!({
                    "version": migration.version,
                    "name": migration.name,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "tool": STORE_MIGRATE_TOOL_ID,
            "status": "ok",
            "database_path": report.database_path,
            "before_schema_version": report.before_schema_version,
            "after_schema_version": report.after_schema_version,
            "applied_migrations": migrations,
        }))
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

impl ActionHandler for StoreTools {
    fn dispatch(&self, invocation: ActionInvocation) -> Result<ActionResult, ActionHandlerError> {
        self.dispatch(invocation.capability_id.as_str(), invocation.arguments)
            .map(ActionResult::new)
            .map_err(Into::into)
    }
}

pub fn declaration() -> ProviderDeclaration {
    ProviderDeclaration::builtin(
        ProviderId::new(PROVIDER_ID).expect("builtin store provider id is valid"),
        "Store Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin store provider declaration is valid")
    .with_action(
        ActionDeclaration::from_schema::<StatusArgs>(
            CapabilityId::new(STORE_STATUS_TOOL_ID).expect("builtin store action id is valid"),
            "Inspect store schema, domain health, and idempotency health.",
            OperationKind::Read,
        )
        .expect("builtin store status schema is valid"),
    )
    .with_action(
        ActionDeclaration::from_schema::<ExportArgs>(
            CapabilityId::new(STORE_EXPORT_TOOL_ID).expect("builtin store action id is valid"),
            "Export one known store domain as bounded structured records.",
            OperationKind::Read,
        )
        .expect("builtin store export schema is valid"),
    )
    .with_action(
        ActionDeclaration::from_schema::<MigrateArgs>(
            CapabilityId::new(STORE_MIGRATE_TOOL_ID).expect("builtin store action id is valid"),
            "Run agentLIBRE store migrations through an explicit admin boundary.",
            OperationKind::Admin,
        )
        .expect("builtin store migration schema is valid")
        .with_state_effects([StateEffect::StoreSchema]),
    )
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatusArgs {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExportArgs {
    domain: StoreDomainArg,
    include_deleted: Option<bool>,
    max_bytes: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MigrateArgs {}

#[derive(Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum StoreDomainArg {
    Memory,
    Notes,
    Cron,
    Permissions,
}

impl From<StoreDomainArg> for StoreDomain {
    fn from(value: StoreDomainArg) -> Self {
        match value {
            StoreDomainArg::Memory => Self::Memory,
            StoreDomainArg::Notes => Self::Notes,
            StoreDomainArg::Cron => Self::Cron,
            StoreDomainArg::Permissions => Self::Permissions,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::memory::{MEMORY_ADD_TOOL_ID, MemoryTools};
    use crate::test_support::{migrated_temp_root, temp_root};

    use super::*;

    #[test]
    fn store_tools_report_status_and_export_known_domains() {
        let root = migrated_temp_root("export");
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

        assert!(status["schema_version"].is_number());
        assert_eq!(status["migration_required"], false);
        assert_eq!(status["domains"][0]["name"], "memory");
        assert_eq!(export["domain"], "memory");
        assert_eq!(export["record_count"], 1);
        assert_eq!(export["records"][0]["title"], "Store export");
    }

    #[test]
    fn store_tools_status_does_not_create_database_and_migrate_is_explicit() {
        let root = temp_root("migrate");
        let tools = StoreTools::new(&root);

        let status = tools.dispatch(STORE_STATUS_TOOL_ID, json!({})).unwrap();
        assert_eq!(status["database_exists"], false);
        assert_eq!(status["migration_required"], true);
        assert!(!root.join(agl_store::DEFAULT_DATABASE_FILE).exists());

        let migrated = tools.dispatch(STORE_MIGRATE_TOOL_ID, json!({})).unwrap();
        let current = tools.dispatch(STORE_STATUS_TOOL_ID, json!({})).unwrap();

        assert_eq!(migrated["tool"], STORE_MIGRATE_TOOL_ID);
        assert_eq!(migrated["status"], "ok");
        assert_eq!(current["database_exists"], true);
        assert_eq!(current["migration_required"], false);
    }

    #[test]
    fn store_declarations_expose_closed_schemas() {
        let declaration = declaration();
        for action in &declaration.actions {
            assert_eq!(action.input_schema["additionalProperties"], false);
        }
        let export = declaration
            .actions
            .iter()
            .find(|action| action.id.as_str() == STORE_EXPORT_TOOL_ID)
            .unwrap();
        assert_eq!(export.input_schema["required"], json!(["domain"]));
        assert!(
            export
                .compile_schema()
                .unwrap()
                .validate(&json!({"domain": "unknown"}))
                .is_err()
        );
    }
}
