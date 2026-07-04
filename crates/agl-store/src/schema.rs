use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::path::default_database_path;
use crate::{
    AglStore, AppliedStoreMigration, CURRENT_SCHEMA_VERSION, Result, STORE_MIGRATIONS, StoreError,
    StoreMigration, StoreMigrationReport, StoreSchemaStatus,
};

impl AglStore {
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

    pub(crate) fn current_schema_status_at(root: impl AsRef<Path>) -> Result<StoreSchemaStatus> {
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

    pub(crate) fn migrate(&self) -> Result<StoreMigrationReport> {
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

    pub(crate) fn schema_version(&self) -> Result<u32> {
        let version = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))?;
        Ok(version)
    }
}

fn validate_migration_sequence(versions: &[u32]) -> Result<()> {
    for (expected, version) in (1_u32..).zip(versions.iter().copied()) {
        if version != expected {
            return Err(StoreError::MigrationGap { missing: expected });
        }
    }
    Ok(())
}
