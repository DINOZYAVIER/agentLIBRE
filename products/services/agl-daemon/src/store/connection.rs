use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

use crate::store::path::{
    database_path, ensure_private_dir, set_private_file_permissions, validate_private_regular_file,
};
use crate::store::{
    AglStore, DEFAULT_DATABASE_FILE, Result, STORE_APPLICATION_ID, STORE_BASELINE_VERSION,
    StoreError,
};

impl AglStore {
    pub(crate) fn open_at(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        ensure_private_dir(root)?;
        let path = database_path(root, DEFAULT_DATABASE_FILE)?;
        validate_database_files(&path)?;
        if path.try_exists()? {
            validate_private_regular_file(&path)?;
        }
        if path.try_exists()? {
            require_current_database(&path)?;
        }
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        validate_private_regular_file(&path)?;
        configure_writer(&conn)?;
        initialize_baseline(&conn)?;
        secure_database_files(&path)?;
        Ok(Self {
            conn,
            database_path: path,
        })
    }

    #[cfg(test)]
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }

    pub(crate) fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub(crate) fn transaction<T>(
        &self,
        operation: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        match operation(&transaction) {
            Ok(value) => {
                transaction.commit()?;
                Ok(value)
            }
            Err(error) => {
                let _ = transaction.rollback();
                Err(error)
            }
        }
    }
}

pub(crate) fn open_reader(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA query_only=ON; PRAGMA foreign_keys=ON;")?;
    Ok(connection)
}

pub(super) fn require_current_database(path: &Path) -> Result<()> {
    validate_private_regular_file(path)?;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    let application_id =
        connection.query_row("PRAGMA application_id", [], |row| row.get::<_, u32>(0))?;
    let version = connection.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))?;
    if application_id != STORE_APPLICATION_ID || version != STORE_BASELINE_VERSION {
        return Err(StoreError::IncompatibleDatabase {
            path: path.to_path_buf(),
            application_id,
            version,
            required_application_id: STORE_APPLICATION_ID,
            required_version: STORE_BASELINE_VERSION,
        });
    }
    Ok(())
}

fn managed_database_files(path: &Path) -> [PathBuf; 3] {
    let value = path.as_os_str().to_string_lossy();
    [
        path.to_path_buf(),
        PathBuf::from(format!("{value}-wal")),
        PathBuf::from(format!("{value}-shm")),
    ]
}

pub(super) fn validate_database_files(path: &Path) -> Result<()> {
    for candidate in managed_database_files(path) {
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => validate_private_regular_file(&candidate)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(super) fn configure_writer(connection: &Connection) -> Result<()> {
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=FULL;
         PRAGMA foreign_keys=ON;
         PRAGMA trusted_schema=OFF;",
    )?;
    Ok(())
}

pub(super) fn initialize_baseline(connection: &Connection) -> Result<()> {
    let transaction = connection.unchecked_transaction()?;
    crate::store::agent::create_agent_schema_connection(&transaction)?;
    crate::store::inference_health::create_schema(&transaction)?;
    transaction.pragma_update(None, "application_id", STORE_APPLICATION_ID)?;
    transaction.pragma_update(None, "user_version", STORE_BASELINE_VERSION)?;
    transaction.commit()?;
    Ok(())
}

fn secure_database_files(path: &Path) -> Result<()> {
    for candidate in managed_database_files(path) {
        if candidate.try_exists()? {
            validate_private_regular_file(&candidate)?;
            set_private_file_permissions(&candidate)?;
        }
    }
    Ok(())
}
