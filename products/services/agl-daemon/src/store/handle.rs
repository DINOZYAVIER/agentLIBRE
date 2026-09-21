use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use rusqlite::Connection;

use crate::store::connection::open_reader;
use crate::store::{AglStore, StoreError};

const MAX_READERS: usize = 4;

#[derive(Clone, Debug)]
pub struct StoreHandle {
    writer: Arc<Mutex<AglStore>>,
    readers: Arc<ReaderPool>,
    // Drop after every SQLite connection, including the reader pool.
    _ownership: Arc<super::ownership::StoreOwnership>,
}

impl StoreHandle {
    pub fn open_at(root: impl AsRef<Path>) -> crate::store::Result<Self> {
        let ownership = super::ownership::StoreOwnership::acquire(root.as_ref())?;
        let writer = AglStore::open_at(root)?;
        let path = writer.database_path().to_path_buf();
        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            readers: Arc::new(ReaderPool::new(path)),
            _ownership: Arc::new(ownership),
        })
    }

    pub(crate) fn lock(&self) -> crate::store::Result<MutexGuard<'_, AglStore>> {
        self.writer.lock().map_err(|_| StoreError::InvalidValue {
            field: "Store writer",
            value: "poisoned".to_owned(),
            reason: "a write panicked while holding the Store writer",
        })
    }

    pub(crate) fn read<T>(
        &self,
        operation: impl FnOnce(&Connection) -> crate::store::Result<T>,
    ) -> crate::store::Result<T> {
        let reader = self.readers.acquire()?;
        operation(&reader)
    }
}

#[derive(Debug)]
struct ReaderPool {
    path: std::path::PathBuf,
    state: Mutex<ReaderPoolState>,
    available: Condvar,
}

#[derive(Debug, Default)]
struct ReaderPoolState {
    open: usize,
    available: Vec<Connection>,
}

impl ReaderPool {
    fn new(path: std::path::PathBuf) -> Self {
        Self {
            path,
            state: Mutex::new(ReaderPoolState::default()),
            available: Condvar::new(),
        }
    }

    fn acquire(self: &Arc<Self>) -> crate::store::Result<ReaderLease> {
        let mut state = self.state.lock().map_err(|_| StoreError::InvalidValue {
            field: "Store reader pool",
            value: "poisoned".to_owned(),
            reason: "a read panicked while holding the reader pool",
        })?;
        loop {
            if let Some(connection) = state.available.pop() {
                return Ok(ReaderLease {
                    pool: self.clone(),
                    connection: Some(connection),
                });
            }
            if state.open < MAX_READERS {
                state.open += 1;
                drop(state);
                match open_reader(&self.path) {
                    Ok(connection) => {
                        return Ok(ReaderLease {
                            pool: self.clone(),
                            connection: Some(connection),
                        });
                    }
                    Err(error) => {
                        let mut state = self.state.lock().expect("reader pool lock poisoned");
                        state.open -= 1;
                        self.available.notify_one();
                        return Err(error);
                    }
                }
            }
            state = self
                .available
                .wait(state)
                .map_err(|_| StoreError::InvalidValue {
                    field: "Store reader pool",
                    value: "poisoned".to_owned(),
                    reason: "reader wait was interrupted by a panic",
                })?;
        }
    }
}

struct ReaderLease {
    pool: Arc<ReaderPool>,
    connection: Option<Connection>,
}

impl std::ops::Deref for ReaderLease {
    type Target = Connection;
    fn deref(&self) -> &Self::Target {
        self.connection
            .as_ref()
            .expect("reader lease owns a connection")
    }
}

impl Drop for ReaderLease {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take()
            && let Ok(mut state) = self.pool.state.lock()
        {
            state.available.push(connection);
            self.pool.available.notify_one();
        }
    }
}
