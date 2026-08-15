use std::collections::BTreeSet;

use agl_ids::{RunId, SessionId};
use agl_permission::{
    PermissionDuration, PermissionError, PermissionGrantDraft, PermissionGrantMachine,
    PermissionGrantRecord, PermissionGrantState, PermissionMachineError, PermissionOperationId,
    PermissionRepository, PermissionRequestDraft, PermissionRequestMachine,
    PermissionRequestRecord, PermissionRequestResolution, PermissionRequestState,
    PermissionRevision, permission_grant_is_live_for_run,
};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::util::{store_id, timestamp};
use crate::{AglStore, StoreHandle};

impl PermissionRepository for StoreHandle {
    fn create_request(
        &self,
        draft: PermissionRequestDraft,
    ) -> Result<PermissionRequestRecord, PermissionError> {
        create_request(&*self.lock().map_err(repository_error)?, draft)
    }

    fn request(&self, id: &str) -> Result<Option<PermissionRequestRecord>, PermissionError> {
        request(&*self.lock().map_err(repository_error)?, id)
    }

    fn requests_by_state(
        &self,
        state: PermissionRequestState,
    ) -> Result<Vec<PermissionRequestRecord>, PermissionError> {
        requests_by_state(&*self.lock().map_err(repository_error)?, state)
    }

    fn grant_request(
        &self,
        request_id: &str,
        granted_by_ref: &str,
        operation_id: PermissionOperationId,
        resolution_ref: Option<&str>,
    ) -> Result<Vec<PermissionGrantRecord>, PermissionError> {
        grant_request(
            &*self.lock().map_err(repository_error)?,
            request_id,
            granted_by_ref,
            operation_id,
            resolution_ref,
        )
    }

    fn resolve_request(
        &self,
        request_id: &str,
        resolution: PermissionRequestResolution,
        operation_id: PermissionOperationId,
        resolution_ref: Option<&str>,
        note: Option<&str>,
    ) -> Result<PermissionRequestRecord, PermissionError> {
        resolve_request(
            &*self.lock().map_err(repository_error)?,
            request_id,
            resolution,
            operation_id,
            resolution_ref,
            note,
        )
    }

    fn create_grant(
        &self,
        draft: PermissionGrantDraft,
    ) -> Result<PermissionGrantRecord, PermissionError> {
        create_grant(&*self.lock().map_err(repository_error)?, draft)
    }

    fn grant(&self, id: &str) -> Result<Option<PermissionGrantRecord>, PermissionError> {
        grant(&*self.lock().map_err(repository_error)?, id)
    }

    fn active_grants(&self) -> Result<Vec<PermissionGrantRecord>, PermissionError> {
        active_grants(&*self.lock().map_err(repository_error)?)
    }

    fn admit_grant(
        &self,
        grant_id: &str,
        run_id: &RunId,
        operation_id: PermissionOperationId,
    ) -> Result<PermissionGrantRecord, PermissionError> {
        admit_grant(
            &*self.lock().map_err(repository_error)?,
            grant_id,
            run_id,
            operation_id,
        )
    }

    fn revoke_grant(
        &self,
        grant_id: &str,
        operation_id: PermissionOperationId,
        revoke_ref: Option<&str>,
    ) -> Result<PermissionGrantRecord, PermissionError> {
        revoke_grant(
            &*self.lock().map_err(repository_error)?,
            grant_id,
            operation_id,
            revoke_ref,
        )
    }

    fn expire_session_grants(&self, session_id: &SessionId) -> Result<usize, PermissionError> {
        expire_session_grants(&*self.lock().map_err(repository_error)?, session_id)
    }

    fn live_process_grant_ids(&self) -> Result<BTreeSet<String>, PermissionError> {
        live_process_grant_ids(&*self.lock().map_err(repository_error)?)
    }
}

