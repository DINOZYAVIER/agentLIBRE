use std::path::PathBuf;

use rusqlite::Connection;

mod artifact_commits;
mod connection;
mod content_attachments;
mod cron;
mod error;
mod export;
mod handle;
mod idempotency;
mod matrix_outbox;
mod memory;
mod migrations;
mod note;
mod path;
mod permissions;
mod runs;
mod schema;
mod status;
mod types;
mod util;

pub use error::{Result, StoreError};
pub use handle::StoreHandle;
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
