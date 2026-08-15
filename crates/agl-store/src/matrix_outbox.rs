use agl_matrix::{
    MatrixDeliveryResult, MatrixError, MatrixMachineError, MatrixOperationId, MatrixOutboxDraft,
    MatrixOutboxId, MatrixOutboxMachine, MatrixOutboxRecord, MatrixOutboxRepository,
    MatrixOutboxState, MatrixRevision,
};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::util::{store_id, timestamp};
use crate::{AglStore, StoreHandle};

impl MatrixOutboxRepository for StoreHandle {
    fn enqueue(&self, draft: MatrixOutboxDraft) -> Result<MatrixOutboxRecord, MatrixError> {
        enqueue(&*self.lock().map_err(repository_error)?, draft)
    }

    fn get(&self, id: &MatrixOutboxId) -> Result<Option<MatrixOutboxRecord>, MatrixError> {
        get(&*self.lock().map_err(repository_error)?, id)
    }

    fn queued_page(&self, limit: usize) -> Result<Vec<MatrixOutboxRecord>, MatrixError> {
        queued_page(&*self.lock().map_err(repository_error)?, limit)
    }

    fn queued(&self, now_ms: u64, limit: usize) -> Result<Vec<MatrixOutboxRecord>, MatrixError> {
        queued(&*self.lock().map_err(repository_error)?, now_ms, limit)
    }

    fn claim(
        &self,
        id: &MatrixOutboxId,
        operation_id: MatrixOperationId,
        lease_owner: &str,
        now_ms: u64,
        lease_expires_at_ms: u64,
    ) -> Result<MatrixOutboxRecord, MatrixError> {
        claim(
            &*self.lock().map_err(repository_error)?,
            id,
            operation_id,
            lease_owner,
            now_ms,
            lease_expires_at_ms,
        )
    }

    fn complete(
        &self,
        id: &MatrixOutboxId,
        operation_id: MatrixOperationId,
        lease_owner: &str,
        result: MatrixDeliveryResult,
    ) -> Result<MatrixOutboxRecord, MatrixError> {
        complete(
            &*self.lock().map_err(repository_error)?,
            id,
            operation_id,
            lease_owner,
            result,
        )
    }

    fn recover_expired(
        &self,
        now_ms: u64,
        limit: usize,
    ) -> Result<Vec<MatrixOutboxRecord>, MatrixError> {
        recover_expired(&*self.lock().map_err(repository_error)?, now_ms, limit)
    }
}

fn enqueue(store: &AglStore, draft: MatrixOutboxDraft) -> Result<MatrixOutboxRecord, MatrixError> {
    if let Some(existing) = by_dedupe_key(store.connection(), &draft.dedupe_key)? {
        return MatrixOutboxMachine::restore(existing)?
            .exact_replay(&draft)
            .map_err(MatrixError::from);
    }

    let id = MatrixOutboxId::new(store_id("matrix_outbox"))?;
    let mut record = MatrixOutboxMachine::enqueue(id, draft, 0).record().clone();
    let now = timestamp();
    record.created_at = now.clone();
    record.updated_at = now;
    let MatrixOutboxState::Queued { not_before_ms } = &record.state else {
        unreachable!("new Matrix outbox record is queued")
    };
    let inserted = store
        .connection()
        .execute(
            "INSERT INTO matrix_notification_outbox
             (id, notify_ref, source_kind, source_id, dedupe_key, body, payload_fingerprint,
              transaction_id, state, revision, not_before_ms, lease_owner,
              lease_expires_at_ms, attempts, last_error, created_at, updated_at, delivered_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'queued', ?9, ?10,
                     NULL, NULL, 0, NULL, ?11, ?11, NULL)
             ON CONFLICT(dedupe_key) DO NOTHING",
            params![
                record.id.as_str(),
                record.draft.notify_ref,
                record.draft.source_kind,
                record.draft.source_id,
                record.draft.dedupe_key,
                record.draft.body,
                record.payload_fingerprint,
                record.transaction_id,
                u64_to_i64(record.revision.get(), "revision")?,
                u64_to_i64(*not_before_ms, "not_before_ms")?,
                record.created_at,
            ],
        )
        .map_err(repository_error)?;
    if inserted == 1 {
        return Ok(record);
    }
    let existing =
        by_dedupe_key(store.connection(), &record.draft.dedupe_key)?.ok_or_else(|| {
            MatrixError::Repository {
                reason: "Matrix dedupe conflict did not expose an existing row".to_owned(),
            }
        })?;
    MatrixOutboxMachine::restore(existing)?
        .exact_replay(&record.draft)
        .map_err(MatrixError::from)
}