fn create_request(
    store: &AglStore,
    draft: PermissionRequestDraft,
) -> Result<PermissionRequestRecord, PermissionError> {
    validate_request_draft(&draft)?;
    let id = store_id("permission_request");
    let now = timestamp();
    store
        .connection()
        .execute(
            "INSERT INTO permission_requests
             (id, requested_tools_json, max_operation_kind, state_effects_json,
              sensitive_inputs_json, scope_json, duration, reason, requester_ref,
              state, revision, created_at, updated_at, resolved_at, resolution_ref,
              resolution_note, transition_operation_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending', 1,
                     ?10, ?10, NULL, NULL, NULL, NULL)",
            params![
                id,
                json(&draft.requested_tools)?,
                draft.max_operation_kind.as_str(),
                json(&draft.state_effects)?,
                json(&draft.sensitive_inputs)?,
                json(&draft.scope)?,
                draft.duration.as_str(),
                draft.reason,
                draft.requester_ref,
                now,
            ],
        )
        .map_err(repository_error)?;
    request(store, &id)?.ok_or(PermissionError::NotFound { id })
}

fn request(store: &AglStore, id: &str) -> Result<Option<PermissionRequestRecord>, PermissionError> {
    request_on_connection(store.connection(), id)
}

fn requests_by_state(
    store: &AglStore,
    state: PermissionRequestState,
) -> Result<Vec<PermissionRequestRecord>, PermissionError> {
    let mut statement = store
        .connection()
        .prepare(
            "SELECT id, requested_tools_json, max_operation_kind, state_effects_json,
                    sensitive_inputs_json, scope_json, duration, reason, requester_ref,
                    state, revision, created_at, updated_at, resolved_at, resolution_ref,
                    resolution_note, transition_operation_id
             FROM permission_requests WHERE state = ?1 ORDER BY updated_at, id",
        )
        .map_err(repository_error)?;
    let rows = statement
        .query_map(params![state.as_str()], request_from_row)
        .map_err(repository_error)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(repository_error)
}

fn grant_request(
    store: &AglStore,
    request_id: &str,
    granted_by_ref: &str,
    operation_id: PermissionOperationId,
    resolution_ref: Option<&str>,
) -> Result<Vec<PermissionGrantRecord>, PermissionError> {
    validate_bounded("granted_by_ref", granted_by_ref)?;
    let fingerprint = format!(
        "grant_request\0{request_id}\0{granted_by_ref}\0{}",
        resolution_ref.unwrap_or("")
    );
    let tx = store
        .connection()
        .unchecked_transaction()
        .map_err(repository_error)?;
    if operation_replayed(&tx, &operation_id, "request", request_id, &fingerprint)? {
        return grants_for_request(&tx, request_id);
    }
    let current =
        request_on_connection(&tx, request_id)?.ok_or_else(|| PermissionError::NotFound {
            id: request_id.to_owned(),
        })?;
    let mut machine = PermissionRequestMachine::restore(current.state, current.revision);
    let transition = machine.resolve(operation_id.clone(), PermissionRequestResolution::Granted)?;
    let mut grants = Vec::with_capacity(current.requested_tools.len());
    for tool_id in &current.requested_tools {
        grants.push(insert_grant_on_connection(
            &tx,
            PermissionGrantDraft {
                request_id: Some(current.id.clone()),
                tool_id: tool_id.clone(),
                max_operation_kind: current.max_operation_kind,
                state_effects: current.state_effects.clone(),
                sensitive_inputs: current.sensitive_inputs.clone(),
                scope: current.scope.clone(),
                duration: current.duration,
                granted_by_ref: granted_by_ref.to_owned(),
            },
        )?);
    }
    persist_request_transition(
        &tx,
        &current,
        transition.new_state,
        transition.new_revision,
        &operation_id,
        resolution_ref,
        None,
    )?;
    insert_operation(&tx, &operation_id, "request", request_id, &fingerprint)?;
    tx.commit().map_err(repository_error)?;
    Ok(grants)
}

