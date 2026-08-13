use std::sync::Mutex;

use rusqlite::params;

use super::{Result, StoreError, TerminalStore};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistedAdmission {
    pub(crate) resource_kind: String,
    pub(crate) resource_id: String,
    pub(crate) request_fingerprint: String,
    pub(crate) authority_fingerprint: String,
    pub(crate) operations_json: String,
}

pub(crate) struct SqliteAdmissionRepository {
    store: Mutex<TerminalStore>,
}

impl SqliteAdmissionRepository {
    pub(crate) fn open_at(root: impl AsRef<std::path::Path>) -> Result<Self> {
        Ok(Self {
            store: Mutex::new(TerminalStore::open_at(root)?),
        })
    }

    pub(crate) fn list(&self) -> Result<Vec<PersistedAdmission>> {
        let store = self.store.lock().map_err(|_| StoreError::InvalidValue {
            field: "service_admissions",
            value: "poisoned".to_owned(),
            reason: "admission repository lock is poisoned",
        })?;
        let mut statement = store.connection().prepare(
            "SELECT resource_kind, resource_id, request_fingerprint,
                    authority_fingerprint, operations_json
             FROM service_admissions ORDER BY resource_kind, resource_id",
        )?;
        statement
            .query_map([], |row| {
                Ok(PersistedAdmission {
                    resource_kind: row.get(0)?,
                    resource_id: row.get(1)?,
                    request_fingerprint: row.get(2)?,
                    authority_fingerprint: row.get(3)?,
                    operations_json: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub(crate) fn record(&self, admission: &PersistedAdmission) -> Result<()> {
        let store = self.store.lock().map_err(|_| StoreError::InvalidValue {
            field: "service_admissions",
            value: "poisoned".to_owned(),
            reason: "admission repository lock is poisoned",
        })?;
        store.transaction(|tx| {
            tx.execute(
                "INSERT INTO service_admissions
                 (resource_kind, resource_id, request_fingerprint,
                  authority_fingerprint, operations_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(resource_kind, resource_id) DO UPDATE SET
                   request_fingerprint = excluded.request_fingerprint,
                   authority_fingerprint = excluded.authority_fingerprint,
                   operations_json = excluded.operations_json",
                params![
                    admission.resource_kind,
                    admission.resource_id,
                    admission.request_fingerprint,
                    admission.authority_fingerprint,
                    admission.operations_json,
                ],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admitted_authority_and_operations_survive_repository_restart() {
        let root = std::env::temp_dir().join(format!(
            "agl-terminal-admission-restart-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let expected = PersistedAdmission {
            resource_kind: "execution".to_owned(),
            resource_id: "exe_019c1234-1234-7123-8123-123456789abc".to_owned(),
            request_fingerprint: format!("sha256:{}", "a".repeat(64)),
            authority_fingerprint: format!("sha256:{}", "b".repeat(64)),
            operations_json: "[\"inspect\",\"terminate\"]".to_owned(),
        };
        SqliteAdmissionRepository::open_at(&root)
            .unwrap()
            .record(&expected)
            .unwrap();

        let reopened = SqliteAdmissionRepository::open_at(&root).unwrap();
        assert_eq!(reopened.list().unwrap(), vec![expected]);
        let _ = std::fs::remove_dir_all(root);
    }
}
