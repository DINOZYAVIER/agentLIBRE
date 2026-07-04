use std::path::PathBuf;

use rusqlite::Connection;

mod connection;
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
mod util;

pub use error::{Result, StoreError};
pub use migrations::{CURRENT_SCHEMA_VERSION, STORE_MIGRATIONS, StoreMigration};
#[cfg(test)]
use path::database_path;
pub use path::default_database_path;
pub use types::*;

pub const DEFAULT_DATABASE_FILE: &str = "agentlibre.sqlite3";

#[derive(Debug)]
pub struct AglStore {
    conn: Connection,
    database_path: PathBuf,
}

#[cfg(test)]
mod tests;
