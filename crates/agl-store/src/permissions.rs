use serde::Deserialize;

use rusqlite::{OptionalExtension, params};

use crate::{
    AglStore, PermissionGrantDraft, PermissionGrantRecord, PermissionGrantStatus,
    PermissionRequestDraft, PermissionRequestRecord, PermissionRequestStatus, Result, StoreError,
    store_id, timestamp, validate_non_blank, validate_non_empty_list,
};

impl AglStore {
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
