use std::collections::BTreeSet;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Deserialize;

use crate::connection::{configure_read_only, secure_database_files};
use crate::path::default_database_path;
use crate::{
    AglStore, AppliedStoreMigration, CURRENT_SCHEMA_VERSION, Result, STORE_MIGRATIONS, StoreError,
    StoreMigration, StoreMigrationReport, StoreSchemaStatus,
};

impl AglStore {
    pub fn migrate_at(root: impl AsRef<Path>) -> Result<StoreMigrationReport> {
        let store = Self::open_for_migration_at(root)?;
        let report = store.migrate()?;
        secure_database_files(store.database_path())?;
        Ok(report)
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
        configure_read_only(&conn)?;
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

    pub(crate) fn current_schema_status_at(root: impl AsRef<Path>) -> Result<StoreSchemaStatus> {
        let status = Self::schema_status_at(root)?;
        if !status.database_exists {
            return Err(StoreError::InvalidValue {
                field: "store",
                value: status.database_path.display().to_string(),
                reason: "store database does not exist; run core.store:migrate first",
            });
        }
        if status.migration_required {
            return Err(StoreError::InvalidValue {
                field: "store",
                value: format!(
                    "schema_version={:?}, current_schema_version={}",
                    status.schema_version, status.current_schema_version
                ),
                reason: "store schema migration required; run core.store:migrate first",
            });
        }
        Ok(status)
    }

    pub(crate) fn migrate(&self) -> Result<StoreMigrationReport> {
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

    pub(crate) fn applied_migration_versions(&self) -> Result<Vec<u32>> {
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
        // SQLite rewrites foreign-key declarations in dependent tables when a
        // referenced table is renamed while foreign-key enforcement is on.
        // Migration 17 intentionally performs the documented table-rebuild
        // sequence for `runs`; enforcement must therefore be disabled before
        // its transaction begins so those declarations continue to name the
        // replacement `runs` table rather than the temporary `runs_v16` table.
        let rebuilds_referenced_runs_table = migration.version == 17;
        if rebuilds_referenced_runs_table {
            self.conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
        }
        self.conn.execute_batch("BEGIN;")?;
        let preparation = match migration.version {
            19 => prepare_kernel_run_authority_migration(&self.conn),
            20 => prepare_domain_persistence_authority_migration(&self.conn),
            _ => Ok(()),
        };
        if let Err(error) = preparation {
            let _ = self.conn.execute_batch("ROLLBACK;");
            if rebuilds_referenced_runs_table {
                self.conn.execute_batch("PRAGMA foreign_keys = ON;")?;
            }
            return Err(error);
        }
        let batch = format!(
            r#"
            {sql}
            INSERT INTO schema_migrations(version, applied_at)
            VALUES ({version}, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'));
            PRAGMA user_version = {version};
            COMMIT;
            "#,
            sql = migration.sql,
            version = migration.version
        );
        let migration_result = self.conn.execute_batch(&batch);
        if migration_result.is_err() {
            let _ = self.conn.execute_batch("ROLLBACK;");
        }
        if rebuilds_referenced_runs_table {
            self.conn.execute_batch("PRAGMA foreign_keys = ON;")?;
            let enabled = self
                .conn
                .query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))?
                != 0;
            if !enabled {
                return Err(StoreError::InvalidValue {
                    field: "store migration",
                    value: migration.name.to_owned(),
                    reason: "foreign-key enforcement was not restored",
                });
            }
        }
        migration_result?;
        Ok(())
    }

    pub(crate) fn schema_version(&self) -> Result<u32> {
        let version = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))?;
        Ok(version)
    }
}

