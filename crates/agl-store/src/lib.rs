use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params, types::Type};
use serde::Deserialize;
use serde_json::json;

mod error;
mod idempotency;
mod matrix_outbox;
mod migrations;
mod path;
mod permissions;
mod status;
mod types;

pub use error::{Result, StoreError};
pub use migrations::{CURRENT_SCHEMA_VERSION, STORE_MIGRATIONS, StoreMigration};
pub use path::default_database_path;
use path::{database_path, ensure_private_dir, set_private_file_permissions};
pub use types::*;

pub const DEFAULT_DATABASE_FILE: &str = "agentlibre.sqlite3";

#[derive(Debug)]
pub struct AglStore {
    conn: Connection,
    database_path: PathBuf,
}

impl AglStore {
    pub fn open_at(root: impl AsRef<Path>) -> Result<Self> {
        let store = Self::open_for_migration_at(root)?;
        store.migrate()?;
        Ok(store)
    }

    pub fn migrate_at(root: impl AsRef<Path>) -> Result<StoreMigrationReport> {
        let store = Self::open_for_migration_at(root)?;
        store.migrate()
    }

    pub fn schema_status_at(root: impl AsRef<Path>) -> Result<StoreSchemaStatus> {
        let database_path = default_database_path(root)?;
        if !database_path.exists() {
            return Ok(StoreSchemaStatus {
                database_path,
                database_exists: false,
                schema_version: None,
                current_schema_version: CURRENT_SCHEMA_VERSION,
                applied_migrations: Vec::new(),
                migration_required: true,
            });
        }
        let conn = Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let store = Self {
            conn,
            database_path: database_path.clone(),
        };
        let schema_version = store.schema_version()?;
        let applied_migrations = if store.schema_migrations_table_exists()? {
            store.applied_migration_versions()?
        } else {
            Vec::new()
        };
        let migration_required = schema_version != CURRENT_SCHEMA_VERSION
            || applied_migrations.len() != STORE_MIGRATIONS.len()
            || applied_migrations.last().copied() != Some(CURRENT_SCHEMA_VERSION);
        Ok(StoreSchemaStatus {
            database_path,
            database_exists: true,
            schema_version: Some(schema_version),
            current_schema_version: CURRENT_SCHEMA_VERSION,
            applied_migrations,
            migration_required,
        })
    }

    pub fn open_current_read_only_at(root: impl AsRef<Path>) -> Result<Self> {
        let status = Self::schema_status_at(root)?;
        if !status.database_exists {
            return Err(StoreError::InvalidValue {
                field: "store",
                value: status.database_path.display().to_string(),
                reason: "store database does not exist; run store.migrate first",
            });
        }
        if status.migration_required {
            return Err(StoreError::InvalidValue {
                field: "store",
                value: format!(
                    "schema_version={:?}, current_schema_version={}",
                    status.schema_version, status.current_schema_version
                ),
                reason: "store schema migration required; run store.migrate first",
            });
        }
        let conn =
            Connection::open_with_flags(&status.database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self {
            conn,
            database_path: status.database_path,
        })
    }

    pub fn open_current_at(root: impl AsRef<Path>) -> Result<Self> {
        let status = Self::schema_status_at(root)?;
        if !status.database_exists {
            return Err(StoreError::InvalidValue {
                field: "store",
                value: status.database_path.display().to_string(),
                reason: "store database does not exist; run store.migrate first",
            });
        }
        if status.migration_required {
            return Err(StoreError::InvalidValue {
                field: "store",
                value: format!(
                    "schema_version={:?}, current_schema_version={}",
                    status.schema_version, status.current_schema_version
                ),
                reason: "store schema migration required; run store.migrate first",
            });
        }
        let conn = Connection::open(&status.database_path)?;
        set_private_file_permissions(&status.database_path)?;
        Ok(Self {
            conn,
            database_path: status.database_path,
        })
    }

