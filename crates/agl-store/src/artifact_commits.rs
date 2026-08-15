use agl_artifact::{ArtifactCommitError, ArtifactCommitRecord, ArtifactCommitRepository};
use rusqlite::{OptionalExtension, params};

use crate::{AglStore, StoreHandle};

impl ArtifactCommitRepository for AglStore {
    fn save(&self, record: ArtifactCommitRecord) -> Result<(), ArtifactCommitError> {
        let record_json = serde_json::to_string(&record)
            .map_err(|error| ArtifactCommitError::Repository(error.to_string()))?;
        let correlation_json = serde_json::to_string(record.correlation())
            .map_err(|error| ArtifactCommitError::Repository(error.to_string()))?;

        let existing = self
            .conn
            .query_row(
                "SELECT revision, correlation_json, record_json
                 FROM artifact_commit_operations WHERE operation_id = ?1",
                [record.operation_id()],
                |row| {
                    Ok((
                        row.get::<_, u64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(repository_error)?;

        if let Some((revision, existing_correlation, existing_json)) = existing {
            let existing_record: ArtifactCommitRecord = serde_json::from_str(&existing_json)
                .map_err(|error| ArtifactCommitError::Repository(error.to_string()))?;
            if existing_record.correlation() != record.correlation()
                || existing_record.prepare() != record.prepare()
            {
                return Err(ArtifactCommitError::IdentityConflict);
            }
            if revision > record.revision() {
                return Err(ArtifactCommitError::Repository(
                    "artifact commit revision cannot move backwards".to_owned(),
                ));
            }
            if revision == record.revision() {
                if existing_json == record_json {
                    return Ok(());
                }
                return Err(ArtifactCommitError::IdentityConflict);
            }
            if matches!(
                existing_record.state(),
                agl_artifact::ArtifactCommitState::Committed { .. }
                    | agl_artifact::ArtifactCommitState::Failed { .. }
                    | agl_artifact::ArtifactCommitState::Conflict { .. }
            ) {
                return Err(ArtifactCommitError::Terminal);
            }
            if existing_correlation
                .as_deref()
                .is_some_and(|existing| existing != correlation_json)
            {
                return Err(ArtifactCommitError::IdentityConflict);
            }
        }

        self.conn
            .execute(
                "INSERT INTO artifact_commit_operations
                    (operation_id, revision, state, correlation_json, record_json)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(operation_id) DO UPDATE SET
                    revision = excluded.revision,
                    state = excluded.state,
                    correlation_json = COALESCE(
                        artifact_commit_operations.correlation_json,
                        excluded.correlation_json
                    ),
                    record_json = excluded.record_json",
                params![
                    record.operation_id(),
                    record.revision(),
                    record.state_name(),
                    Some(correlation_json),
                    record_json,
                ],
            )
            .map_err(repository_error)?;
        Ok(())
    }

    fn load(&self, operation_id: &str) -> Result<ArtifactCommitRecord, ArtifactCommitError> {
        let json = self
            .conn
            .query_row(
                "SELECT record_json FROM artifact_commit_operations WHERE operation_id = ?1",
                [operation_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(repository_error)?
            .ok_or_else(|| {
                ArtifactCommitError::Repository(format!("operation `{operation_id}` not found"))
            })?;
        serde_json::from_str(&json)
            .map_err(|error| ArtifactCommitError::Repository(error.to_string()))
    }

    fn incomplete(&self) -> Result<Vec<ArtifactCommitRecord>, ArtifactCommitError> {
        let mut statement = self
            .conn
            .prepare(
                "SELECT record_json FROM artifact_commit_operations
                 WHERE state IN ('prepared', 'child_committed', 'parent_committed')
                 ORDER BY operation_id",
            )
            .map_err(repository_error)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(repository_error)?;
        rows.map(|row| {
            let json = row.map_err(repository_error)?;
            serde_json::from_str(&json)
                .map_err(|error| ArtifactCommitError::Repository(error.to_string()))
        })
        .collect()
    }
}

impl ArtifactCommitRepository for StoreHandle {
    fn save(&self, record: ArtifactCommitRecord) -> Result<(), ArtifactCommitError> {
        self.lock()
            .map_err(|error| ArtifactCommitError::Repository(error.to_string()))?
            .save(record)
    }

    fn load(&self, operation_id: &str) -> Result<ArtifactCommitRecord, ArtifactCommitError> {
        self.lock()
            .map_err(|error| ArtifactCommitError::Repository(error.to_string()))?
            .load(operation_id)
    }

    fn incomplete(&self) -> Result<Vec<ArtifactCommitRecord>, ArtifactCommitError> {
        self.lock()
            .map_err(|error| ArtifactCommitError::Repository(error.to_string()))?
            .incomplete()
    }
}

fn repository_error(error: rusqlite::Error) -> ArtifactCommitError {
    ArtifactCommitError::Repository(error.to_string())
}