fn prepare_domain_persistence_authority_migration(connection: &Connection) -> Result<()> {
    validate_permission_requests_for_v20(connection)?;
    validate_permission_grants_for_v20(connection)?;

    connection.execute_batch(
        "CREATE TABLE agl175_matrix_migration (
            id TEXT PRIMARY KEY,
            payload_fingerprint TEXT NOT NULL,
            transaction_id TEXT NOT NULL
        );",
    )?;
    let mut statement = connection.prepare(
        "SELECT id, notify_ref, source_kind, source_id, dedupe_key, body
         FROM matrix_notification_outbox ORDER BY id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    for row in rows {
        let (id, notify_ref, source_kind, source_id, dedupe_key, body) = row?;
        let outbox_id =
            agl_matrix::MatrixOutboxId::new(id.clone()).map_err(|_| StoreError::InvalidValue {
                field: "matrix_notification_outbox.id",
                value: id.clone(),
                reason: "cannot migrate invalid Matrix outbox identity",
            })?;
        let draft = agl_matrix::MatrixOutboxDraft::new(
            notify_ref,
            source_kind,
            source_id,
            dedupe_key,
            body,
        )
        .map_err(|_| StoreError::InvalidValue {
            field: "matrix_notification_outbox",
            value: id.clone(),
            reason: "cannot migrate invalid Matrix outbox payload",
        })?;
        connection.execute(
            "INSERT INTO agl175_matrix_migration (id, payload_fingerprint, transaction_id)
             VALUES (?1, ?2, ?3)",
            params![
                id,
                draft.payload_fingerprint(),
                agl_matrix::stable_matrix_transaction_id(&outbox_id),
            ],
        )?;
    }
    Ok(())
}

fn validate_permission_requests_for_v20(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare(
        "SELECT id, requested_tools_json, max_operation_kind, state_effects_json,
                sensitive_inputs_json, scope_json, duration, status
         FROM permission_requests ORDER BY id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
        ))
    })?;
    for row in rows {
        let (id, tools, operation, effects, sensitive, scope, duration, state) = row?;
        parse_permission_json::<Vec<agl_kernel::ToolId>>(&id, "requested_tools_json", &tools)?;
        parse_permission_enum::<agl_kernel::OperationKind>(&id, "max_operation_kind", &operation)?;
        parse_permission_json::<std::collections::BTreeSet<agl_kernel::EffectId>>(
            &id,
            "state_effects_json",
            &effects,
        )?;
        parse_permission_json::<std::collections::BTreeSet<agl_kernel::SensitiveInput>>(
            &id,
            "sensitive_inputs_json",
            &sensitive,
        )?;
        parse_permission_json::<serde_json::Value>(&id, "scope_json", &scope)?;
        agl_permission::PermissionDuration::parse(&duration).map_err(|_| {
            invalid_permission_migration(&id, "duration", "invalid permission duration")
        })?;
        agl_permission::PermissionRequestState::parse(&state).map_err(|_| {
            invalid_permission_migration(&id, "status", "invalid permission request state")
        })?;
    }
    Ok(())
}

fn validate_permission_grants_for_v20(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare(
        "SELECT id, tool_id, max_operation_kind, state_effects_json,
                sensitive_inputs_json, scope_json, duration, status,
                last_admitted_run_id
         FROM permission_grants ORDER BY id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, Option<String>>(8)?,
        ))
    })?;
    for row in rows {
        let (id, tool, operation, effects, sensitive, scope, duration, status, run_id) = row?;
        parse_permission_enum::<agl_kernel::ToolId>(&id, "tool_id", &tool)?;
        parse_permission_enum::<agl_kernel::OperationKind>(&id, "max_operation_kind", &operation)?;
        parse_permission_json::<std::collections::BTreeSet<agl_kernel::EffectId>>(
            &id,
            "state_effects_json",
            &effects,
        )?;
        parse_permission_json::<std::collections::BTreeSet<agl_kernel::SensitiveInput>>(
            &id,
            "sensitive_inputs_json",
            &sensitive,
        )?;
        parse_permission_json::<serde_json::Value>(&id, "scope_json", &scope)?;
        let duration = agl_permission::PermissionDuration::parse(&duration).map_err(|_| {
            invalid_permission_migration(&id, "duration", "invalid permission duration")
        })?;
        if let Some(run_id) = &run_id {
            agl_ids::RunId::parse(run_id).map_err(|_| {
                invalid_permission_migration(
                    &id,
                    "last_admitted_run_id",
                    "invalid typed Run identity",
                )
            })?;
        }
        let compatible = matches!(
            (duration, status.as_str(), run_id.is_some()),
            (
                agl_permission::PermissionDuration::OneTurn,
                "active" | "revoked",
                false
            ) | (agl_permission::PermissionDuration::OneTurn, "expired", true)
                | (
                    agl_permission::PermissionDuration::Session,
                    "active" | "expired" | "revoked",
                    _
                )
        );
        if !compatible {
            return Err(invalid_permission_migration(
                &id,
                "duration/status/admission",
                "incompatible permission grant lifecycle metadata",
            ));
        }
    }
    Ok(())
}