    fn open_for_migration_at(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let database_path = database_path(root, DEFAULT_DATABASE_FILE)?;
        ensure_private_dir(root)?;
        let conn = Connection::open(&database_path)?;
        set_private_file_permissions(&database_path)?;
        Ok(Self {
            conn,
            database_path,
        })
    }

    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn transaction<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let tx = self.conn.unchecked_transaction()?;
        match f(&tx) {
            Ok(value) => {
                tx.commit()?;
                Ok(value)
            }
            Err(err) => {
                let _ = tx.rollback();
                Err(err)
            }
        }
    }

    pub fn export_domain_jsonl<W: Write>(
        &self,
        options: &StoreExportOptions,
        writer: W,
    ) -> Result<usize> {
        match options.domain {
            StoreDomain::Memory => self.export_memory_jsonl(options.include_deleted, writer),
            StoreDomain::Notes => self.export_notes_jsonl(options.include_deleted, writer),
            StoreDomain::Cron => self.export_cron_jsonl(options.include_deleted, writer),
            StoreDomain::Permissions => {
                self.export_permissions_jsonl(options.include_deleted, writer)
            }
        }
    }

    fn migrate(&self) -> Result<StoreMigrationReport> {
        let before_schema_version = self.schema_version()?;
        self.ensure_migration_table()?;
        let current_version = self.schema_version()?;
        if current_version > CURRENT_SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchemaVersion {
                found: current_version,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }
        let applied_versions = self.applied_migration_versions()?;
        for version in &applied_versions {
            if *version > CURRENT_SCHEMA_VERSION {
                return Err(StoreError::UnsupportedSchemaVersion {
                    found: *version,
                    supported: CURRENT_SCHEMA_VERSION,
                });
            }
        }
        validate_migration_sequence(&applied_versions)?;
        let mut applied_migrations = Vec::new();
        for migration in STORE_MIGRATIONS {
            if !self.migration_applied(migration.version)? {
                self.apply_migration(migration)?;
                applied_migrations.push(AppliedStoreMigration {
                    version: migration.version,
                    name: migration.name.to_string(),
                });
            }
        }
        Ok(StoreMigrationReport {
            database_path: self.database_path.clone(),
            before_schema_version,
            after_schema_version: self.schema_version()?,
            applied_migrations,
        })
    }

    fn ensure_migration_table(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    fn applied_migration_versions(&self) -> Result<Vec<u32>> {
        let mut stmt = self
            .conn
            .prepare("SELECT version FROM schema_migrations ORDER BY version")?;
        let rows = stmt.query_map([], |row| row.get::<_, u32>(0))?;
        let mut versions = Vec::new();
        for row in rows {
            versions.push(row?);
        }
        Ok(versions)
    }

    fn schema_migrations_table_exists(&self) -> Result<bool> {
        let exists = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(exists)
    }

    fn migration_applied(&self, version: u32) -> Result<bool> {
        let applied = self
            .conn
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE version = ?1",
                params![version],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(applied)
    }

    fn apply_migration(&self, migration: &StoreMigration) -> Result<()> {
        let batch = format!(
            r#"
            BEGIN;
            {sql}
            INSERT INTO schema_migrations(version, applied_at)
            VALUES ({version}, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'));
            PRAGMA user_version = {version};
            COMMIT;
            "#,
            sql = migration.sql,
            version = migration.version
        );
        self.conn.execute_batch(&batch)?;
        Ok(())
    }

    fn schema_version(&self) -> Result<u32> {
        let version = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))?;
        Ok(version)
    }

    fn export_memory_jsonl<W: Write>(&self, include_deleted: bool, mut writer: W) -> Result<usize> {
        let sql = if include_deleted {
            "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at
             FROM memory_entries
             ORDER BY updated_at ASC, id ASC"
        } else {
            "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, created_at, updated_at, deleted_at
             FROM memory_entries
             WHERE deleted_at IS NULL
             ORDER BY updated_at ASC, id ASC"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| {
            Ok(json!({
                "domain": StoreDomain::Memory.as_str(),
                "record_type": "memory_entry",
                "id": row.get::<_, String>(0)?,
                "scope_kind": row.get::<_, String>(1)?,
                "scope_key": row.get::<_, String>(2)?,
                "kind": row.get::<_, String>(3)?,
                "title": row.get::<_, String>(4)?,
                "body": row.get::<_, String>(5)?,
                "source_ref": row.get::<_, Option<String>>(6)?,
                "confidence": row.get::<_, i64>(7)?,
                "created_at": row.get::<_, String>(8)?,
                "updated_at": row.get::<_, String>(9)?,
                "deleted_at": row.get::<_, Option<String>>(10)?,
            }))
        })?;
        let mut count = write_jsonl_rows(&mut writer, rows)?;

        let suggestions_sql = if include_deleted {
            "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
             FROM memory_suggestions
             ORDER BY updated_at ASC, id ASC"
        } else {
            "SELECT id, scope_kind, scope_key, kind, title, body, source_ref, confidence, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
             FROM memory_suggestions
             WHERE status = 'pending'
             ORDER BY updated_at ASC, id ASC"
        };
        let mut suggestions_stmt = self.conn.prepare(suggestions_sql)?;
        let suggestions = suggestions_stmt.query_map([], |row| {
            Ok(json!({
                "domain": StoreDomain::Memory.as_str(),
                "record_type": "memory_suggestion",
                "id": row.get::<_, String>(0)?,
                "scope_kind": row.get::<_, String>(1)?,
                "scope_key": row.get::<_, String>(2)?,
                "kind": row.get::<_, String>(3)?,
                "title": row.get::<_, String>(4)?,
                "body": row.get::<_, String>(5)?,
                "source_ref": row.get::<_, String>(6)?,
                "confidence": row.get::<_, i64>(7)?,
                "status": row.get::<_, String>(8)?,
                "created_at": row.get::<_, String>(9)?,
                "updated_at": row.get::<_, String>(10)?,
                "resolved_at": row.get::<_, Option<String>>(11)?,
                "resolution_ref": row.get::<_, Option<String>>(12)?,
                "resolution_note": row.get::<_, Option<String>>(13)?,
            }))
        })?;
        count += write_jsonl_rows(&mut writer, suggestions)?;
        Ok(count)
    }

    fn export_notes_jsonl<W: Write>(&self, include_deleted: bool, mut writer: W) -> Result<usize> {
        let notes_sql = if include_deleted {
            "SELECT id, title, body, created_at, updated_at, deleted_at
             FROM notes
             ORDER BY updated_at ASC, id ASC"
        } else {
            "SELECT id, title, body, created_at, updated_at, deleted_at
             FROM notes
             WHERE deleted_at IS NULL
             ORDER BY updated_at ASC, id ASC"
        };
        let mut notes_stmt = self.conn.prepare(notes_sql)?;
        let notes = notes_stmt.query_map([], |row| {
            Ok(json!({
                "domain": StoreDomain::Notes.as_str(),
                "record_type": "note",
                "id": row.get::<_, String>(0)?,
                "title": row.get::<_, String>(1)?,
                "body": row.get::<_, String>(2)?,
                "created_at": row.get::<_, String>(3)?,
                "updated_at": row.get::<_, String>(4)?,
                "deleted_at": row.get::<_, Option<String>>(5)?,
            }))
        })?;
        let mut count = write_jsonl_rows(&mut writer, notes)?;

        let links_sql = if include_deleted {
            "SELECT id, note_id, target_ref, label, created_at
             FROM note_links
             ORDER BY created_at ASC, id ASC"
        } else {
            "SELECT l.id, l.note_id, l.target_ref, l.label, l.created_at
             FROM note_links l
             JOIN notes n ON n.id = l.note_id
             WHERE n.deleted_at IS NULL
             ORDER BY l.created_at ASC, l.id ASC"
        };
        let mut links_stmt = self.conn.prepare(links_sql)?;
        let links = links_stmt.query_map([], |row| {
            Ok(json!({
                "domain": StoreDomain::Notes.as_str(),
                "record_type": "note_link",
                "id": row.get::<_, String>(0)?,
                "note_id": row.get::<_, String>(1)?,
                "target_ref": row.get::<_, String>(2)?,
                "label": row.get::<_, Option<String>>(3)?,
                "created_at": row.get::<_, String>(4)?,
            }))
        })?;
        count += write_jsonl_rows(&mut writer, links)?;
        Ok(count)
    }

    fn export_cron_jsonl<W: Write>(&self, include_deleted: bool, mut writer: W) -> Result<usize> {
        let jobs_sql = if include_deleted {
            "SELECT id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, prompt, input, created_at, updated_at, deleted_at
             FROM cron_jobs
             ORDER BY updated_at ASC, id ASC"
        } else {
            "SELECT id, name, enabled, target_kind, target_ref, schedule_expr, timezone, notify_ref, prompt, input, created_at, updated_at, deleted_at
             FROM cron_jobs
             WHERE deleted_at IS NULL
             ORDER BY updated_at ASC, id ASC"
        };
        let mut jobs_stmt = self.conn.prepare(jobs_sql)?;
        let jobs = jobs_stmt.query_map([], |row| {
            Ok(json!({
                "domain": StoreDomain::Cron.as_str(),
                "record_type": "cron_job",
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "enabled": row.get::<_, bool>(2)?,
                "target_kind": row.get::<_, String>(3)?,
                "target_ref": row.get::<_, String>(4)?,
                "schedule_expr": row.get::<_, String>(5)?,
                "timezone": row.get::<_, String>(6)?,
                "notify_ref": row.get::<_, Option<String>>(7)?,
                "prompt": row.get::<_, Option<String>>(8)?,
                "input": row.get::<_, Option<String>>(9)?,
                "created_at": row.get::<_, String>(10)?,
                "updated_at": row.get::<_, String>(11)?,
                "deleted_at": row.get::<_, Option<String>>(12)?,
            }))
        })?;
        let mut count = write_jsonl_rows(&mut writer, jobs)?;

        let runs_sql = if include_deleted {
            "SELECT id, job_id, scheduled_for, started_at, finished_at, status, result_ref, error
             FROM cron_runs
             ORDER BY scheduled_for ASC, id ASC"
        } else {
            "SELECT r.id, r.job_id, r.scheduled_for, r.started_at, r.finished_at, r.status, r.result_ref, r.error
             FROM cron_runs r
             JOIN cron_jobs j ON j.id = r.job_id
             WHERE j.deleted_at IS NULL
             ORDER BY r.scheduled_for ASC, r.id ASC"
        };
        let mut runs_stmt = self.conn.prepare(runs_sql)?;
        let runs = runs_stmt.query_map([], |row| {
            Ok(json!({
                "domain": StoreDomain::Cron.as_str(),
                "record_type": "cron_run",
                "id": row.get::<_, String>(0)?,
                "job_id": row.get::<_, String>(1)?,
                "scheduled_for": row.get::<_, String>(2)?,
                "started_at": row.get::<_, Option<String>>(3)?,
                "finished_at": row.get::<_, Option<String>>(4)?,
                "status": row.get::<_, String>(5)?,
                "result_ref": row.get::<_, Option<String>>(6)?,
                "error": row.get::<_, Option<String>>(7)?,
            }))
        })?;
        count += write_jsonl_rows(&mut writer, runs)?;

        let mut outbox_stmt = self.conn.prepare(
            "SELECT id, notify_ref, source_kind, source_id, dedupe_key, body, status, error, created_at, updated_at, delivered_at
             FROM matrix_notification_outbox
             ORDER BY created_at ASC, id ASC",
        )?;
        let outbox = outbox_stmt.query_map([], |row| {
            Ok(json!({
                "domain": StoreDomain::Cron.as_str(),
                "record_type": "matrix_notification_outbox",
                "id": row.get::<_, String>(0)?,
                "notify_ref": row.get::<_, String>(1)?,
                "source_kind": row.get::<_, String>(2)?,
                "source_id": row.get::<_, String>(3)?,
                "dedupe_key": row.get::<_, String>(4)?,
                "body": row.get::<_, String>(5)?,
                "status": row.get::<_, String>(6)?,
                "error": row.get::<_, Option<String>>(7)?,
                "created_at": row.get::<_, String>(8)?,
                "updated_at": row.get::<_, String>(9)?,
                "delivered_at": row.get::<_, Option<String>>(10)?,
            }))
        })?;
        count += write_jsonl_rows(&mut writer, outbox)?;
        Ok(count)
    }

    fn export_permissions_jsonl<W: Write>(
        &self,
        include_deleted: bool,
        mut writer: W,
    ) -> Result<usize> {
        let request_sql = if include_deleted {
            "SELECT id, requested_tools_json, max_operation_kind, state_effects_json, scope_json, duration, reason, requester_ref, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
             FROM permission_requests
             ORDER BY updated_at ASC, id ASC"
        } else {
            "SELECT id, requested_tools_json, max_operation_kind, state_effects_json, scope_json, duration, reason, requester_ref, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
             FROM permission_requests
             WHERE status = 'pending'
             ORDER BY updated_at ASC, id ASC"
        };
        let mut request_stmt = self.conn.prepare(request_sql)?;
        let requests = request_stmt.query_map([], |row| {
            Ok(json!({
                "domain": StoreDomain::Permissions.as_str(),
                "record_type": "permission_request",
                "id": row.get::<_, String>(0)?,
                "requested_tools": parse_json_cell::<Vec<String>>(row.get::<_, String>(1)?)?,
                "max_operation_kind": row.get::<_, String>(2)?,
                "state_effects": parse_json_cell::<Vec<String>>(row.get::<_, String>(3)?)?,
                "scope": parse_json_cell::<serde_json::Value>(row.get::<_, String>(4)?)?,
                "duration": row.get::<_, String>(5)?,
                "reason": row.get::<_, String>(6)?,
                "requester_ref": row.get::<_, String>(7)?,
                "status": row.get::<_, String>(8)?,
                "created_at": row.get::<_, String>(9)?,
                "updated_at": row.get::<_, String>(10)?,
                "resolved_at": row.get::<_, Option<String>>(11)?,
                "resolution_ref": row.get::<_, Option<String>>(12)?,
                "resolution_note": row.get::<_, Option<String>>(13)?,
            }))
        })?;
        let mut count = write_jsonl_rows(&mut writer, requests)?;

        let grant_sql = if include_deleted {
            "SELECT id, request_id, tool_id, max_operation_kind, state_effects_json, scope_json, duration, granted_by_ref, status, created_at, updated_at, revoked_at, revoke_ref, admitted_at, last_admitted_run_id, consumed_at
             FROM permission_grants
             ORDER BY updated_at ASC, id ASC"
        } else {
            "SELECT id, request_id, tool_id, max_operation_kind, state_effects_json, scope_json, duration, granted_by_ref, status, created_at, updated_at, revoked_at, revoke_ref, admitted_at, last_admitted_run_id, consumed_at
             FROM permission_grants
             WHERE status = 'active'
             ORDER BY updated_at ASC, id ASC"
        };
        let mut grant_stmt = self.conn.prepare(grant_sql)?;
        let grants = grant_stmt.query_map([], |row| {
            Ok(json!({
                "domain": StoreDomain::Permissions.as_str(),
                "record_type": "permission_grant",
                "id": row.get::<_, String>(0)?,
                "request_id": row.get::<_, Option<String>>(1)?,
                "tool_id": row.get::<_, String>(2)?,
                "max_operation_kind": row.get::<_, String>(3)?,
                "state_effects": parse_json_cell::<Vec<String>>(row.get::<_, String>(4)?)?,
                "scope": parse_json_cell::<serde_json::Value>(row.get::<_, String>(5)?)?,
                "duration": row.get::<_, String>(6)?,
                "granted_by_ref": row.get::<_, String>(7)?,
                "status": row.get::<_, String>(8)?,
                "created_at": row.get::<_, String>(9)?,
                "updated_at": row.get::<_, String>(10)?,
                "revoked_at": row.get::<_, Option<String>>(11)?,
                "revoke_ref": row.get::<_, Option<String>>(12)?,
                "admitted_at": row.get::<_, Option<String>>(13)?,
                "last_admitted_run_id": row.get::<_, Option<String>>(14)?,
                "consumed_at": row.get::<_, Option<String>>(15)?,
            }))
        })?;
        count += write_jsonl_rows(&mut writer, grants)?;
        Ok(count)
    }
}

