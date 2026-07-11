use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

use crate::path::{database_path, ensure_private_dir, set_private_file_permissions};
use crate::{AglStore, DEFAULT_DATABASE_FILE, Result};

impl AglStore {
    pub fn open_at(root: impl AsRef<Path>) -> Result<Self> {
        let store = Self::open_for_migration_at(root)?;
        store.migrate()?;
        secure_database_files(&store.database_path)?;
        Ok(store)
    }

    pub fn open_current_read_only_at(root: impl AsRef<Path>) -> Result<Self> {
        let status = Self::current_schema_status_at(root)?;
        let conn =
            Connection::open_with_flags(&status.database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        configure_read_only(&conn)?;
        Ok(Self {
            conn,
            database_path: status.database_path,
        })
    }

    pub fn open_current_at(root: impl AsRef<Path>) -> Result<Self> {
        let status = Self::current_schema_status_at(root)?;
        let conn = Connection::open(&status.database_path)?;
        configure_writable(&conn)?;
        secure_database_files(&status.database_path)?;
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
        configure_writable(&conn)?;
        secure_database_files(&database_path)?;
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
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        match f(&tx) {
            Ok(value) => {
                tx.commit()?;
                secure_database_files(&self.database_path)?;
                Ok(value)
            }
            Err(err) => {
                let _ = tx.rollback();
                Err(err)
            }
        }
    }
}

const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

fn configure_writable(conn: &Connection) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         PRAGMA synchronous = FULL;",
    )?;
    Ok(())
}

pub(crate) fn configure_read_only(conn: &Connection) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    Ok(())
}

pub(crate) fn secure_database_files(database_path: &Path) -> Result<()> {
    set_private_file_permissions(database_path)?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = database_path.as_os_str().to_os_string();
        sidecar.push(suffix);
        let sidecar = std::path::PathBuf::from(sidecar);
        if sidecar.exists() {
            set_private_file_permissions(&sidecar)?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn connection_pragmas(conn: &Connection) -> Result<(String, bool, i64, i64)> {
    let journal_mode = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    let foreign_keys = conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))? != 0;
    let synchronous = conn.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
    let busy_timeout = conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
    Ok((journal_mode, foreign_keys, synchronous, busy_timeout))
}
