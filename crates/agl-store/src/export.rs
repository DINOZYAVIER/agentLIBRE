use std::io::Write;

use rusqlite::types::Type;
use serde::Deserialize;
use serde_json::json;

use crate::{AglStore, Result, StoreDomain, StoreExportOptions};

impl AglStore {
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
        let mut count = self.write_query_jsonl(&mut writer, sql, |row| {
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
        count += self.write_query_jsonl(&mut writer, suggestions_sql, |row| {
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
        let mut count = self.write_query_jsonl(&mut writer, notes_sql, |row| {
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
        count += self.write_query_jsonl(&mut writer, links_sql, |row| {
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
        let mut count = self.write_query_jsonl(&mut writer, jobs_sql, |row| {
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
        count += self.write_query_jsonl(&mut writer, runs_sql, |row| {
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

        count += self.write_query_jsonl(
            &mut writer,
            "SELECT id, notify_ref, source_kind, source_id, dedupe_key, body, status, error, created_at, updated_at, delivered_at
             FROM matrix_notification_outbox
             ORDER BY created_at ASC, id ASC",
            |row| {
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
            },
        )?;
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
        let mut count = self.write_query_jsonl(&mut writer, request_sql, |row| {
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
        count += self.write_query_jsonl(&mut writer, grant_sql, |row| {
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
        Ok(count)
    }

    fn write_query_jsonl<W, F>(&self, writer: &mut W, sql: &str, mapper: F) -> Result<usize>
    where
        W: Write,
        F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<serde_json::Value>,
    {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], mapper)?;
        write_jsonl_rows(writer, rows)
    }
}

fn parse_json_cell<T: for<'de> Deserialize<'de>>(value: String) -> rusqlite::Result<T> {
    serde_json::from_str(&value)
        .map_err(|err| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(err)))
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
