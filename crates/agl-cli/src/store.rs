use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write as _;

use agl_core_tools::{StoreAdminDomain, StoreSchemaSnapshot, StoreStatusSnapshot};
use agl_runtime::{AgentLibreRuntimeConfig, StoreRuntime};
use anyhow::{Context, Result, bail};

use crate::args::{
    StoreCommand, StoreDomainArg, StoreExportCliOptions, StoreMigrateOptions, StoreStatusOptions,
};

pub(crate) fn run_store(command: StoreCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "store", "starting command");
    match command {
        StoreCommand::Status(options) => run_store_status(options, runtime),
        StoreCommand::Migrate(options) => run_store_migrate(options, runtime),
        StoreCommand::Export(options) => run_store_export(options, runtime),
    }
}

fn run_store_status(options: StoreStatusOptions, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    let schema = StoreRuntime::inspect(&runtime.paths).context("failed to read store schema")?;
    if schema.migration_required {
        return crate::print_json_or(options.json, &schema, || print_store_schema_status(&schema));
    }
    let store = StoreRuntime::open(&runtime.paths).context("failed to open store runtime")?;
    let status = store
        .repositories()
        .administration
        .status()
        .context("failed to read store status")?;
    if options.json {
        crate::print_json(&serde_json::json!({
            "schema": schema,
            "status": status,
        }))
    } else {
        print_store_status(&schema, &status);
        Ok(())
    }
}

fn run_store_migrate(
    options: StoreMigrateOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let report = StoreRuntime::migrate(&runtime.paths).context("failed to migrate store")?;
    crate::print_json_or(options.json, &report, || {
        println!("core.store:migrated=true");
        println!("store.path={}", report.database_path.display());
        println!(
            "store.schema_version.before={}",
            report.before_schema_version
        );
        println!("store.schema_version.after={}", report.after_schema_version);
        println!(
            "store.migrations.applied={}",
            report.applied_migrations.len()
        );
        for migration in &report.applied_migrations {
            println!(
                "store.migration version={} name={}",
                migration.version, migration.name
            );
        }
    })
}

fn run_store_export(
    options: StoreExportCliOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let domain = store_domain(options.domain);
    let schema = StoreRuntime::inspect(&runtime.paths).context("failed to read store schema")?;
    if schema.migration_required {
        bail!("store migration required; run core.store:migrate first");
    }
    let store = StoreRuntime::open(&runtime.paths).context("failed to open current store")?;
    if let Some(parent) = options
        .out
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create store export directory {}",
                parent.display()
            )
        })?;
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .create_new(!options.force)
        .truncate(options.force)
        .open(&options.out)
        .with_context(|| {
            if options.force {
                format!("failed to open store export path {}", options.out.display())
            } else {
                format!(
                    "failed to create store export path {}; pass --force to overwrite",
                    options.out.display()
                )
            }
        })?;
    let export = store
        .repositories()
        .administration
        .export_jsonl(domain, options.include_deleted)
        .context("failed to export store domain")?;
    file.write_all(&export.bytes)
        .context("failed to write store export")?;
    let records = export.record_count;
    let record_types = record_type_counts(&options.out)?;

    if options.json {
        crate::print_json(&serde_json::json!({
            "domain": domain.as_str(),
            "path": options.out,
            "records": records,
            "record_types": record_types,
            "include_deleted": options.include_deleted,
        }))?;
    } else {
        println!("core.store:exported=true");
        println!("core.store:export.domain={}", domain.as_str());
        println!("core.store:export.path={}", options.out.display());
        println!("core.store:export.records={records}");
        println!(
            "core.store:export.include_deleted={}",
            options.include_deleted
        );
        for (record_type, count) in record_types {
            println!("core.store:export.record_type.{record_type}={count}");
        }
    }
    Ok(())
}

fn store_domain(domain: StoreDomainArg) -> StoreAdminDomain {
    match domain {
        StoreDomainArg::Memory => StoreAdminDomain::Memory,
        StoreDomainArg::Notes => StoreAdminDomain::Notes,
        StoreDomainArg::Cron => StoreAdminDomain::Cron,
        StoreDomainArg::Permissions => StoreAdminDomain::Permissions,
    }
}

fn record_type_counts(path: &std::path::Path) -> Result<BTreeMap<String, usize>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read store export {}", path.display()))?;
    let mut counts = BTreeMap::new();
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        let value: serde_json::Value =
            serde_json::from_str(line).context("failed to parse exported JSONL record")?;
        let record_type = value
            .get("record_type")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown");
        *counts.entry(record_type.to_string()).or_insert(0) += 1;
    }
    Ok(counts)
}

fn print_store_status(schema: &StoreSchemaSnapshot, status: &StoreStatusSnapshot) {
    print_store_schema_status(schema);
    for domain in &status.domains {
        println!(
            "store.domain.{}={} total_rows={} active_rows={}",
            domain.name, domain.status, domain.total_rows, domain.active_rows
        );
    }
    println!(
        "store.idempotency.in_progress={}",
        status.idempotency_in_progress
    );
    println!(
        "store.idempotency.stale_in_progress={}",
        status.stale_idempotency_count
    );
}

fn print_store_schema_status(status: &StoreSchemaSnapshot) {
    println!("store.path={}", status.database_path.display());
    println!(
        "store.schema_version={}",
        status
            .schema_version
            .map(|version| version.to_string())
            .unwrap_or_else(|| "none".to_string())
    );
    println!(
        "store.current_schema_version={}",
        status.current_schema_version
    );
    println!("store.database_exists={}", status.database_exists);
    println!("store.migration_required={}", status.migration_required);
    println!(
        "store.applied_migrations={}",
        status
            .applied_migrations
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    );
    if status.migration_required {
        println!("next_step=agl store migrate");
    }
}