fn validate_non_empty_list(values: &[String], field: &'static str) -> Result<()> {
    if values.is_empty() {
        return Err(StoreError::InvalidValue {
            field,
            value: "[]".to_string(),
            reason: "list cannot be empty",
        });
    }
    for value in values {
        validate_non_blank(value, field)?;
    }
    Ok(())
}

fn validate_non_blank(value: &str, field: &'static str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(StoreError::InvalidValue {
            field,
            value: value.to_string(),
            reason: match field {
                "namespace" => "namespace cannot be blank",
                "key" => "key cannot be blank",
                "fingerprint" => "fingerprint cannot be blank",
                _ => "value cannot be blank",
            },
        });
    }
    Ok(())
}

fn parse_json_cell<T: for<'de> Deserialize<'de>>(value: String) -> rusqlite::Result<T> {
    serde_json::from_str(&value)
        .map_err(|err| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(err)))
}

fn validate_migration_sequence(versions: &[u32]) -> Result<()> {
    for (expected, version) in (1_u32..).zip(versions.iter().copied()) {
        if version != expected {
            return Err(StoreError::MigrationGap { missing: expected });
        }
    }
    Ok(())
}

fn timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("unix:{seconds}")
}

fn store_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{prefix}_{}_{}", std::process::id(), nanos)
}

fn write_jsonl_rows<W, I>(writer: &mut W, rows: I) -> Result<usize>
where
    W: Write,
    I: IntoIterator<Item = rusqlite::Result<serde_json::Value>>,
{
    let mut count = 0;
    for row in rows {
        let value = row?;
        serde_json::to_writer(&mut *writer, &value)?;
        writer.write_all(b"\n")?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests;