fn resolve_request(
    store: &AglStore,
    request_id: &str,
    resolution: PermissionRequestResolution,
    operation_id: PermissionOperationId,
    resolution_ref: Option<&str>,
    note: Option<&str>,
) -> Result<PermissionRequestRecord, PermissionError> {
    if resolution == PermissionRequestResolution::Granted {
        return Err(PermissionError::Machine(
            PermissionMachineError::InvalidTransition {
                reason: "granted resolution must atomically create grants".to_owned(),
            },
        ));
    }
    if let Some(note) = note {
        validate_bounded("resolution_note", note)?;
    }
    let fingerprint = format!(
        "resolve_request\0{request_id}\0{resolution:?}\0{}\0{}",
        resolution_ref.unwrap_or(""),
        note.unwrap_or("")
    );
    let tx = store
        .connection()
        .unchecked_transaction()
        .map_err(repository_error)?;
    if operation_replayed(&tx, &operation_id, "request", request_id, &fingerprint)? {
        return request_on_connection(&tx, request_id)?.ok_or_else(|| PermissionError::NotFound {
            id: request_id.to_owned(),
        });
    }
    let current =
        request_on_connection(&tx, request_id)?.ok_or_else(|| PermissionError::NotFound {
            id: request_id.to_owned(),
        })?;
    let mut machine = PermissionRequestMachine::restore(current.state, current.revision);
    let transition = machine.resolve(operation_id.clone(), resolution)?;
    persist_request_transition(
        &tx,
        &current,
        transition.new_state,
        transition.new_revision,
        &operation_id,
        resolution_ref,
        note,
    )?;
    insert_operation(&tx, &operation_id, "request", request_id, &fingerprint)?;
    let result =
        request_on_connection(&tx, request_id)?.ok_or_else(|| PermissionError::NotFound {
            id: request_id.to_owned(),
        })?;
    tx.commit().map_err(repository_error)?;
    Ok(result)
}

fn create_grant(
    store: &AglStore,
    draft: PermissionGrantDraft,
) -> Result<PermissionGrantRecord, PermissionError> {
    insert_grant_on_connection(store.connection(), draft)
}

fn grant(store: &AglStore, id: &str) -> Result<Option<PermissionGrantRecord>, PermissionError> {
    grant_on_connection(store.connection(), id)
}

fn active_grants(store: &AglStore) -> Result<Vec<PermissionGrantRecord>, PermissionError> {
    let mut statement = store
        .connection()
        .prepare(
            "SELECT id, request_id, tool_id, max_operation_kind, state_effects_json,
                    sensitive_inputs_json, scope_json, duration, granted_by_ref, state,
                    revision, created_at, updated_at, revoked_at, revoke_ref, admitted_at,
                    last_admitted_run_id, consumed_at, transition_operation_id
             FROM permission_grants WHERE state = 'active' ORDER BY updated_at, id",
        )
        .map_err(repository_error)?;
    let rows = statement
        .query_map([], grant_from_row)
        .map_err(repository_error)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(repository_error)
}