fn parse_permission_json<T: serde::de::DeserializeOwned>(
    id: &str,
    field: &'static str,
    value: &str,
) -> Result<T> {
    serde_json::from_str(value)
        .map_err(|_| invalid_permission_migration(id, field, "invalid typed permission JSON"))
}

fn parse_permission_enum<T: serde::de::DeserializeOwned>(
    id: &str,
    field: &'static str,
    value: &str,
) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|_| invalid_permission_migration(id, field, "invalid typed permission value"))
}

fn invalid_permission_migration(id: &str, field: &'static str, reason: &'static str) -> StoreError {
    StoreError::InvalidValue {
        field,
        value: id.to_owned(),
        reason,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObsoleteStoreStatusRunInput {
    builtin: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObsoleteStoreStatusRequest {
    store_root: String,
}

fn prepare_kernel_run_authority_migration(connection: &Connection) -> Result<()> {
    let mut obsolete_roots = BTreeSet::new();
    {
        let mut statement = connection.prepare("SELECT id, kind, input_json FROM runs")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (run_id, kind, input_json) = row?;
            if kind == "cron"
                && serde_json::from_str::<ObsoleteStoreStatusRunInput>(&input_json)
                    .is_ok_and(|input| input.builtin == "store-status")
            {
                obsolete_roots.insert(run_id);
            }
        }
    }

    for run_id in &obsolete_roots {
        reject_obsolete_run_reference(
            connection,
            "SELECT EXISTS(SELECT 1 FROM runs WHERE parent_run_id = ?1)",
            run_id,
            "obsolete store-status run has a child run",
        )?;
        reject_obsolete_run_reference(
            connection,
            "SELECT EXISTS(SELECT 1 FROM content_attachments WHERE run_id = ?1)",
            run_id,
            "obsolete store-status run has a content attachment",
        )?;
        reject_obsolete_run_reference(
            connection,
            "SELECT EXISTS(SELECT 1 FROM permission_grants WHERE last_admitted_run_id = ?1)",
            run_id,
            "obsolete store-status run is referenced by a permission grant",
        )?;
    }

    let mut typed_steps = Vec::new();
    {
        let mut statement = connection.prepare(
            "SELECT id, run_id, turn_id, request_sequence, effect_kind, delivery_class,
                    request_json, result_json
             FROM run_steps ORDER BY run_id, request_sequence",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, u64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })?;
        for row in rows {
            let (step_id, run_id, turn_id, sequence, kind, delivery, request_json, result_json) =
                row?;
            if obsolete_roots.contains(&run_id) {
                let exact_request =
                    serde_json::from_str::<ObsoleteStoreStatusRequest>(&request_json)
                        .is_ok_and(|request| request.store_root == "private");
                if kind != "store_status" || !exact_request {
                    return invalid_run_migration(
                        step_id,
                        "obsolete store-status run contains an unexpected step",
                    );
                }
                continue;
            }

            let request: agl_kernel::TurnRequest =
                serde_json::from_str(&request_json).map_err(|_| StoreError::InvalidValue {
                    field: "run_steps.request_json",
                    value: step_id.clone(),
                    reason: "cannot migrate row to typed RunRequest",
                })?;
            if request.kind().as_str() != kind
                || request.key().sequence != sequence
                || turn_id.as_deref() != Some(request.key().turn_id.as_str())
            {
                return invalid_run_migration(
                    step_id,
                    "stored request identity does not match its RunStep columns",
                );
            }
            let delivery = agl_kernel::RunDelivery::parse(&delivery).ok_or_else(|| {
                StoreError::InvalidValue {
                    field: "run_steps.delivery_class",
                    value: delivery.clone(),
                    reason: "cannot migrate unknown Run delivery",
                }
            })?;
            let request = agl_kernel::RunRequest::new(delivery, request);
            let result = result_json
                .map(|raw| {
                    let result: agl_kernel::TurnRequestResult = serde_json::from_str(&raw)?;
                    agl_kernel::RunRequestResult::for_request(&request, result).map_err(|_| {
                        StoreError::InvalidValue {
                            field: "run_steps.result_json",
                            value: step_id.clone(),
                            reason: "stored result identity does not match its RunRequest",
                        }
                    })
                })
                .transpose()?;
            typed_steps.push((
                step_id,
                serde_json::to_string(&request)?,
                result
                    .map(|result| serde_json::to_string(&result))
                    .transpose()?,
            ));
        }
    }

    for (step_id, request_json, result_json) in typed_steps {
        connection.execute(
            "UPDATE run_steps SET request_json = ?2, result_json = ?3 WHERE id = ?1",
            params![step_id, request_json, result_json],
        )?;
    }

    for run_id in &obsolete_roots {
        let stale_result_ref = format!("run:{run_id}");
        let linked_cron_runs = {
            let mut statement = connection.prepare(
                "SELECT id, job_id, scheduled_for, status
                 FROM cron_runs WHERE supervisor_run_id = ?1 ORDER BY id",
            )?;
            let rows = statement.query_map([run_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        for (cron_run_id, job_id, scheduled_for, status) in linked_cron_runs {
            let active = matches!(status.as_str(), "queued" | "running");
            connection.execute(
                "UPDATE cron_runs
                 SET status = CASE WHEN ?3 THEN 'failed' ELSE status END,
                     started_at = CASE WHEN ?3
                         THEN COALESCE(started_at, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
                         ELSE started_at END,
                     finished_at = CASE WHEN ?3
                         THEN strftime('%Y-%m-%dT%H:%M:%SZ', 'now') ELSE finished_at END,
                     error = CASE WHEN ?3
                         THEN 'agl174 migration removed obsolete store-status supervisor run'
                         ELSE error END,
                     result_ref = CASE WHEN result_ref = ?2 THEN NULL ELSE result_ref END,
                     supervisor_run_id = NULL
                 WHERE id = ?1 AND supervisor_run_id = ?4",
                params![cron_run_id, stale_result_ref, active, run_id],
            )?;
            connection.execute(
                "UPDATE idempotency_keys
                 SET status = CASE WHEN ?4 THEN 'failed' ELSE status END,
                     result_ref = ?3, admitted_run_id = NULL,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now'),
                     last_error_code = CASE WHEN ?4
                         THEN 'agl174_obsolete_store_status_run_removed'
                         ELSE last_error_code END
                 WHERE namespace = 'core.cron:run' AND key = ?1 || ':' || ?2",
                params![job_id, scheduled_for, cron_run_id, active],
            )?;
        }
        connection.execute(
            "DELETE FROM idempotency_keys
             WHERE admitted_run_id = ?1 OR result_ref = ?2",
            params![run_id, stale_result_ref],
        )?;
        connection.execute("DELETE FROM runs WHERE id = ?1", [run_id])?;
    }
    Ok(())
}

fn reject_obsolete_run_reference(
    connection: &Connection,
    query: &str,
    run_id: &str,
    reason: &'static str,
) -> Result<()> {
    if connection.query_row(query, [run_id], |row| row.get::<_, bool>(0))? {
        return invalid_run_migration(run_id.to_owned(), reason);
    }
    Ok(())
}

fn invalid_run_migration<T>(value: String, reason: &'static str) -> Result<T> {
    Err(StoreError::InvalidValue {
        field: "store migration 019_kernel_run_authority",
        value,
        reason,
    })
}

fn validate_migration_sequence(versions: &[u32]) -> Result<()> {
    for (expected, version) in (1_u32..).zip(versions.iter().copied()) {
        if version != expected {
            return Err(StoreError::MigrationGap { missing: expected });
        }
    }
    Ok(())
}