fn get(store: &AglStore, id: &MatrixOutboxId) -> Result<Option<MatrixOutboxRecord>, MatrixError> {
    get_on_connection(store.connection(), id)
}

fn queued_page(store: &AglStore, limit: usize) -> Result<Vec<MatrixOutboxRecord>, MatrixError> {
    let mut statement = store
        .connection()
        .prepare(
            "SELECT id, notify_ref, source_kind, source_id, dedupe_key, body,
                    payload_fingerprint, transaction_id, state, revision, not_before_ms,
                    lease_owner, lease_expires_at_ms, attempts, last_error, created_at,
                    updated_at, delivered_at
             FROM matrix_notification_outbox
             WHERE state = 'queued'
             ORDER BY not_before_ms, created_at, id LIMIT ?1",
        )
        .map_err(repository_error)?;
    let rows = statement
        .query_map(
            params![i64::try_from(limit.max(1)).unwrap_or(i64::MAX)],
            record_from_row,
        )
        .map_err(repository_error)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(repository_error)
}

fn queued(
    store: &AglStore,
    now_ms: u64,
    limit: usize,
) -> Result<Vec<MatrixOutboxRecord>, MatrixError> {
    let mut statement = store
        .connection()
        .prepare(
            "SELECT id, notify_ref, source_kind, source_id, dedupe_key, body,
                    payload_fingerprint, transaction_id, state, revision, not_before_ms,
                    lease_owner, lease_expires_at_ms, attempts, last_error, created_at,
                    updated_at, delivered_at
             FROM matrix_notification_outbox
             WHERE state = 'queued' AND not_before_ms <= ?1
             ORDER BY not_before_ms, created_at, id LIMIT ?2",
        )
        .map_err(repository_error)?;
    let rows = statement
        .query_map(
            params![
                u64_to_i64(now_ms, "now_ms")?,
                i64::try_from(limit.max(1)).unwrap_or(i64::MAX),
            ],
            record_from_row,
        )
        .map_err(repository_error)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(repository_error)
}

fn claim(
    store: &AglStore,
    id: &MatrixOutboxId,
    operation_id: MatrixOperationId,
    lease_owner: &str,
    now_ms: u64,
    lease_expires_at_ms: u64,
) -> Result<MatrixOutboxRecord, MatrixError> {
    let fingerprint = format!("claim\0{lease_owner}\0{now_ms}\0{lease_expires_at_ms}");
    apply_transition(
        store,
        id,
        operation_id,
        &fingerprint,
        |machine, operation_id| {
            machine.claim(
                operation_id,
                lease_owner.to_owned(),
                now_ms,
                lease_expires_at_ms,
            )
        },
    )
}

fn complete(
    store: &AglStore,
    id: &MatrixOutboxId,
    operation_id: MatrixOperationId,
    lease_owner: &str,
    result: MatrixDeliveryResult,
) -> Result<MatrixOutboxRecord, MatrixError> {
    let fingerprint = format!("complete\0{lease_owner}\0{result:?}");
    apply_transition(
        store,
        id,
        operation_id,
        &fingerprint,
        |machine, operation_id| machine.complete(operation_id, lease_owner.to_owned(), result),
    )
}

fn recover_expired(
    store: &AglStore,
    now_ms: u64,
    limit: usize,
) -> Result<Vec<MatrixOutboxRecord>, MatrixError> {
    let mut statement = store
        .connection()
        .prepare(
            "SELECT id FROM matrix_notification_outbox
             WHERE state = 'delivering' AND lease_expires_at_ms <= ?1
             ORDER BY lease_expires_at_ms, id LIMIT ?2",
        )
        .map_err(repository_error)?;
    let rows = statement
        .query_map(
            params![
                u64_to_i64(now_ms, "now_ms")?,
                i64::try_from(limit.max(1)).unwrap_or(i64::MAX),
            ],
            |row| row.get::<_, String>(0),
        )
        .map_err(repository_error)?;
    let ids = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(repository_error)?;
    drop(statement);

    let mut recovered = Vec::with_capacity(ids.len());
    for raw_id in ids {
        let id = MatrixOutboxId::new(raw_id)?;
        let record =
            get(store, &id)?.ok_or_else(|| MatrixError::NotFound { id: id.to_string() })?;
        let operation_id = MatrixOperationId::new(format!(
            "recover:{}:{}:{}",
            id,
            record.revision.get(),
            now_ms
        ))?;
        let fingerprint = format!("recover\0{now_ms}");
        recovered.push(apply_transition(
            store,
            &id,
            operation_id,
            &fingerprint,
            |machine, operation_id| machine.recover_expired(operation_id, now_ms),
        )?);
    }
    Ok(recovered)
}