fn admit_grant(
    store: &AglStore,
    grant_id: &str,
    run_id: &RunId,
    operation_id: PermissionOperationId,
) -> Result<PermissionGrantRecord, PermissionError> {
    if store.run(run_id).map_err(repository_error)?.is_none() {
        return Err(PermissionError::NotFound {
            id: run_id.to_string(),
        });
    }
    let fingerprint = format!("admit_grant\0{grant_id}\0{run_id}");
    let tx = store
        .connection()
        .unchecked_transaction()
        .map_err(repository_error)?;
    if operation_replayed(&tx, &operation_id, "grant", grant_id, &fingerprint)? {
        return grant_on_connection(&tx, grant_id)?.ok_or_else(|| PermissionError::NotFound {
            id: grant_id.to_owned(),
        });
    }
    let current = grant_on_connection(&tx, grant_id)?.ok_or_else(|| PermissionError::NotFound {
        id: grant_id.to_owned(),
    })?;
    let mut machine =
        PermissionGrantMachine::restore(current.duration, current.state.clone(), current.revision)?;
    let transition = machine.admit(operation_id.clone(), run_id.clone())?;
    persist_grant_transition(
        &tx,
        &current,
        &transition.new_state,
        transition.new_revision,
        &operation_id,
        transition.admitted_run_id.as_ref(),
        None,
    )?;
    insert_operation(&tx, &operation_id, "grant", grant_id, &fingerprint)?;
    tx.execute(
        "INSERT INTO permission_grant_admissions
         (grant_id, operation_id, run_id, admitted_at) VALUES (?1, ?2, ?3, ?4)",
        params![
            grant_id,
            operation_id.as_str(),
            run_id.as_str(),
            timestamp()
        ],
    )
    .map_err(repository_error)?;
    let result = grant_on_connection(&tx, grant_id)?.ok_or_else(|| PermissionError::NotFound {
        id: grant_id.to_owned(),
    })?;
    tx.commit().map_err(repository_error)?;
    Ok(result)
}

fn revoke_grant(
    store: &AglStore,
    grant_id: &str,
    operation_id: PermissionOperationId,
    revoke_ref: Option<&str>,
) -> Result<PermissionGrantRecord, PermissionError> {
    let fingerprint = format!("revoke_grant\0{grant_id}\0{}", revoke_ref.unwrap_or(""));
    apply_grant_terminal(
        store,
        grant_id,
        operation_id,
        &fingerprint,
        revoke_ref,
        |machine, operation_id| machine.revoke(operation_id),
    )
}

fn expire_session_grants(
    store: &AglStore,
    session_id: &SessionId,
) -> Result<usize, PermissionError> {
    let mut statement = store
        .connection()
        .prepare(
            "SELECT id FROM permission_grants
             WHERE state = 'active' AND duration = 'session'
               AND json_extract(scope_json, '$.session_id') = ?1
             ORDER BY id",
        )
        .map_err(repository_error)?;
    let rows = statement
        .query_map(params![session_id.as_str()], |row| row.get::<_, String>(0))
        .map_err(repository_error)?;
    let ids = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(repository_error)?;
    drop(statement);
    for grant_id in &ids {
        let operation_id = PermissionOperationId::new(format!(
            "expire-session:{}:{}",
            session_id.as_str(),
            grant_id
        ))?;
        let fingerprint = format!("expire_session_grant\0{grant_id}\0{session_id}");
        apply_grant_terminal(
            store,
            grant_id,
            operation_id,
            &fingerprint,
            None,
            |machine, operation_id| machine.expire(operation_id),
        )?;
    }
    Ok(ids.len())
}

fn live_process_grant_ids(store: &AglStore) -> Result<BTreeSet<String>, PermissionError> {
    let mut statement = store
        .connection()
        .prepare(
            "SELECT id, request_id, tool_id, max_operation_kind, state_effects_json,
                    sensitive_inputs_json, scope_json, duration, granted_by_ref, state,
                    revision, created_at, updated_at, revoked_at, revoke_ref, admitted_at,
                    last_admitted_run_id, consumed_at, transition_operation_id
             FROM permission_grants WHERE state IN ('active', 'consumed') ORDER BY id",
        )
        .map_err(repository_error)?;
    let rows = statement
        .query_map([], grant_from_row)
        .map_err(repository_error)?;
    let grants = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(repository_error)?;
    let mut live = BTreeSet::new();
    for grant in grants {
        let run = match &grant.state {
            PermissionGrantState::Consumed { run_id } => store
                .run(run_id)
                .map_err(repository_error)?
                .map(|record| (record.run_id, record.state)),
            _ => None,
        };
        if permission_grant_is_live_for_run(
            &grant.state,
            run.as_ref().map(|(run_id, state)| (run_id, *state)),
        ) {
            live.insert(grant.id);
        }
    }
    Ok(live)
}

