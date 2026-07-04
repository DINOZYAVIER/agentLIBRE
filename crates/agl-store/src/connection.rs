use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::path::{database_path, ensure_private_dir, set_private_file_permissions};
use crate::{AglStore, DEFAULT_DATABASE_FILE, Result};

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

    pub(crate) fn open_for_migration_at(root: impl AsRef<Path>) -> Result<Self> {
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