fn apply_transition<F>(
    store: &AglStore,
    id: &MatrixOutboxId,
    operation_id: MatrixOperationId,
    fingerprint: &str,
    apply: F,
) -> Result<MatrixOutboxRecord, MatrixError>
where
    F: FnOnce(
        &mut MatrixOutboxMachine,
        MatrixOperationId,
    ) -> Result<agl_matrix::MatrixOutboxTransition, MatrixMachineError>,
{
    let tx = store
        .connection()
        .unchecked_transaction()
        .map_err(repository_error)?;
    if let Some((existing_id, existing_fingerprint)) = tx
        .query_row(
            "SELECT outbox_id, fingerprint FROM matrix_outbox_operations
             WHERE operation_id = ?1",
            params![operation_id.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(repository_error)?
    {
        if existing_id != id.as_str() || existing_fingerprint != fingerprint {
            return Err(MatrixError::Machine(
                MatrixMachineError::IdempotencyConflict { operation_id },
            ));
        }
        return get_on_connection(&tx, id)?
            .ok_or_else(|| MatrixError::NotFound { id: id.to_string() });
    }

    let current =
        get_on_connection(&tx, id)?.ok_or_else(|| MatrixError::NotFound { id: id.to_string() })?;
    let expected_revision = current.revision;
    let mut machine = MatrixOutboxMachine::restore(current)?;
    apply(&mut machine, operation_id.clone())?;
    let mut next = machine.record().clone();
    next.updated_at = timestamp();
    if matches!(next.state, MatrixOutboxState::Sent) {
        next.delivered_at = Some(next.updated_at.clone());
    }
    persist_record(&tx, &next, expected_revision)?;
    tx.execute(
        "INSERT INTO matrix_outbox_operations
         (operation_id, outbox_id, fingerprint, resulting_revision, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            operation_id.as_str(),
            id.as_str(),
            fingerprint,
            u64_to_i64(next.revision.get(), "revision")?,
            next.updated_at,
        ],
    )
    .map_err(repository_error)?;
    tx.commit().map_err(repository_error)?;
    Ok(next)
}

fn persist_record(
    conn: &Connection,
    record: &MatrixOutboxRecord,
    expected_revision: MatrixRevision,
) -> Result<(), MatrixError> {
    let (not_before, lease_owner, lease_expires) = match &record.state {
        MatrixOutboxState::Queued { not_before_ms } => (
            Some(u64_to_i64(*not_before_ms, "not_before_ms")?),
            None,
            None,
        ),
        MatrixOutboxState::Delivering {
            lease_owner,
            lease_expires_at_ms,
            ..
        } => (
            None,
            Some(lease_owner.as_str()),
            Some(u64_to_i64(*lease_expires_at_ms, "lease_expires_at_ms")?),
        ),
        MatrixOutboxState::Sent | MatrixOutboxState::Failed { .. } => (None, None, None),
    };
    let changed = conn
        .execute(
            "UPDATE matrix_notification_outbox
             SET state = ?2, revision = ?3, not_before_ms = ?4, lease_owner = ?5,
                 lease_expires_at_ms = ?6, attempts = ?7, last_error = ?8,
                 updated_at = ?9, delivered_at = ?10
             WHERE id = ?1 AND revision = ?11",
            params![
                record.id.as_str(),
                record.state.as_str(),
                u64_to_i64(record.revision.get(), "revision")?,
                not_before,
                lease_owner,
                lease_expires,
                record.attempts,
                record.last_error,
                record.updated_at,
                record.delivered_at,
                u64_to_i64(expected_revision.get(), "revision")?,
            ],
        )
        .map_err(repository_error)?;
    if changed != 1 {
        return Err(MatrixError::RevisionConflict {
            id: record.id.to_string(),
        });
    }
    Ok(())
}

fn get_on_connection(
    conn: &Connection,
    id: &MatrixOutboxId,
) -> Result<Option<MatrixOutboxRecord>, MatrixError> {
    conn.query_row(
        "SELECT id, notify_ref, source_kind, source_id, dedupe_key, body,
                payload_fingerprint, transaction_id, state, revision, not_before_ms,
                lease_owner, lease_expires_at_ms, attempts, last_error, created_at,
                updated_at, delivered_at
         FROM matrix_notification_outbox WHERE id = ?1",
        params![id.as_str()],
        record_from_row,
    )
    .optional()
    .map_err(repository_error)
}

fn by_dedupe_key(
    conn: &Connection,
    dedupe_key: &str,
) -> Result<Option<MatrixOutboxRecord>, MatrixError> {
    conn.query_row(
        "SELECT id, notify_ref, source_kind, source_id, dedupe_key, body,
                payload_fingerprint, transaction_id, state, revision, not_before_ms,
                lease_owner, lease_expires_at_ms, attempts, last_error, created_at,
                updated_at, delivered_at
         FROM matrix_notification_outbox WHERE dedupe_key = ?1",
        params![dedupe_key],
        record_from_row,
    )
    .optional()
    .map_err(repository_error)
}

fn record_from_row(row: &Row<'_>) -> rusqlite::Result<MatrixOutboxRecord> {
    let id = matrix_row(MatrixOutboxId::new(row.get::<_, String>(0)?), 0)?;
    let draft = matrix_row(
        MatrixOutboxDraft::new(
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
        ),
        1,
    )?;
    let state_name = row.get::<_, String>(8)?;
    let not_before = optional_u64(row.get::<_, Option<i64>>(10)?, 10)?;
    let lease_owner = row.get::<_, Option<String>>(11)?;
    let lease_expires = optional_u64(row.get::<_, Option<i64>>(12)?, 12)?;
    let last_error = row.get::<_, Option<String>>(14)?;
    let state = match state_name.as_str() {
        "queued" => MatrixOutboxState::Queued {
            not_before_ms: not_before
                .ok_or_else(|| invalid_row(10, "queued item lacks deadline"))?,
        },
        "delivering" => MatrixOutboxState::Delivering {
            lease_owner: lease_owner.ok_or_else(|| invalid_row(11, "delivery lacks owner"))?,
            lease_expires_at_ms: lease_expires
                .ok_or_else(|| invalid_row(12, "delivery lacks lease deadline"))?,
            attempt: row.get(13)?,
        },
        "sent" => MatrixOutboxState::Sent,
        "failed" => MatrixOutboxState::Failed {
            error: last_error
                .clone()
                .ok_or_else(|| invalid_row(14, "failed item lacks error"))?,
        },
        _ => return Err(invalid_row(8, "unknown Matrix outbox state")),
    };
    let revision_raw = required_u64(row.get::<_, i64>(9)?, 9)?;
    let revision = matrix_row(MatrixRevision::new(revision_raw), 9)?;
    let record = MatrixOutboxRecord {
        id,
        draft,
        payload_fingerprint: row.get(6)?,
        transaction_id: row.get(7)?,
        state,
        revision,
        attempts: row.get(13)?,
        last_error,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
        delivered_at: row.get(17)?,
    };
    matrix_row(
        MatrixOutboxMachine::restore(record.clone()).map(|_| record),
        0,
    )
}

fn matrix_row<T>(
    result: Result<T, impl std::error::Error + Send + Sync + 'static>,
    column: usize,
) -> rusqlite::Result<T> {
    result.map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn u64_to_i64(value: u64, field: &'static str) -> Result<i64, MatrixError> {
    i64::try_from(value).map_err(|_| {
        MatrixError::Machine(MatrixMachineError::InvalidValue {
            field,
            reason: "value exceeds SQLite signed integer range".to_owned(),
        })
    })
}

fn required_u64(value: i64, column: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| invalid_row(column, "negative unsigned Matrix value"))
}

fn optional_u64(value: Option<i64>, column: usize) -> rusqlite::Result<Option<u64>> {
    value.map(|value| required_u64(value, column)).transpose()
}

fn invalid_row(column: usize, reason: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            reason.to_owned(),
        )),
    )
}

fn repository_error(error: impl std::fmt::Display) -> MatrixError {
    MatrixError::Repository {
        reason: error.to_string(),
    }
}