fn apply_grant_terminal<F>(
    store: &AglStore,
    grant_id: &str,
    operation_id: PermissionOperationId,
    fingerprint: &str,
    revoke_ref: Option<&str>,
    apply: F,
) -> Result<PermissionGrantRecord, PermissionError>
where
    F: FnOnce(
        &mut PermissionGrantMachine,
        PermissionOperationId,
    ) -> Result<agl_permission::PermissionGrantTransition, PermissionMachineError>,
{
    let tx = store
        .connection()
        .unchecked_transaction()
        .map_err(repository_error)?;
    if operation_replayed(&tx, &operation_id, "grant", grant_id, fingerprint)? {
        return grant_on_connection(&tx, grant_id)?.ok_or_else(|| PermissionError::NotFound {
            id: grant_id.to_owned(),
        });
    }
    let current = grant_on_connection(&tx, grant_id)?.ok_or_else(|| PermissionError::NotFound {
        id: grant_id.to_owned(),
    })?;
    let mut machine =
        PermissionGrantMachine::restore(current.duration, current.state.clone(), current.revision)?;
    let transition = apply(&mut machine, operation_id.clone())?;
    persist_grant_transition(
        &tx,
        &current,
        &transition.new_state,
        transition.new_revision,
        &operation_id,
        transition.admitted_run_id.as_ref(),
        revoke_ref,
    )?;
    insert_operation(&tx, &operation_id, "grant", grant_id, fingerprint)?;
    let result = grant_on_connection(&tx, grant_id)?.ok_or_else(|| PermissionError::NotFound {
        id: grant_id.to_owned(),
    })?;
    tx.commit().map_err(repository_error)?;
    Ok(result)
}

fn persist_request_transition(
    conn: &Connection,
    current: &PermissionRequestRecord,
    state: PermissionRequestState,
    revision: PermissionRevision,
    operation_id: &PermissionOperationId,
    resolution_ref: Option<&str>,
    note: Option<&str>,
) -> Result<(), PermissionError> {
    let changed = conn
        .execute(
            "UPDATE permission_requests
             SET state = ?2, revision = ?3, updated_at = ?4, resolved_at = ?4,
                 resolution_ref = ?5, resolution_note = ?6, transition_operation_id = ?7
             WHERE id = ?1 AND state = ?8 AND revision = ?9",
            params![
                current.id,
                state.as_str(),
                revision.get(),
                timestamp(),
                resolution_ref,
                note,
                operation_id.as_str(),
                current.state.as_str(),
                current.revision.get(),
            ],
        )
        .map_err(repository_error)?;
    if changed != 1 {
        return Err(PermissionError::RevisionConflict {
            id: current.id.clone(),
        });
    }
    Ok(())
}

fn persist_grant_transition(
    conn: &Connection,
    current: &PermissionGrantRecord,
    state: &PermissionGrantState,
    revision: PermissionRevision,
    operation_id: &PermissionOperationId,
    admitted_run_id: Option<&RunId>,
    revoke_ref: Option<&str>,
) -> Result<(), PermissionError> {
    let now = timestamp();
    let admitted_run_id = match (admitted_run_id, state) {
        (Some(run_id), _) => Some(run_id.as_str()),
        (None, PermissionGrantState::Consumed { run_id }) => Some(run_id.as_str()),
        (None, PermissionGrantState::Active) => {
            current.last_admitted_run_id.as_ref().map(RunId::as_str)
        }
        (None, PermissionGrantState::Expired | PermissionGrantState::Revoked) => {
            current.last_admitted_run_id.as_ref().map(RunId::as_str)
        }
    };
    let changed = conn
        .execute(
            "UPDATE permission_grants
             SET state = ?2, revision = ?3, updated_at = ?4,
                 revoked_at = CASE WHEN ?2 = 'revoked' THEN ?4 ELSE revoked_at END,
                 revoke_ref = CASE WHEN ?2 = 'revoked' THEN ?5 ELSE revoke_ref END,
                 admitted_at = CASE WHEN ?6 IS NOT NULL THEN COALESCE(admitted_at, ?4) ELSE admitted_at END,
                 last_admitted_run_id = ?6,
                 consumed_at = CASE WHEN ?2 IN ('consumed', 'expired') THEN ?4 ELSE consumed_at END,
                 transition_operation_id = ?7
             WHERE id = ?1 AND state = ?8 AND revision = ?9",
            params![
                current.id,
                state.as_str(),
                revision.get(),
                now,
                revoke_ref,
                admitted_run_id,
                operation_id.as_str(),
                current.state.as_str(),
                current.revision.get(),
            ],
        )
        .map_err(repository_error)?;
    if changed != 1 {
        return Err(PermissionError::RevisionConflict {
            id: current.id.clone(),
        });
    }
    Ok(())
}

