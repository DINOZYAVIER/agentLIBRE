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
}

fn validate_idempotency_part(value: &str, field: &'static str) -> Result<()> {
    validate_non_blank(value, field)
}
