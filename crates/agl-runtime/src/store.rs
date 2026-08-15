use std::path::Path;
use std::sync::Arc;

use agl_artifact::ArtifactCommitRepository;
use agl_content::ContentRepository;
use agl_core_tools::{
    StoreAdminDomain, StoreAdminPort, StoreAppliedMigrationSnapshot, StoreDomainSnapshot,
    StoreExportSnapshot, StoreMigrationSnapshot, StoreSchemaSnapshot, StoreStatusSnapshot,
};
use agl_cron::CronRepository;
use agl_kernel::RunRepository;
use agl_matrix::MatrixOutboxRepository;
use agl_memory::MemoryRepository;
use agl_note::NoteRepository;
use agl_permission::PermissionRepository;
use agl_store::{AglStore, StoreDomain, StoreExportOptions, StoreHandle};

use crate::AgentLibrePaths;

#[derive(Clone)]
pub struct StoreRepositories {
    pub runs: Arc<dyn RunRepository>,
    pub permissions: Arc<dyn PermissionRepository>,
    pub matrix_outbox: Arc<dyn MatrixOutboxRepository>,
    pub cron: Arc<dyn CronRepository>,
    pub memory: Arc<dyn MemoryRepository>,
    pub notes: Arc<dyn NoteRepository>,
    pub content: Arc<dyn ContentRepository>,
    pub artifact_commits: Arc<dyn ArtifactCommitRepository + Send + Sync>,
    pub administration: Arc<dyn StoreAdminPort>,
}

#[derive(Clone)]
pub struct StoreRuntime {
    repositories: StoreRepositories,
}

impl StoreRuntime {
    pub fn open(paths: &AgentLibrePaths) -> agl_store::Result<Self> {
        Self::open_root(paths.store_root())
    }

    pub fn open_root(root: impl AsRef<Path>) -> agl_store::Result<Self> {
        let handle = Arc::new(StoreHandle::open_at(root)?);
        let administration: Arc<dyn StoreAdminPort> = Arc::new(StoreAdminAdapter {
            handle: Arc::clone(&handle),
        });
        let repositories = StoreRepositories {
            runs: handle.clone(),
            permissions: handle.clone(),
            matrix_outbox: handle.clone(),
            cron: handle.clone(),
            memory: handle.clone(),
            notes: handle.clone(),
            content: handle.clone(),
            artifact_commits: handle,
            administration,
        };
        Ok(Self { repositories })
    }

    pub fn inspect(paths: &AgentLibrePaths) -> agl_store::Result<StoreSchemaSnapshot> {
        Self::inspect_root(paths.store_root())
    }

    pub fn migrate(paths: &AgentLibrePaths) -> agl_store::Result<StoreMigrationSnapshot> {
        Self::migrate_root(paths.store_root())
    }

    pub fn inspect_root(root: impl AsRef<Path>) -> agl_store::Result<StoreSchemaSnapshot> {
        AglStore::schema_status_at(root).map(schema_snapshot)
    }

    pub fn migrate_root(root: impl AsRef<Path>) -> agl_store::Result<StoreMigrationSnapshot> {
        AglStore::migrate_at(root).map(migration_snapshot)
    }

    pub fn repositories(&self) -> &StoreRepositories {
        &self.repositories
    }

    pub fn into_repositories(self) -> StoreRepositories {
        self.repositories
    }
}

struct StoreAdminAdapter {
    handle: Arc<StoreHandle>,
}

impl StoreAdminPort for StoreAdminAdapter {
    fn schema_status(&self) -> anyhow::Result<StoreSchemaSnapshot> {
        self.handle
            .schema_status()
            .map(schema_snapshot)
            .map_err(Into::into)
    }

    fn status(&self) -> anyhow::Result<StoreStatusSnapshot> {
        let status = self.handle.status()?;
        Ok(StoreStatusSnapshot {
            idempotency_in_progress: status.idempotency.in_progress,
            stale_idempotency_count: status.idempotency.stale_in_progress.len(),
            domains: status
                .domains
                .into_iter()
                .map(|domain| StoreDomainSnapshot {
                    name: domain.domain.as_str().to_owned(),
                    status: domain.status.as_str().to_owned(),
                    total_rows: domain.total_rows,
                    active_rows: domain.active_rows,
                })
                .collect(),
        })
    }

    fn export_jsonl(
        &self,
        domain: StoreAdminDomain,
        include_deleted: bool,
    ) -> anyhow::Result<StoreExportSnapshot> {
        let domain = match domain {
            StoreAdminDomain::Memory => StoreDomain::Memory,
            StoreAdminDomain::Notes => StoreDomain::Notes,
            StoreAdminDomain::Cron => StoreDomain::Cron,
            StoreAdminDomain::Permissions => StoreDomain::Permissions,
        };
        let (record_count, bytes) = self.handle.export_jsonl(&StoreExportOptions {
            domain,
            include_deleted,
        })?;
        Ok(StoreExportSnapshot {
            record_count,
            bytes,
        })
    }

    fn migrate(&self) -> anyhow::Result<StoreMigrationSnapshot> {
        self.handle
            .migrate()
            .map(migration_snapshot)
            .map_err(Into::into)
    }
}

fn schema_snapshot(status: agl_store::StoreSchemaStatus) -> StoreSchemaSnapshot {
    StoreSchemaSnapshot {
        database_path: status.database_path,
        database_exists: status.database_exists,
        schema_version: status.schema_version,
        current_schema_version: status.current_schema_version,
        applied_migrations: status.applied_migrations,
        migration_required: status.migration_required,
    }
}

fn migration_snapshot(report: agl_store::StoreMigrationReport) -> StoreMigrationSnapshot {
    StoreMigrationSnapshot {
        database_path: report.database_path,
        before_schema_version: report.before_schema_version,
        after_schema_version: report.after_schema_version,
        applied_migrations: report
            .applied_migrations
            .into_iter()
            .map(|migration| StoreAppliedMigrationSnapshot {
                version: migration.version,
                name: migration.name,
            })
            .collect(),
    }
}
