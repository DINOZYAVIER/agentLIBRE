use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

mod error;
mod export;
mod idempotency;
mod matrix_outbox;
mod migrations;
mod path;
mod permissions;
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

    pub fn migrate_at(root: impl AsRef<Path>) -> Result<StoreMigrationReport> {
        let store = Self::open_for_migration_at(root)?;
        store.migrate()
    }

    pub fn schema_status_at(root: impl AsRef<Path>) -> Result<StoreSchemaStatus> {
        let database_path = default_database_path(root)?;
        if !database_path.exists() {
            return Ok(StoreSchemaStatus {
                database_path,
                database_exists: false,
                schema_version: None,
                current_schema_version: CURRENT_SCHEMA_VERSION,
                applied_migrations: Vec::new(),
                migration_required: true,
            });
        }
        let conn = Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let store = Self {
            conn,
            database_path: database_path.clone(),
        };
        let schema_version = store.schema_version()?;
        let applied_migrations = if store.schema_migrations_table_exists()? {
            store.applied_migration_versions()?
        } else {
            Vec::new()
        };
        let migration_required = schema_version != CURRENT_SCHEMA_VERSION
            || applied_migrations.len() != STORE_MIGRATIONS.len()
            || applied_migrations.last().copied() != Some(CURRENT_SCHEMA_VERSION);
        Ok(StoreSchemaStatus {
            database_path,
            database_exists: true,
            schema_version: Some(schema_version),
            current_schema_version: CURRENT_SCHEMA_VERSION,
            applied_migrations,
            migration_required,
        })
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

    fn current_schema_status_at(root: impl AsRef<Path>) -> Result<StoreSchemaStatus> {
        let status = Self::schema_status_at(root)?;
        if !status.database_exists {
            return Err(StoreError::InvalidValue {
                field: "store",
                value: status.database_path.display().to_string(),
                reason: "store database does not exist; run store.migrate first",
            });
        }
        if status.migration_required {
            return Err(StoreError::InvalidValue {
                field: "store",
                value: format!(
                    "schema_version={:?}, current_schema_version={}",
                    status.schema_version, status.current_schema_version
                ),
                reason: "store schema migration required; run store.migrate first",
            });
        }
        Ok(status)
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

    fn migrate(&self) -> Result<StoreMigrationReport> {
        let before_schema_version = self.schema_version()?;
        self.ensure_migration_table()?;
        let current_version = self.schema_version()?;
        if current_version > CURRENT_SCHEMA_VERSION {
            return Err(StoreError::UnsupportedSchemaVersion {
                found: current_version,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }
        let applied_versions = self.applied_migration_versions()?;
        for version in &applied_versions {
            if *version > CURRENT_SCHEMA_VERSION {
                return Err(StoreError::UnsupportedSchemaVersion {
                    found: *version,
                    supported: CURRENT_SCHEMA_VERSION,
                });
            }
        }
        validate_migration_sequence(&applied_versions)?;
        let mut applied_migrations = Vec::new();
        for migration in STORE_MIGRATIONS {
            if !self.migration_applied(migration.version)? {
                self.apply_migration(migration)?;
                applied_migrations.push(AppliedStoreMigration {
                    version: migration.version,
                    name: migration.name.to_string(),
                });
            }
        }
        Ok(StoreMigrationReport {
            database_path: self.database_path.clone(),
            before_schema_version,
            after_schema_version: self.schema_version()?,
            applied_migrations,
        })
    }

    fn ensure_migration_table(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                applied_at TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    fn applied_migration_versions(&self) -> Result<Vec<u32>> {
        let mut stmt = self
            .conn
            .prepare("SELECT version FROM schema_migrations ORDER BY version")?;
        let rows = stmt.query_map([], |row| row.get::<_, u32>(0))?;
        let mut versions = Vec::new();
        for row in rows {
            versions.push(row?);
        }
        Ok(versions)
    }

    fn schema_migrations_table_exists(&self) -> Result<bool> {
        let exists = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(exists)
    }

    fn migration_applied(&self, version: u32) -> Result<bool> {
        let applied = self
            .conn
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE version = ?1",
                params![version],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(applied)
    }

    fn apply_migration(&self, migration: &StoreMigration) -> Result<()> {
        let batch = format!(
            r#"
            BEGIN;
            {sql}
            INSERT INTO schema_migrations(version, applied_at)
            VALUES ({version}, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'));
            PRAGMA user_version = {version};
            COMMIT;
            "#,
            sql = migration.sql,
            version = migration.version
        );
        self.conn.execute_batch(&batch)?;
        Ok(())
    }

    fn schema_version(&self) -> Result<u32> {
        let version = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))?;
        Ok(version)
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

fn validate_migration_sequence(versions: &[u32]) -> Result<()> {
    for (expected, version) in (1_u32..).zip(versions.iter().copied()) {
        if version != expected {
            return Err(StoreError::MigrationGap { missing: expected });
        }
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