fn insert_grant_on_connection(
    conn: &Connection,
    draft: PermissionGrantDraft,
) -> Result<PermissionGrantRecord, PermissionError> {
    validate_grant_draft(&draft)?;
    let id = store_id("permission_grant");
    let now = timestamp();
    conn.execute(
        "INSERT INTO permission_grants
         (id, request_id, tool_id, max_operation_kind, state_effects_json,
          sensitive_inputs_json, scope_json, duration, granted_by_ref, state, revision,
          created_at, updated_at, revoked_at, revoke_ref, admitted_at,
          last_admitted_run_id, consumed_at, transition_operation_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'active', 1,
                 ?10, ?10, NULL, NULL, NULL, NULL, NULL, NULL)",
        params![
            id,
            draft.request_id,
            draft.tool_id.as_str(),
            draft.max_operation_kind.as_str(),
            json(&draft.state_effects)?,
            json(&draft.sensitive_inputs)?,
            json(&draft.scope)?,
            draft.duration.as_str(),
            draft.granted_by_ref,
            now,
        ],
    )
    .map_err(repository_error)?;
    grant_on_connection(conn, &id)?.ok_or(PermissionError::NotFound { id })
}

fn request_on_connection(
    conn: &Connection,
    id: &str,
) -> Result<Option<PermissionRequestRecord>, PermissionError> {
    validate_bounded("request_id", id)?;
    conn.query_row(
        "SELECT id, requested_tools_json, max_operation_kind, state_effects_json,
                sensitive_inputs_json, scope_json, duration, reason, requester_ref,
                state, revision, created_at, updated_at, resolved_at, resolution_ref,
                resolution_note, transition_operation_id
         FROM permission_requests WHERE id = ?1",
        params![id],
        request_from_row,
    )
    .optional()
    .map_err(repository_error)
}

fn grant_on_connection(
    conn: &Connection,
    id: &str,
) -> Result<Option<PermissionGrantRecord>, PermissionError> {
    validate_bounded("grant_id", id)?;
    conn.query_row(
        "SELECT id, request_id, tool_id, max_operation_kind, state_effects_json,
                sensitive_inputs_json, scope_json, duration, granted_by_ref, state,
                revision, created_at, updated_at, revoked_at, revoke_ref, admitted_at,
                last_admitted_run_id, consumed_at, transition_operation_id
         FROM permission_grants WHERE id = ?1",
        params![id],
        grant_from_row,
    )
    .optional()
    .map_err(repository_error)
}

fn grants_for_request(
    conn: &Connection,
    request_id: &str,
) -> Result<Vec<PermissionGrantRecord>, PermissionError> {
    let mut statement = conn
        .prepare(
            "SELECT id, request_id, tool_id, max_operation_kind, state_effects_json,
                    sensitive_inputs_json, scope_json, duration, granted_by_ref, state,
                    revision, created_at, updated_at, revoked_at, revoke_ref, admitted_at,
                    last_admitted_run_id, consumed_at, transition_operation_id
             FROM permission_grants WHERE request_id = ?1 ORDER BY id",
        )
        .map_err(repository_error)?;
    let rows = statement
        .query_map(params![request_id], grant_from_row)
        .map_err(repository_error)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(repository_error)
}

