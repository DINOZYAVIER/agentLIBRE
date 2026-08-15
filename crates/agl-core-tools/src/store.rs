use std::path::PathBuf;
use std::sync::Arc;

use agl_kernel::{
    EffectDeclaration, EffectId, ExtensionDescriptor, ExtensionId, OperationKind, ToolDeclaration,
    ToolDispatchContext, ToolHandler, ToolId, ToolResult,
};
use anyhow::{Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::parse_tool_args as parse_args;

pub const EXTENSION_ID: &str = "core.store";
pub const STORE_STATUS_TOOL_ID: &str = "core.store:status";
pub const STORE_EXPORT_TOOL_ID: &str = "core.store:export";
pub const STORE_MIGRATE_TOOL_ID: &str = "core.store:migrate";

const DEFAULT_EXPORT_MAX_BYTES: usize = 16 * 1024;
const MAX_EXPORT_BYTES: usize = 128 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum StoreAdminDomain {
    Memory,
    Notes,
    Cron,
    Permissions,
}

impl StoreAdminDomain {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Notes => "notes",
            Self::Cron => "cron",
            Self::Permissions => "permissions",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StoreSchemaSnapshot {
    pub database_path: PathBuf,
    pub database_exists: bool,
    pub schema_version: Option<u32>,
    pub current_schema_version: u32,
    pub applied_migrations: Vec<u32>,
    pub migration_required: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StoreDomainSnapshot {
    pub name: String,
    pub status: String,
    pub total_rows: u64,
    pub active_rows: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StoreStatusSnapshot {
    pub idempotency_in_progress: u64,
    pub stale_idempotency_count: usize,
    pub domains: Vec<StoreDomainSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StoreExportSnapshot {
    pub record_count: usize,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StoreAppliedMigrationSnapshot {
    pub version: u32,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StoreMigrationSnapshot {
    pub database_path: PathBuf,
    pub before_schema_version: u32,
    pub after_schema_version: u32,
    pub applied_migrations: Vec<StoreAppliedMigrationSnapshot>,
}

pub trait StoreAdminPort: Send + Sync {
    fn schema_status(&self) -> Result<StoreSchemaSnapshot>;
    fn status(&self) -> Result<StoreStatusSnapshot>;
    fn export_jsonl(
        &self,
        domain: StoreAdminDomain,
        include_deleted: bool,
    ) -> Result<StoreExportSnapshot>;
    fn migrate(&self) -> Result<StoreMigrationSnapshot>;
}

#[derive(Clone)]
pub struct StoreTools {
    administration: Arc<dyn StoreAdminPort>,
}

impl StoreTools {
    pub fn new(administration: Arc<dyn StoreAdminPort>) -> Self {
        Self { administration }
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
        let schema = self.administration.schema_status()?;
        let (idempotency, domains) = if schema.migration_required {
            (Value::Null, Vec::new())
        } else {
            let status = self.administration.status()?;
            let idempotency = json!({
                "in_progress": status.idempotency_in_progress,
                "stale_in_progress": status.stale_idempotency_count,
            });
            let domains = status
                .domains
                .into_iter()
                .map(|domain| {
                    json!({
                        "name": domain.name,
                        "status": domain.status,
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
        let domain = StoreAdminDomain::from(args.domain);
        let max_bytes = args
            .max_bytes
            .unwrap_or(DEFAULT_EXPORT_MAX_BYTES)
            .min(MAX_EXPORT_BYTES);
        let export = self
            .administration
            .export_jsonl(domain, args.include_deleted.unwrap_or(false))?;
        let body = String::from_utf8(export.bytes).context("store export was not valid UTF-8")?;
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
        let truncated = exported_records.len() < export.record_count;
        Ok(json!({
            "tool": STORE_EXPORT_TOOL_ID,
            "status": "ok",
            "domain": domain.as_str(),
            "record_count": export.record_count,
            "returned_count": exported_records.len(),
            "truncated": truncated,
            "bytes": exported_bytes,
            "records": exported_records,
        }))
    }

    fn migrate(&self, arguments: Value) -> Result<Value> {
        parse_args::<MigrateArgs>(STORE_MIGRATE_TOOL_ID, arguments)?;
        let report = self.administration.migrate()?;
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
}

impl ToolHandler for StoreTools {
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
        ExtensionId::new(EXTENSION_ID).expect("builtin store extension id is valid"),
        "Store Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin store extension declaration is valid")
    .with_tool(
        ToolDeclaration::from_schema::<StatusArgs>(
            ToolId::new(STORE_STATUS_TOOL_ID).expect("builtin store action id is valid"),
            "Inspect store schema, domain health, and idempotency health.",
            OperationKind::Read,
        )
        .expect("builtin store status schema is valid"),
    )
    .with_tool(
        ToolDeclaration::from_schema::<ExportArgs>(
            ToolId::new(STORE_EXPORT_TOOL_ID).expect("builtin store action id is valid"),
            "Export one known store domain as bounded structured records.",
            OperationKind::Read,
        )
        .expect("builtin store export schema is valid"),
    )
    .with_tool(
        ToolDeclaration::from_schema::<MigrateArgs>(
            ToolId::new(STORE_MIGRATE_TOOL_ID).expect("builtin store action id is valid"),
            "Run agentLIBRE store migrations through an explicit admin boundary.",
            OperationKind::Admin,
        )
        .expect("builtin store migration schema is valid")
        .with_state_effects([EffectId::store_schema()]),
    )
    .with_effect(EffectDeclaration::for_standard(EffectId::store_schema()).unwrap())
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

impl From<StoreDomainArg> for StoreAdminDomain {
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

    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct FakeAdmin {
        migrated: Mutex<bool>,
    }

    impl StoreAdminPort for FakeAdmin {
        fn schema_status(&self) -> Result<StoreSchemaSnapshot> {
            let migrated = *self.migrated.lock().unwrap();
            Ok(StoreSchemaSnapshot {
                database_path: PathBuf::from("/tmp/agentlibre.sqlite3"),
                database_exists: migrated,
                schema_version: migrated.then_some(20),
                current_schema_version: 20,
                applied_migrations: if migrated {
                    (1..=20).collect()
                } else {
                    Vec::new()
                },
                migration_required: !migrated,
            })
        }

        fn status(&self) -> Result<StoreStatusSnapshot> {
            Ok(StoreStatusSnapshot {
                idempotency_in_progress: 0,
                stale_idempotency_count: 0,
                domains: vec![StoreDomainSnapshot {
                    name: "memory".to_owned(),
                    status: "ok".to_owned(),
                    total_rows: 1,
                    active_rows: 1,
                }],
            })
        }

        fn export_jsonl(
            &self,
            _domain: StoreAdminDomain,
            _include_deleted: bool,
        ) -> Result<StoreExportSnapshot> {
            Ok(StoreExportSnapshot {
                record_count: 1,
                bytes: b"{\"title\":\"Store export\"}\n".to_vec(),
            })
        }

        fn migrate(&self) -> Result<StoreMigrationSnapshot> {
            *self.migrated.lock().unwrap() = true;
            Ok(StoreMigrationSnapshot {
                database_path: PathBuf::from("/tmp/agentlibre.sqlite3"),
                before_schema_version: 0,
                after_schema_version: 20,
                applied_migrations: vec![StoreAppliedMigrationSnapshot {
                    version: 20,
                    name: "020_domain_persistence_authority".to_owned(),
                }],
            })
        }
    }

    #[test]
    fn store_tools_report_status_and_export_known_domains() {
        let admin = Arc::new(FakeAdmin::default());
        *admin.migrated.lock().unwrap() = true;
        let tools = StoreTools::new(admin);
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
        let tools = StoreTools::new(Arc::new(FakeAdmin::default()));

        let status = tools.dispatch(STORE_STATUS_TOOL_ID, json!({})).unwrap();
        assert_eq!(status["database_exists"], false);
        assert_eq!(status["migration_required"], true);
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
        for action in &declaration.tools {
            assert_eq!(action.input_schema["additionalProperties"], false);
        }
        let export = declaration
            .tools
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
