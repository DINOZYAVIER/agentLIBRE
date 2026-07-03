use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params, types::Type};
use serde::Deserialize;
use serde_json::json;

mod error;
mod migrations;
mod path;
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

    pub fn health(&self) -> Result<StoreHealth> {
        Ok(StoreHealth {
            database_path: self.database_path.clone(),
            migration_version: self.schema_version()?,
        })
    }

    pub fn status(&self) -> Result<StoreStatus> {
        Ok(StoreStatus {
            database_path: self.database_path.clone(),
            schema_version: self.schema_version()?,
            domains: StoreDomain::all()
                .into_iter()
                .map(|domain| self.domain_health(domain))
                .collect::<Result<Vec<_>>>()?,
            idempotency: self.idempotency_health()?,
        })
    }

    pub fn domain_health(&self, domain: StoreDomain) -> Result<StoreDomainHealth> {
        let (total_rows, active_rows) = self.domain_counts(domain)?;
        Ok(StoreDomainHealth {
            domain,
            status: StoreDomainStatus::Ok,
            total_rows,
            active_rows,
        })
    }

    pub fn idempotency_health(&self) -> Result<StoreIdempotencyHealth> {
        let stale_in_progress = self.stale_in_progress_idempotency_records()?;
        Ok(StoreIdempotencyHealth {
            in_progress: stale_in_progress.len() as u64,
            stale_in_progress,
        })
    }

    pub fn stale_in_progress_idempotency_records(
        &self,
    ) -> Result<Vec<StoreStaleIdempotencyRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT namespace, key, created_at, updated_at
             FROM idempotency_keys
             WHERE status = 'in_progress'
             ORDER BY updated_at ASC, namespace ASC, key ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(StoreStaleIdempotencyRecord {
                namespace: row.get(0)?,
                key: row.get(1)?,
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
            })
        })?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row?);
        }
        Ok(records)
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

    pub fn begin_idempotency(
        &self,
        namespace: &str,
        key: &str,
        fingerprint: &str,
    ) -> Result<IdempotencyOutcome> {
        validate_idempotency_part(namespace, "namespace")?;
        validate_idempotency_part(key, "key")?;
        validate_idempotency_part(fingerprint, "fingerprint")?;

        if let Some(existing) = self.idempotency_record(namespace, key)? {
            if existing.fingerprint == fingerprint {
                return Ok(IdempotencyOutcome::Replayed(existing));
            }
            return Err(StoreError::IdempotencyConflict {
                namespace: namespace.to_string(),
                key: key.to_string(),
                existing_fingerprint: existing.fingerprint,
                requested_fingerprint: fingerprint.to_string(),
            });
        }

        let now = timestamp();
        self.conn.execute(
            "INSERT INTO idempotency_keys
             (namespace, key, fingerprint, status, result_ref, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5)",
            params![
                namespace,
                key,
                fingerprint,
                IdempotencyStatus::InProgress.as_str(),
                now
            ],
        )?;
        let record = self
            .idempotency_record(namespace, key)?
            .expect("inserted idempotency key should be readable");
        Ok(IdempotencyOutcome::Inserted(record))
    }

    pub fn complete_idempotency(
        &self,
        namespace: &str,
        key: &str,
        result_ref: Option<&str>,
    ) -> Result<IdempotencyRecord> {
        self.finish_idempotency(namespace, key, IdempotencyStatus::Completed, result_ref)
    }

    pub fn fail_idempotency(
        &self,
        namespace: &str,
        key: &str,
        result_ref: Option<&str>,
    ) -> Result<IdempotencyRecord> {
        self.finish_idempotency(namespace, key, IdempotencyStatus::Failed, result_ref)
    }

    pub fn skip_idempotency(
        &self,
        namespace: &str,
        key: &str,
        result_ref: Option<&str>,
    ) -> Result<IdempotencyRecord> {
        self.finish_idempotency(namespace, key, IdempotencyStatus::Skipped, result_ref)
    }

    fn finish_idempotency(
        &self,
        namespace: &str,
        key: &str,
        status: IdempotencyStatus,
        result_ref: Option<&str>,
    ) -> Result<IdempotencyRecord> {
        validate_idempotency_part(namespace, "namespace")?;
        validate_idempotency_part(key, "key")?;
        let now = timestamp();
        self.conn.execute(
            "UPDATE idempotency_keys
             SET status = ?3, result_ref = ?4, updated_at = ?5
             WHERE namespace = ?1 AND key = ?2",
            params![namespace, key, status.as_str(), result_ref, now],
        )?;
        self.idempotency_record(namespace, key)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("idempotency key {namespace}/{key}"),
            })
    }

    pub fn idempotency_record(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<IdempotencyRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT namespace, key, fingerprint, status, result_ref, created_at, updated_at
             FROM idempotency_keys
             WHERE namespace = ?1 AND key = ?2",
        )?;
        stmt.query_row(params![namespace, key], |row| {
            let status: String = row.get(3)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                status,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })
        .optional()?
        .map(
            |(namespace, key, fingerprint, status, result_ref, created_at, updated_at)| {
                Ok(IdempotencyRecord {
                    namespace,
                    key,
                    fingerprint,
                    status: IdempotencyStatus::parse(&status)?,
                    result_ref,
                    created_at,
                    updated_at,
                })
            },
        )
        .transpose()
    }

    pub fn enqueue_matrix_notification(
        &self,
        draft: MatrixNotificationOutboxDraft,
    ) -> Result<MatrixNotificationOutboxItem> {
        validate_non_blank(&draft.notify_ref, "matrix_notification_outbox.notify_ref")?;
        validate_non_blank(&draft.source_kind, "matrix_notification_outbox.source_kind")?;
        validate_non_blank(&draft.source_id, "matrix_notification_outbox.source_id")?;
        validate_non_blank(&draft.dedupe_key, "matrix_notification_outbox.dedupe_key")?;
        validate_non_blank(&draft.body, "matrix_notification_outbox.body")?;

        let id = store_id("matrix_outbox");
        let now = timestamp();
        self.conn.execute(
            "INSERT INTO matrix_notification_outbox
             (id, notify_ref, source_kind, source_id, dedupe_key, body, status, error, created_at, updated_at, delivered_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued', NULL, ?7, ?7, NULL)
             ON CONFLICT(dedupe_key) DO NOTHING",
            params![
                &id,
                &draft.notify_ref,
                &draft.source_kind,
                &draft.source_id,
                &draft.dedupe_key,
                &draft.body,
                &now
            ],
        )?;
        self.matrix_notification_by_dedupe_key(&draft.dedupe_key)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("matrix notification outbox {}", draft.dedupe_key),
            })
    }

    pub fn queued_matrix_notifications(
        &self,
        limit: usize,
    ) -> Result<Vec<MatrixNotificationOutboxItem>> {
        let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare(
            "SELECT id, notify_ref, source_kind, source_id, dedupe_key, body, status, error, created_at, updated_at, delivered_at
             FROM matrix_notification_outbox
             WHERE status = 'queued'
             ORDER BY created_at ASC, id ASC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], matrix_notification_from_row)?;
        let mut notifications = Vec::new();
        for row in rows {
            notifications.push(row??);
        }
        Ok(notifications)
    }

    pub fn queued_matrix_notifications_page(
        &self,
        limit: usize,
    ) -> Result<(Vec<MatrixNotificationOutboxItem>, bool)> {
        let limit = limit.max(1);
        let mut notifications = self.queued_matrix_notifications(limit.saturating_add(1))?;
        let truncated = notifications.len() > limit;
        if truncated {
            notifications.truncate(limit);
        }
        Ok((notifications, truncated))
    }

    pub fn mark_matrix_notification_sent(&self, id: &str) -> Result<MatrixNotificationOutboxItem> {
        validate_non_blank(id, "matrix_notification_outbox.id")?;
        let now = timestamp();
        self.conn.execute(
            "UPDATE matrix_notification_outbox
             SET status = 'sent', error = NULL, updated_at = ?2, delivered_at = ?2
             WHERE id = ?1",
            params![id, now],
        )?;
        self.matrix_notification(id)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("matrix notification outbox {id}"),
            })
    }

    pub fn mark_matrix_notification_failed(
        &self,
        id: &str,
        error: &str,
    ) -> Result<MatrixNotificationOutboxItem> {
        validate_non_blank(id, "matrix_notification_outbox.id")?;
        validate_non_blank(error, "matrix_notification_outbox.error")?;
        let now = timestamp();
        self.conn.execute(
            "UPDATE matrix_notification_outbox
             SET status = 'failed', error = ?2, updated_at = ?3
             WHERE id = ?1",
            params![id, error, now],
        )?;
        self.matrix_notification(id)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("matrix notification outbox {id}"),
            })
    }

    pub fn matrix_notification(&self, id: &str) -> Result<Option<MatrixNotificationOutboxItem>> {
        validate_non_blank(id, "matrix_notification_outbox.id")?;
        self.conn
            .query_row(
                "SELECT id, notify_ref, source_kind, source_id, dedupe_key, body, status, error, created_at, updated_at, delivered_at
                 FROM matrix_notification_outbox
                 WHERE id = ?1",
                params![id],
                matrix_notification_from_row,
            )
            .optional()?
            .transpose()
    }

    pub fn matrix_notification_by_dedupe_key(
        &self,
        dedupe_key: &str,
    ) -> Result<Option<MatrixNotificationOutboxItem>> {
        validate_non_blank(dedupe_key, "matrix_notification_outbox.dedupe_key")?;
        self.conn
            .query_row(
                "SELECT id, notify_ref, source_kind, source_id, dedupe_key, body, status, error, created_at, updated_at, delivered_at
                 FROM matrix_notification_outbox
                 WHERE dedupe_key = ?1",
                params![dedupe_key],
                matrix_notification_from_row,
            )
            .optional()?
            .transpose()
    }

    pub fn create_permission_request(
        &self,
        draft: PermissionRequestDraft,
    ) -> Result<PermissionRequestRecord> {
        validate_non_empty_list(
            &draft.requested_tools,
            "permission_requests.requested_tools",
        )?;
        validate_non_blank(
            &draft.max_operation_kind,
            "permission_requests.max_operation_kind",
        )?;
        validate_non_blank(&draft.duration, "permission_requests.duration")?;
        validate_non_blank(&draft.reason, "permission_requests.reason")?;
        validate_non_blank(&draft.requester_ref, "permission_requests.requester_ref")?;

        let id = store_id("permission_request");
        let now = timestamp();
        let requested_tools_json = serde_json::to_string(&draft.requested_tools)?;
        let state_effects_json = serde_json::to_string(&draft.state_effects)?;
        let scope_json = serde_json::to_string(&draft.scope)?;
        self.conn.execute(
            "INSERT INTO permission_requests
             (id, requested_tools_json, max_operation_kind, state_effects_json, scope_json, duration, reason, requester_ref, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', ?9, ?9, NULL, NULL, NULL)",
            params![
                &id,
                &requested_tools_json,
                &draft.max_operation_kind,
                &state_effects_json,
                &scope_json,
                &draft.duration,
                &draft.reason,
                &draft.requester_ref,
                &now
            ],
        )?;
        self.permission_request(&id)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("permission request {id}"),
            })
    }

    pub fn permission_request(&self, id: &str) -> Result<Option<PermissionRequestRecord>> {
        validate_non_blank(id, "permission_requests.id")?;
        self.conn
            .query_row(
                "SELECT id, requested_tools_json, max_operation_kind, state_effects_json, scope_json, duration, reason, requester_ref, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
                 FROM permission_requests
                 WHERE id = ?1",
                params![id],
                permission_request_from_row,
            )
            .optional()?
            .transpose()
    }

    pub fn pending_permission_requests(&self) -> Result<Vec<PermissionRequestRecord>> {
        self.permission_requests_by_status(PermissionRequestStatus::Pending)
    }

    pub fn permission_requests_by_status(
        &self,
        status: PermissionRequestStatus,
    ) -> Result<Vec<PermissionRequestRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, requested_tools_json, max_operation_kind, state_effects_json, scope_json, duration, reason, requester_ref, status, created_at, updated_at, resolved_at, resolution_ref, resolution_note
             FROM permission_requests
             WHERE status = ?1
             ORDER BY updated_at ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![status.as_str()], permission_request_from_row)?;
        let mut requests = Vec::new();
        for row in rows {
            requests.push(row??);
        }
        Ok(requests)
    }

    pub fn create_permission_grant(
        &self,
        draft: PermissionGrantDraft,
    ) -> Result<PermissionGrantRecord> {
        validate_non_blank(&draft.tool_id, "permission_grants.tool_id")?;
        validate_non_blank(
            &draft.max_operation_kind,
            "permission_grants.max_operation_kind",
        )?;
        validate_non_blank(&draft.duration, "permission_grants.duration")?;
        validate_non_blank(&draft.granted_by_ref, "permission_grants.granted_by_ref")?;
        if let Some(request_id) = &draft.request_id {
            validate_non_blank(request_id, "permission_grants.request_id")?;
        }

        let id = store_id("permission_grant");
        let now = timestamp();
        let state_effects_json = serde_json::to_string(&draft.state_effects)?;
        let scope_json = serde_json::to_string(&draft.scope)?;
        self.conn.execute(
            "INSERT INTO permission_grants
             (id, request_id, tool_id, max_operation_kind, state_effects_json, scope_json, duration, granted_by_ref, status, created_at, updated_at, revoked_at, revoke_ref)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'active', ?9, ?9, NULL, NULL)",
            params![
                &id,
                &draft.request_id,
                &draft.tool_id,
                &draft.max_operation_kind,
                &state_effects_json,
                &scope_json,
                &draft.duration,
                &draft.granted_by_ref,
                &now
            ],
        )?;
        self.permission_grant(&id)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("permission grant {id}"),
            })
    }

    pub fn grant_permission_request(
        &self,
        request_id: &str,
        granted_by_ref: &str,
        resolution_ref: Option<&str>,
    ) -> Result<Vec<PermissionGrantRecord>> {
        validate_non_blank(request_id, "permission_requests.id")?;
        validate_non_blank(granted_by_ref, "permission_grants.granted_by_ref")?;
        let request = self
            .permission_request(request_id)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("permission request {request_id}"),
            })?;
        if request.status != PermissionRequestStatus::Pending {
            return Err(StoreError::InvalidValue {
                field: "permission_requests.status",
                value: request.status.as_str().to_string(),
                reason: "permission request is not pending",
            });
        }

        let mut grants = Vec::with_capacity(request.requested_tools.len());
        for tool_id in &request.requested_tools {
            grants.push(self.create_permission_grant(PermissionGrantDraft {
                request_id: Some(request.id.clone()),
                tool_id: tool_id.clone(),
                max_operation_kind: request.max_operation_kind.clone(),
                state_effects: request.state_effects.clone(),
                scope: request.scope.clone(),
                duration: request.duration.clone(),
                granted_by_ref: granted_by_ref.to_string(),
            })?);
        }
        self.resolve_permission_request(
            request_id,
            PermissionRequestStatus::Granted,
            resolution_ref,
            None,
        )?;
        Ok(grants)
    }

    pub fn deny_permission_request(
        &self,
        request_id: &str,
        resolution_ref: Option<&str>,
        note: Option<&str>,
    ) -> Result<PermissionRequestRecord> {
        self.resolve_permission_request(
            request_id,
            PermissionRequestStatus::Denied,
            resolution_ref,
            note,
        )
    }

    pub fn revoke_permission_request(
        &self,
        request_id: &str,
        resolution_ref: Option<&str>,
        note: Option<&str>,
    ) -> Result<PermissionRequestRecord> {
        self.resolve_permission_request(
            request_id,
            PermissionRequestStatus::Revoked,
            resolution_ref,
            note,
        )
    }

    fn resolve_permission_request(
        &self,
        request_id: &str,
        status: PermissionRequestStatus,
        resolution_ref: Option<&str>,
        note: Option<&str>,
    ) -> Result<PermissionRequestRecord> {
        validate_non_blank(request_id, "permission_requests.id")?;
        let now = timestamp();
        self.conn.execute(
            "UPDATE permission_requests
             SET status = ?2, updated_at = ?3, resolved_at = ?3, resolution_ref = ?4, resolution_note = ?5
             WHERE id = ?1",
            params![request_id, status.as_str(), now, resolution_ref, note],
        )?;
        self.permission_request(request_id)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("permission request {request_id}"),
            })
    }

    pub fn permission_grant(&self, id: &str) -> Result<Option<PermissionGrantRecord>> {
        validate_non_blank(id, "permission_grants.id")?;
        self.conn
            .query_row(
                "SELECT id, request_id, tool_id, max_operation_kind, state_effects_json, scope_json, duration, granted_by_ref, status, created_at, updated_at, revoked_at, revoke_ref, admitted_at, last_admitted_run_id, consumed_at
                 FROM permission_grants
                 WHERE id = ?1",
                params![id],
                permission_grant_from_row,
            )
            .optional()?
            .transpose()
    }

    pub fn active_permission_grants(&self) -> Result<Vec<PermissionGrantRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, request_id, tool_id, max_operation_kind, state_effects_json, scope_json, duration, granted_by_ref, status, created_at, updated_at, revoked_at, revoke_ref, admitted_at, last_admitted_run_id, consumed_at
             FROM permission_grants
             WHERE status = 'active'
             ORDER BY updated_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], permission_grant_from_row)?;
        let mut grants = Vec::new();
        for row in rows {
            grants.push(row??);
        }
        Ok(grants)
    }

    pub fn revoke_permission_grant(
        &self,
        grant_id: &str,
        revoke_ref: Option<&str>,
    ) -> Result<PermissionGrantRecord> {
        validate_non_blank(grant_id, "permission_grants.id")?;
        let now = timestamp();
        self.conn.execute(
            "UPDATE permission_grants
             SET status = 'revoked', updated_at = ?2, revoked_at = ?2, revoke_ref = ?3
             WHERE id = ?1",
            params![grant_id, now, revoke_ref],
        )?;
        self.permission_grant(grant_id)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("permission grant {grant_id}"),
            })
    }

    pub fn admit_permission_grant(
        &self,
        grant_id: &str,
        run_id: &str,
    ) -> Result<PermissionGrantRecord> {
        validate_non_blank(grant_id, "permission_grants.id")?;
        validate_non_blank(run_id, "permission_grants.last_admitted_run_id")?;
        let now = timestamp();
        self.conn.execute(
            "UPDATE permission_grants
             SET status = 'expired',
                 updated_at = ?2,
                 admitted_at = COALESCE(admitted_at, ?2),
                 last_admitted_run_id = ?3,
                 consumed_at = ?2
             WHERE id = ?1 AND status = 'active'",
            params![grant_id, now, run_id],
        )?;
        self.permission_grant(grant_id)?
            .ok_or_else(|| StoreError::NotFound {
                resource: format!("permission grant {grant_id}"),
            })
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

    fn domain_counts(&self, domain: StoreDomain) -> Result<(u64, u64)> {
        match domain {
            StoreDomain::Memory => Ok((
                self.query_count("SELECT COUNT(*) FROM memory_entries")?
                    + self.query_count("SELECT COUNT(*) FROM memory_suggestions")?,
                self.query_count("SELECT COUNT(*) FROM memory_entries WHERE deleted_at IS NULL")?
                    + self.query_count(
                        "SELECT COUNT(*) FROM memory_suggestions WHERE status = 'pending'",
                    )?,
            )),
            StoreDomain::Notes => Ok((
                self.query_count("SELECT COUNT(*) FROM notes")?,
                self.query_count("SELECT COUNT(*) FROM notes WHERE deleted_at IS NULL")?,
            )),
            StoreDomain::Cron => Ok((
                self.query_count("SELECT COUNT(*) FROM cron_jobs")?
                    + self.query_count("SELECT COUNT(*) FROM cron_runs")?
                    + self.query_count("SELECT COUNT(*) FROM matrix_notification_outbox")?,
                self.query_count("SELECT COUNT(*) FROM cron_jobs WHERE deleted_at IS NULL")?
                    + self.query_count(
                        "SELECT COUNT(*) FROM matrix_notification_outbox WHERE status = 'queued'",
                    )?,
            )),
            StoreDomain::Permissions => Ok((
                self.query_count("SELECT COUNT(*) FROM permission_requests")?
                    + self.query_count("SELECT COUNT(*) FROM permission_grants")?,
                self.query_count(
                    "SELECT COUNT(*) FROM permission_requests WHERE status = 'pending'",
                )? + self.query_count(
                    "SELECT COUNT(*) FROM permission_grants WHERE status = 'active'",
                )?,
            )),
        }
    }

    fn query_count(&self, sql: &str) -> Result<u64> {
        let count = self.conn.query_row(sql, [], |row| row.get::<_, i64>(0))?;
        Ok(u64::try_from(count).unwrap_or(0))
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

fn validate_idempotency_part(value: &str, field: &'static str) -> Result<()> {
    validate_non_blank(value, field)
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

fn matrix_notification_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<MatrixNotificationOutboxItem>> {
    let status: String = row.get(6)?;
    Ok((|| {
        Ok(MatrixNotificationOutboxItem {
            id: row.get(0)?,
            notify_ref: row.get(1)?,
            source_kind: row.get(2)?,
            source_id: row.get(3)?,
            dedupe_key: row.get(4)?,
            body: row.get(5)?,
            status: MatrixNotificationOutboxStatus::parse(&status)?,
            error: row.get(7)?,
            created_at: row.get(8)?,
            updated_at: row.get(9)?,
            delivered_at: row.get(10)?,
        })
    })())
}

fn permission_request_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<PermissionRequestRecord>> {
    let requested_tools_json: String = row.get(1)?;
    let state_effects_json: String = row.get(3)?;
    let scope_json: String = row.get(4)?;
    let status: String = row.get(8)?;
    Ok((|| {
        Ok(PermissionRequestRecord {
            id: row.get(0)?,
            requested_tools: parse_json_store(&requested_tools_json)?,
            max_operation_kind: row.get(2)?,
            state_effects: parse_json_store(&state_effects_json)?,
            scope: parse_json_store(&scope_json)?,
            duration: row.get(5)?,
            reason: row.get(6)?,
            requester_ref: row.get(7)?,
            status: PermissionRequestStatus::parse(&status)?,
            created_at: row.get(9)?,
            updated_at: row.get(10)?,
            resolved_at: row.get(11)?,
            resolution_ref: row.get(12)?,
            resolution_note: row.get(13)?,
        })
    })())
}

fn permission_grant_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Result<PermissionGrantRecord>> {
    let state_effects_json: String = row.get(4)?;
    let scope_json: String = row.get(5)?;
    let status: String = row.get(8)?;
    Ok((|| {
        Ok(PermissionGrantRecord {
            id: row.get(0)?,
            request_id: row.get(1)?,
            tool_id: row.get(2)?,
            max_operation_kind: row.get(3)?,
            state_effects: parse_json_store(&state_effects_json)?,
            scope: parse_json_store(&scope_json)?,
            duration: row.get(6)?,
            granted_by_ref: row.get(7)?,
            status: PermissionGrantStatus::parse(&status)?,
            created_at: row.get(9)?,
            updated_at: row.get(10)?,
            revoked_at: row.get(11)?,
            revoke_ref: row.get(12)?,
            admitted_at: row.get(13)?,
            last_admitted_run_id: row.get(14)?,
            consumed_at: row.get(15)?,
        })
    })())
}

fn parse_json_store<T: for<'de> Deserialize<'de>>(value: &str) -> Result<T> {
    serde_json::from_str(value).map_err(StoreError::from)
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