fn operation_replayed(
    conn: &Connection,
    operation_id: &PermissionOperationId,
    target_kind: &str,
    target_id: &str,
    fingerprint: &str,
) -> Result<bool, PermissionError> {
    let existing = conn
        .query_row(
            "SELECT target_kind, target_id, fingerprint FROM permission_operations
             WHERE operation_id = ?1",
            params![operation_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(repository_error)?;
    let Some((existing_kind, existing_id, existing_fingerprint)) = existing else {
        return Ok(false);
    };
    if existing_kind == target_kind
        && existing_id == target_id
        && existing_fingerprint == fingerprint
    {
        Ok(true)
    } else {
        Err(PermissionError::IdempotencyConflict {
            operation_id: operation_id.to_string(),
        })
    }
}

fn insert_operation(
    conn: &Connection,
    operation_id: &PermissionOperationId,
    target_kind: &str,
    target_id: &str,
    fingerprint: &str,
) -> Result<(), PermissionError> {
    conn.execute(
        "INSERT INTO permission_operations
         (operation_id, target_kind, target_id, fingerprint, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            operation_id.as_str(),
            target_kind,
            target_id,
            fingerprint,
            timestamp(),
        ],
    )
    .map_err(repository_error)?;
    Ok(())
}

fn request_from_row(row: &Row<'_>) -> rusqlite::Result<PermissionRequestRecord> {
    Ok(PermissionRequestRecord {
        id: row.get(0)?,
        requested_tools: permission_json(&row.get::<_, String>(1)?, 1)?,
        max_operation_kind: permission_value(&row.get::<_, String>(2)?, 2)?,
        state_effects: permission_json(&row.get::<_, String>(3)?, 3)?,
        sensitive_inputs: permission_json(&row.get::<_, String>(4)?, 4)?,
        scope: permission_json(&row.get::<_, String>(5)?, 5)?,
        duration: permission_machine(PermissionDuration::parse(&row.get::<_, String>(6)?), 6)?,
        reason: row.get(7)?,
        requester_ref: row.get(8)?,
        state: permission_machine(PermissionRequestState::parse(&row.get::<_, String>(9)?), 9)?,
        revision: permission_machine(
            PermissionRevision::new(unsigned(row.get::<_, i64>(10)?, 10)?),
            10,
        )?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        resolved_at: row.get(13)?,
        resolution_ref: row.get(14)?,
        resolution_note: row.get(15)?,
        transition_operation_id: row
            .get::<_, Option<String>>(16)?
            .map(PermissionOperationId::new)
            .transpose()
            .map_err(|error| conversion_error(16, error))?,
    })
}

fn grant_from_row(row: &Row<'_>) -> rusqlite::Result<PermissionGrantRecord> {
    let duration = permission_machine(PermissionDuration::parse(&row.get::<_, String>(7)?), 7)?;
    let state_name = row.get::<_, String>(9)?;
    let run_id = row
        .get::<_, Option<String>>(16)?
        .map(|value| RunId::parse(&value))
        .transpose()
        .map_err(|error| conversion_error(16, error))?;
    let state = match state_name.as_str() {
        "active" => PermissionGrantState::Active,
        "consumed" => PermissionGrantState::Consumed {
            run_id: run_id
                .clone()
                .ok_or_else(|| invalid_row(16, "consumed grant lacks Run identity"))?,
        },
        "expired" => PermissionGrantState::Expired,
        "revoked" => PermissionGrantState::Revoked,
        _ => return Err(invalid_row(9, "unknown permission grant state")),
    };
    permission_machine(
        PermissionGrantMachine::restore(
            duration,
            state.clone(),
            PermissionRevision::new(unsigned(row.get::<_, i64>(10)?, 10)?)
                .map_err(|error| conversion_error(10, error))?,
        ),
        9,
    )?;
    Ok(PermissionGrantRecord {
        id: row.get(0)?,
        request_id: row.get(1)?,
        tool_id: permission_value(&row.get::<_, String>(2)?, 2)?,
        max_operation_kind: permission_value(&row.get::<_, String>(3)?, 3)?,
        state_effects: permission_json(&row.get::<_, String>(4)?, 4)?,
        sensitive_inputs: permission_json(&row.get::<_, String>(5)?, 5)?,
        scope: permission_json(&row.get::<_, String>(6)?, 6)?,
        duration,
        granted_by_ref: row.get(8)?,
        state,
        revision: PermissionRevision::new(unsigned(row.get::<_, i64>(10)?, 10)?)
            .map_err(|error| conversion_error(10, error))?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        revoked_at: row.get(13)?,
        revoke_ref: row.get(14)?,
        admitted_at: row.get(15)?,
        last_admitted_run_id: run_id,
        consumed_at: row.get(17)?,
        transition_operation_id: row
            .get::<_, Option<String>>(18)?
            .map(PermissionOperationId::new)
            .transpose()
            .map_err(|error| conversion_error(18, error))?,
    })
}

fn validate_request_draft(draft: &PermissionRequestDraft) -> Result<(), PermissionError> {
    if draft.requested_tools.is_empty() {
        return Err(PermissionError::Machine(
            PermissionMachineError::InvalidValue {
                field: "requested_tools",
                reason: "at least one Tool is required".to_owned(),
            },
        ));
    }
    validate_bounded("reason", &draft.reason)?;
    validate_bounded("requester_ref", &draft.requester_ref)
}

fn validate_grant_draft(draft: &PermissionGrantDraft) -> Result<(), PermissionError> {
    if let Some(request_id) = &draft.request_id {
        validate_bounded("request_id", request_id)?;
    }
    validate_bounded("granted_by_ref", &draft.granted_by_ref)
}

fn validate_bounded(field: &'static str, value: &str) -> Result<(), PermissionError> {
    if value.is_empty()
        || value.len() > 4_096
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(PermissionError::Machine(
            PermissionMachineError::InvalidValue {
                field,
                reason: "must be nonempty bounded text without control or surrounding whitespace"
                    .to_owned(),
            },
        ));
    }
    Ok(())
}

fn json<T: serde::Serialize>(value: &T) -> Result<String, PermissionError> {
    serde_json::to_string(value).map_err(repository_error)
}

fn permission_json<T: serde::de::DeserializeOwned>(
    value: &str,
    column: usize,
) -> rusqlite::Result<T> {
    serde_json::from_str(value).map_err(|error| conversion_error(column, error))
}

fn permission_value<T: serde::de::DeserializeOwned>(
    value: &str,
    column: usize,
) -> rusqlite::Result<T> {
    serde_json::from_value(serde_json::Value::String(value.to_owned()))
        .map_err(|error| conversion_error(column, error))
}

fn permission_machine<T>(
    result: Result<T, impl std::error::Error + Send + Sync + 'static>,
    column: usize,
) -> rusqlite::Result<T> {
    result.map_err(|error| conversion_error(column, error))
}

fn unsigned(value: i64, column: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| invalid_row(column, "negative permission revision"))
}

fn conversion_error(
    column: usize,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(error))
}

fn invalid_row(column: usize, reason: &str) -> rusqlite::Error {
    conversion_error(
        column,
        std::io::Error::new(std::io::ErrorKind::InvalidData, reason.to_owned()),
    )
}

fn repository_error(error: impl std::fmt::Display) -> PermissionError {
    PermissionError::Repository {
        reason: error.to_string(),
    }
}
