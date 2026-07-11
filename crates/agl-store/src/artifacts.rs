use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agl_content::{
    ArtifactId, ArtifactRef, ArtifactRetention, ArtifactSensitivity, ArtifactSource, BlobDigest,
    ImageDimensions, MediaType,
};
use agl_ids::RunId;
use rusqlite::{OptionalExtension, params};

use crate::path::{ensure_private_dir, set_private_file_permissions};
use crate::{AglStore, Result, StoreError};

const BLOBS_DIR: &str = "blobs";
const TEMP_DIR: &str = ".tmp";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ArtifactWriteFailpoint {
    BeforeBlobWrite,
    AfterBlobWrite,
    BeforeMetadataCommit,
    AfterMetadataCommit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredArtifact {
    pub run_id: RunId,
    pub reference: ArtifactRef,
    pub retention: ArtifactRetention,
    pub tombstoned: bool,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedArtifact {
    pub reference: ArtifactRef,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ArtifactGcReport {
    pub artifact_records_deleted: usize,
    pub blob_records_deleted: usize,
    pub blob_files_deleted: usize,
    pub orphan_files_deleted: usize,
}

impl AglStore {
    #[allow(clippy::too_many_arguments)]
    pub fn write_artifact(
        &self,
        run_id: &RunId,
        media_type: MediaType,
        bytes: &[u8],
        image: Option<ImageDimensions>,
        sensitivity: ArtifactSensitivity,
        source: ArtifactSource,
        retention: ArtifactRetention,
    ) -> Result<StoredArtifact> {
        self.write_artifact_inner(
            run_id,
            media_type,
            bytes,
            image,
            sensitivity,
            source,
            retention,
            None,
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_artifact_injected(
        &self,
        run_id: &RunId,
        media_type: MediaType,
        bytes: &[u8],
        image: Option<ImageDimensions>,
        sensitivity: ArtifactSensitivity,
        source: ArtifactSource,
        retention: ArtifactRetention,
        failpoint: ArtifactWriteFailpoint,
    ) -> Result<StoredArtifact> {
        self.write_artifact_inner(
            run_id,
            media_type,
            bytes,
            image,
            sensitivity,
            source,
            retention,
            Some(failpoint),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn write_artifact_inner(
        &self,
        run_id: &RunId,
        media_type: MediaType,
        bytes: &[u8],
        image: Option<ImageDimensions>,
        sensitivity: ArtifactSensitivity,
        source: ArtifactSource,
        retention: ArtifactRetention,
        failpoint: Option<ArtifactWriteFailpoint>,
    ) -> Result<StoredArtifact> {
        if bytes.is_empty() {
            return Err(StoreError::InvalidValue {
                field: "artifact.bytes",
                value: "0".to_string(),
                reason: "artifact bytes cannot be empty",
            });
        }
        let artifact_id = ArtifactId::generate();
        let digest = BlobDigest::from_bytes(bytes);
        let byte_length = u64::try_from(bytes.len()).map_err(|_| StoreError::InvalidValue {
            field: "artifact.bytes",
            value: bytes.len().to_string(),
            reason: "artifact byte length exceeds u64",
        })?;
        let reference = ArtifactRef::new(
            artifact_id,
            digest.clone(),
            media_type,
            byte_length,
            image,
            sensitivity,
            source,
        )?;
        let _lock = ArtifactStoreLock::acquire(self.store_root())?;
        inject_artifact_failure(failpoint, ArtifactWriteFailpoint::BeforeBlobWrite)?;
        self.persist_blob(&digest, bytes)?;
        inject_artifact_failure(failpoint, ArtifactWriteFailpoint::AfterBlobWrite)?;
        let now_ms = unix_millis();
        inject_artifact_failure(failpoint, ArtifactWriteFailpoint::BeforeMetadataCommit)?;
        self.transaction(|transaction| {
            transaction.execute(
                "INSERT INTO content_blobs (digest, media_type, byte_length, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(digest) DO NOTHING",
                params![digest.as_str(), media_type.mime(), byte_length, now_ms],
            )?;
            let stored: (String, u64) = transaction.query_row(
                "SELECT media_type, byte_length FROM content_blobs WHERE digest = ?1",
                [digest.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if stored != (media_type.mime().to_string(), byte_length) {
                return Err(StoreError::ArtifactIntegrityFailed {
                    artifact_id: reference.artifact_id.to_string(),
                    reason: "deduplicated blob metadata differs".to_string(),
                });
            }
            transaction.execute(
                "INSERT INTO artifacts
                 (id, blob_digest, run_id, media_type, byte_length, width, height,
                  sensitivity, source_json, retention, state, created_at_ms, tombstoned_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'live', ?11, NULL)",
                params![
                    reference.artifact_id.as_str(),
                    digest.as_str(),
                    run_id.as_str(),
                    media_type.mime(),
                    byte_length,
                    image.map(|dimensions| dimensions.width),
                    image.map(|dimensions| dimensions.height),
                    sensitivity_name(sensitivity),
                    serde_json::to_string(&reference.source)?,
                    retention_name(retention),
                    now_ms,
                ],
            )?;
            Ok(())
        })?;
        inject_artifact_failure(failpoint, ArtifactWriteFailpoint::AfterMetadataCommit)?;
        Ok(StoredArtifact {
            run_id: run_id.clone(),
            reference,
            retention,
            tombstoned: false,
            created_at_ms: now_ms,
        })
    }

    pub fn artifact(&self, artifact_id: &ArtifactId) -> Result<Option<StoredArtifact>> {
        self.connection()
            .query_row(
                "SELECT run_id, blob_digest, media_type, byte_length, width, height,
                        sensitivity, source_json, retention, state, created_at_ms
                 FROM artifacts WHERE id = ?1",
                [artifact_id.as_str()],
                |row| {
                    let run_id: String = row.get(0)?;
                    let digest: String = row.get(1)?;
                    let media_type: String = row.get(2)?;
                    let width: Option<u32> = row.get(4)?;
                    let height: Option<u32> = row.get(5)?;
                    let sensitivity: String = row.get(6)?;
                    let source: String = row.get(7)?;
                    let retention: String = row.get(8)?;
                    let state: String = row.get(9)?;
                    Ok((
                        run_id,
                        digest,
                        media_type,
                        row.get::<_, u64>(3)?,
                        width,
                        height,
                        sensitivity,
                        source,
                        retention,
                        state,
                        row.get::<_, i64>(10)?,
                    ))
                },
            )
            .optional()?
            .map(|row| decode_artifact(artifact_id.clone(), row))
            .transpose()
    }

    pub fn resolve_artifact(
        &self,
        owner_run_id: &RunId,
        reference: &ArtifactRef,
    ) -> Result<ResolvedArtifact> {
        let stored = self.artifact(&reference.artifact_id)?.ok_or_else(|| {
            StoreError::ArtifactUnavailable {
                artifact_id: reference.artifact_id.to_string(),
            }
        })?;
        if &stored.run_id != owner_run_id {
            return Err(StoreError::ArtifactAccessDenied);
        }
        if stored.tombstoned {
            return Err(StoreError::ArtifactUnavailable {
                artifact_id: reference.artifact_id.to_string(),
            });
        }
        if &stored.reference != reference {
            return Err(StoreError::ArtifactIntegrityFailed {
                artifact_id: reference.artifact_id.to_string(),
                reason: "reference metadata differs from the canonical artifact".to_string(),
            });
        }
        let path = self.blob_path(&reference.digest)?;
        let file = File::open(&path).map_err(|_| StoreError::ArtifactUnavailable {
            artifact_id: reference.artifact_id.to_string(),
        })?;
        let capacity = usize::try_from(reference.byte_length).map_err(|_| {
            StoreError::ArtifactIntegrityFailed {
                artifact_id: reference.artifact_id.to_string(),
                reason: "artifact length exceeds process address space".to_string(),
            }
        })?;
        let mut bytes = Vec::with_capacity(capacity);
        file.take(reference.byte_length.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() != capacity || BlobDigest::from_bytes(&bytes) != reference.digest {
            return Err(StoreError::ArtifactIntegrityFailed {
                artifact_id: reference.artifact_id.to_string(),
                reason: "blob length or digest mismatch".to_string(),
            });
        }
        Ok(ResolvedArtifact {
            reference: reference.clone(),
            bytes,
        })
    }

    pub fn tombstone_run_artifacts(&self, run_id: &RunId) -> Result<usize> {
        let now_ms = unix_millis();
        Ok(self.connection().execute(
            "UPDATE artifacts SET state = 'tombstoned', tombstoned_at_ms = ?2
             WHERE run_id = ?1 AND state = 'live' AND retention = 'run_scoped'",
            params![run_id.as_str(), now_ms],
        )?)
    }

    pub fn garbage_collect_artifacts(&self) -> Result<ArtifactGcReport> {
        let _lock = ArtifactStoreLock::acquire(self.store_root())?;
        let artifact_records_deleted = self.transaction(|transaction| {
            Ok(transaction.execute("DELETE FROM artifacts WHERE state = 'tombstoned'", [])?)
        })?;
        let mut statement = self.connection().prepare(
            "SELECT digest FROM content_blobs
             WHERE NOT EXISTS (
                 SELECT 1 FROM artifacts WHERE artifacts.blob_digest = content_blobs.digest
             )",
        )?;
        let digests = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);
        let mut report = ArtifactGcReport {
            artifact_records_deleted,
            ..ArtifactGcReport::default()
        };
        for digest in digests {
            let parsed = BlobDigest::parse(digest.clone())?;
            report.blob_records_deleted += self
                .connection()
                .execute("DELETE FROM content_blobs WHERE digest = ?1", [&digest])?;
            let path = self.blob_path(&parsed)?;
            if path.exists() {
                std::fs::remove_file(path)?;
                report.blob_files_deleted += 1;
            }
        }
        report.orphan_files_deleted = self.collect_orphan_blob_files()?;
        Ok(report)
    }

    fn persist_blob(&self, digest: &BlobDigest, bytes: &[u8]) -> Result<()> {
        let destination = self.blob_path(digest)?;
        let parent = destination
            .parent()
            .ok_or_else(|| StoreError::InvalidPath {
                path: destination.clone(),
                reason: "blob path has no parent",
            })?;
        ensure_private_dir(parent)?;
        let temp_root = self.store_root().join(BLOBS_DIR).join(TEMP_DIR);
        ensure_private_dir(&temp_root)?;
        let temp = temp_root.join(ArtifactId::generate().as_str());
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        let mut temp_guard = TempFileGuard::new(temp.clone());
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        set_private_file_permissions(&temp)?;
        match std::fs::hard_link(&temp, &destination) {
            Ok(()) => {
                set_private_file_permissions(&destination)?;
                sync_directory(parent)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                verify_blob_file(&destination, digest, bytes.len())?;
            }
            Err(error) => return Err(error.into()),
        }
        std::fs::remove_file(&temp)?;
        temp_guard.disarm();
        sync_directory(&temp_root)?;
        Ok(())
    }

    fn collect_orphan_blob_files(&self) -> Result<usize> {
        let mut deleted =
            collect_temp_blob_files(&self.store_root().join(BLOBS_DIR).join(TEMP_DIR))?;
        let root = self.store_root().join(BLOBS_DIR).join("sha256");
        if !root.exists() {
            return Ok(deleted);
        }
        for prefix in std::fs::read_dir(root)? {
            let prefix = prefix?;
            if !prefix.file_type()?.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(prefix.path())? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let Some(hex) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                let digest = format!("sha256:{hex}");
                let known: bool = self.connection().query_row(
                    "SELECT EXISTS(SELECT 1 FROM content_blobs WHERE digest = ?1)",
                    [&digest],
                    |row| row.get(0),
                )?;
                if !known {
                    std::fs::remove_file(entry.path())?;
                    deleted += 1;
                }
            }
        }
        Ok(deleted)
    }

    fn store_root(&self) -> &Path {
        self.database_path()
            .parent()
            .expect("store database always has a parent")
    }

    fn blob_path(&self, digest: &BlobDigest) -> Result<PathBuf> {
        let hex = digest.hex();
        let prefix = hex.get(..2).ok_or_else(|| StoreError::InvalidPath {
            path: PathBuf::from(hex),
            reason: "blob digest is too short",
        })?;
        Ok(self
            .store_root()
            .join(BLOBS_DIR)
            .join("sha256")
            .join(prefix)
            .join(hex))
    }
}

struct TempFileGuard {
    path: Option<PathBuf>,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

struct ArtifactStoreLock {
    file: File,
}

impl ArtifactStoreLock {
    fn acquire(store_root: &Path) -> Result<Self> {
        let blob_root = store_root.join(BLOBS_DIR);
        ensure_private_dir(&blob_root)?;
        let path = blob_root.join(".artifact.lock");
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        set_private_file_permissions(&path)?;
        lock_file_exclusive(&file)?;
        Ok(Self { file })
    }
}

impl Drop for ArtifactStoreLock {
    fn drop(&mut self) {
        unlock_file(&self.file);
    }
}

#[cfg(unix)]
fn lock_file_exclusive(file: &File) -> Result<()> {
    use std::os::fd::AsRawFd;

    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
}

#[cfg(not(unix))]
fn lock_file_exclusive(_file: &File) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn unlock_file(file: &File) {
    use std::os::fd::AsRawFd;

    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
}

#[cfg(not(unix))]
fn unlock_file(_file: &File) {}

fn inject_artifact_failure(
    configured: Option<ArtifactWriteFailpoint>,
    current: ArtifactWriteFailpoint,
) -> Result<()> {
    if configured == Some(current) {
        return Err(StoreError::InvalidValue {
            field: "artifact.failpoint",
            value: format!("{current:?}"),
            reason: "injected artifact write failure",
        });
    }
    Ok(())
}

fn collect_temp_blob_files(root: &Path) -> Result<usize> {
    if !root.exists() {
        return Ok(0);
    }
    let mut deleted = 0;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            std::fs::remove_file(entry.path())?;
            deleted += 1;
        }
    }
    if deleted > 0 {
        sync_directory(root)?;
    }
    Ok(deleted)
}

type RawArtifactRow = (
    String,
    String,
    String,
    u64,
    Option<u32>,
    Option<u32>,
    String,
    String,
    String,
    String,
    i64,
);

fn decode_artifact(artifact_id: ArtifactId, row: RawArtifactRow) -> Result<StoredArtifact> {
    let image = match (row.4, row.5) {
        (Some(width), Some(height)) => Some(ImageDimensions::new(width, height)?),
        (None, None) => None,
        _ => {
            return Err(StoreError::ArtifactIntegrityFailed {
                artifact_id: artifact_id.to_string(),
                reason: "partial image dimensions".to_string(),
            });
        }
    };
    let reference = ArtifactRef::new(
        artifact_id,
        BlobDigest::parse(row.1)?,
        MediaType::parse_mime(&row.2)?,
        row.3,
        image,
        parse_sensitivity(&row.6)?,
        serde_json::from_str(&row.7)?,
    )?;
    Ok(StoredArtifact {
        run_id: RunId::parse(&row.0).map_err(|_| StoreError::ArtifactIntegrityFailed {
            artifact_id: reference.artifact_id.to_string(),
            reason: "invalid owner run ID".to_string(),
        })?,
        reference,
        retention: parse_retention(&row.8)?,
        tombstoned: row.9 == "tombstoned",
        created_at_ms: row.10,
    })
}

fn sensitivity_name(value: ArtifactSensitivity) -> &'static str {
    match value {
        ArtifactSensitivity::Private => "private",
        ArtifactSensitivity::Sensitive => "sensitive",
    }
}

fn parse_sensitivity(value: &str) -> Result<ArtifactSensitivity> {
    match value {
        "private" => Ok(ArtifactSensitivity::Private),
        "sensitive" => Ok(ArtifactSensitivity::Sensitive),
        _ => Err(StoreError::InvalidValue {
            field: "artifacts.sensitivity",
            value: value.to_string(),
            reason: "invalid artifact sensitivity",
        }),
    }
}

fn retention_name(value: ArtifactRetention) -> &'static str {
    match value {
        ArtifactRetention::RunScoped => "run_scoped",
        ArtifactRetention::Persistent => "persistent",
    }
}

fn parse_retention(value: &str) -> Result<ArtifactRetention> {
    match value {
        "run_scoped" => Ok(ArtifactRetention::RunScoped),
        "persistent" => Ok(ArtifactRetention::Persistent),
        _ => Err(StoreError::InvalidValue {
            field: "artifacts.retention",
            value: value.to_string(),
            reason: "invalid artifact retention",
        }),
    }
}

fn verify_blob_file(path: &Path, digest: &BlobDigest, expected_length: usize) -> Result<()> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    if bytes.len() != expected_length || BlobDigest::from_bytes(&bytes) != *digest {
        return Err(StoreError::ArtifactIntegrityFailed {
            artifact_id: "deduplicated_blob".to_string(),
            reason: "existing blob does not match its digest".to_string(),
        });
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

fn unix_millis() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}
