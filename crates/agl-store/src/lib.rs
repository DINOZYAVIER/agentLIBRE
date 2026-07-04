use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};

mod error;
mod export;
mod idempotency;
mod matrix_outbox;
mod migrations;
mod path;
mod permissions;
mod schema;
mod status;
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

    pub fn open_current_read_only_at(root: impl AsRef<Path>) -> Result<Self> {
        let status = Self::current_schema_status_at(root)?;
        let conn =
            Connection::open_with_flags(&status.database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self {
            conn,
            database_path: status.database_path,
        })
    }

    pub fn open_current_at(root: impl AsRef<Path>) -> Result<Self> {
        let status = Self::current_schema_status_at(root)?;
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

#[cfg(test)]
mod tests;
