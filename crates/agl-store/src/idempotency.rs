use rusqlite::{OptionalExtension, params};

use crate::{
    AglStore, IdempotencyOutcome, IdempotencyRecord, IdempotencyStatus, Result, StoreError,
    util::{timestamp, validate_non_blank},
};

impl AglStore {
    pub fn begin_idempotency(
        &self,
        namespace: &str,
        key: &str,
        fingerprint: &str,
    ) -> Result<IdempotencyOutcome> {
        validate_idempotency_part(namespace, "namespace")?;
        validate_idempotency_part(key, "key")?;
        validate_idempotency_part(fingerprint, "fingerprint")?;

        self.transaction(|tx| {
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
            tx.execute(
                "INSERT INTO idempotency_keys
                 (namespace, key, fingerprint, status, result_ref, created_at, updated_at, attempts)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?5, 1)",
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
        })
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
        self.transaction(|tx| {
            let now = timestamp();
            tx.execute(
                "UPDATE idempotency_keys
                 SET status = ?3, result_ref = ?4, lease_owner = NULL,
                     lease_expires_at_ms = NULL, updated_at = ?5
                 WHERE namespace = ?1 AND key = ?2",
                params![namespace, key, status.as_str(), result_ref, now],
            )?;
            self.idempotency_record(namespace, key)?
                .ok_or_else(|| StoreError::NotFound {
                    resource: format!("idempotency key {namespace}/{key}"),
                })
        })
    }

    pub fn idempotency_record(
        &self,
        namespace: &str,
        key: &str,
    ) -> Result<Option<IdempotencyRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT namespace, key, fingerprint, status, result_ref,
                    lease_owner, lease_expires_at_ms, admitted_run_id, attempts,
                    last_error_code, created_at, updated_at
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
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<i64>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, u32>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, String>(11)?,
            ))
        })
        .optional()?
        .map(
            |(
                namespace,
                key,
                fingerprint,
                status,
                result_ref,
                lease_owner,
                lease_expires_at_ms,
                admitted_run_id,
                attempts,
                last_error_code,
                created_at,
                updated_at,
            )| {
                Ok(IdempotencyRecord {
                    namespace,
                    key,
                    fingerprint,
                    status: IdempotencyStatus::parse(&status)?,
                    result_ref,
                    lease_owner,
                    lease_expires_at_ms,
                    admitted_run_id: admitted_run_id
                        .as_deref()
                        .map(agl_ids::RunId::parse)
                        .transpose()
                        .map_err(|_| StoreError::InvalidValue {
                            field: "idempotency_keys.admitted_run_id",
                            value: admitted_run_id.unwrap_or_default(),
                            reason: "invalid typed run ID",
                        })?,
                    attempts,
                    last_error_code,
                    created_at,
                    updated_at,
                })
            },
        )
        .transpose()
    }
}

fn validate_idempotency_part(value: &str, field: &'static str) -> Result<()> {
    validate_non_blank(value, field)
}
