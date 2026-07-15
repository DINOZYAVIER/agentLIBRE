use crate::{
    AglStore, Result, StoreDomain, StoreDomainHealth, StoreDomainStatus, StoreHealth,
    StoreIdempotencyHealth, StoreStaleIdempotencyRecord, StoreStatus,
};

impl AglStore {
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
}
