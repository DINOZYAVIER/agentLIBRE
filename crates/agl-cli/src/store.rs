use std::collections::BTreeMap;
use std::fs::OpenOptions;

use agl_runtime::AgentLibreRuntimeConfig;
use agl_store::{
    AglStore, StoreDomain, StoreExportOptions as AglStoreExportOptions, StoreSchemaStatus,
    StoreStatus, default_store_root,
};
use anyhow::{Context, Result};

use crate::args::{
    StoreCommand, StoreDomainArg, StoreExportCliOptions, StoreMigrateOptions, StoreStatusOptions,
};

pub(crate) fn run_store(command: StoreCommand, runtime: &AgentLibreRuntimeConfig) -> Result<()> {
    tracing::info!(target: "agentlibre::app", command = "store", "starting command");
    let store_root = default_store_root(&runtime.paths);

    match command {
        StoreCommand::Status(options) => run_store_status(options, &store_root),
        StoreCommand::Migrate(options) => run_store_migrate(options, &store_root),
        StoreCommand::Export(options) => run_store_export(options, &store_root),
    }
}

fn run_store_status(options: StoreStatusOptions, store_root: &std::path::Path) -> Result<()> {
    let schema = AglStore::schema_status_at(store_root).context("failed to read store schema")?;
    if schema.migration_required {
        if options.json {
            println!("{}", serde_json::to_string_pretty(&schema)?);
        } else {
            print_store_schema_status(&schema);
        }
        return Ok(());
    }
    let store = AglStore::open_current_read_only_at(store_root).context("failed to open store")?;
    let status = store.status().context("failed to read store status")?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print_store_status(&status);
    }
    Ok(())
}

fn run_store_migrate(options: StoreMigrateOptions, store_root: &std::path::Path) -> Result<()> {
    let report = AglStore::migrate_at(store_root).context("failed to migrate store")?;
    if options.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("store.migrated=true");
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
        for migration in report.applied_migrations {
            println!(
                "store.migration version={} name={}",
                migration.version, migration.name
            );
        }
    }
    Ok(())
}

fn run_store_export(options: StoreExportCliOptions, store_root: &std::path::Path) -> Result<()> {
    let domain = store_domain(options.domain);
    let store =
        AglStore::open_current_read_only_at(store_root).context("failed to open current store")?;
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
    let file = OpenOptions::new()
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
    let records = store
        .export_domain_jsonl(
            &AglStoreExportOptions {
                domain,
                include_deleted: options.include_deleted,
            },
            file,
        )
        .context("failed to export store domain")?;
    let record_types = record_type_counts(&options.out)?;

    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "domain": domain.as_str(),
                "path": options.out,
                "records": records,
                "record_types": record_types,
                "include_deleted": options.include_deleted,
            }))?
        );
    } else {
        println!("store.exported=true");
        println!("store.export.domain={}", domain.as_str());
        println!("store.export.path={}", options.out.display());
        println!("store.export.records={records}");
        println!("store.export.include_deleted={}", options.include_deleted);
        for (record_type, count) in record_types {
            println!("store.export.record_type.{record_type}={count}");
        }
    }
    Ok(())
}

fn store_domain(domain: StoreDomainArg) -> StoreDomain {
    match domain {
        StoreDomainArg::Memory => StoreDomain::Memory,
        StoreDomainArg::Notes => StoreDomain::Notes,
        StoreDomainArg::Cron => StoreDomain::Cron,
        StoreDomainArg::Permissions => StoreDomain::Permissions,
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

fn print_store_status(status: &StoreStatus) {
    println!("store.path={}", status.database_path.display());
    println!("store.schema_version={}", status.schema_version);
    println!("store.current_schema_version={}", status.schema_version);
    println!("store.database_exists=true");
    println!("store.migration_required=false");
    for domain in &status.domains {
        println!(
            "store.domain.{}={} total_rows={} active_rows={}",
            domain.domain.as_str(),
            domain.status.as_str(),
            domain.total_rows,
            domain.active_rows
        );
    }
    println!(
        "store.idempotency.in_progress={}",
        status.idempotency.in_progress
    );
    println!(
        "store.idempotency.stale_in_progress={}",
        status.idempotency.stale_in_progress.len()
    );
    for (index, record) in status.idempotency.stale_in_progress.iter().enumerate() {
        println!(
            "store.idempotency.stale.{index}.namespace={}",
            record.namespace
        );
        println!("store.idempotency.stale.{index}.key={}", record.key);
        println!(
            "store.idempotency.stale.{index}.created_at={}",
            record.created_at
        );
        println!(
            "store.idempotency.stale.{index}.updated_at={}",
            record.updated_at
        );
    }
}

fn print_store_schema_status(status: &StoreSchemaStatus) {
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
