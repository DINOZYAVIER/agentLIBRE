use std::path::PathBuf;

use rusqlite::Connection;

mod agent;
mod connection;
mod error;
mod handle;
mod inference_health;
mod ownership;
pub(crate) mod path;
mod plans;
pub(crate) mod rotation;

pub use agent::AgentRunAdmission;
pub use error::{Result, StoreError};
pub use handle::StoreHandle;

pub const DEFAULT_DATABASE_FILE: &str = "agentlibre.sqlite3";
pub const STORE_BASELINE_VERSION: u32 = 19;

const STORE_APPLICATION_ID: u32 = 0x4147_4c32;

pub(crate) fn unfinished_agent_runs_at(
    root: impl AsRef<std::path::Path>,
) -> Result<Vec<agl_core::AgentRunId>> {
    let path = path::database_path(root.as_ref(), DEFAULT_DATABASE_FILE)?;
    if !path.try_exists()? {
        return Ok(Vec::new());
    }
    connection::validate_database_files(&path)?;
    path::validate_private_regular_file(&path)?;
    let connection = connection::open_reader(&path)?;
    let mut statement = connection.prepare(
        "SELECT agent_run_id FROM agent_runs WHERE status IN ('pending','running') ORDER BY rowid",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    rows.map(|row| {
        let bytes = row?;
        agl_core::AgentRunId::from_bytes(bytes.try_into().map_err(|_| {
            rusqlite::Error::InvalidColumnType(
                0,
                "agent_run_id".to_owned(),
                rusqlite::types::Type::Blob,
            )
        })?)
        .map_err(|_| {
            rusqlite::Error::InvalidColumnType(
                0,
                "agent_run_id".to_owned(),
                rusqlite::types::Type::Blob,
            )
        })
        .map_err(StoreError::from)
    })
    .collect()
}

pub(crate) fn preflight_at(root: impl AsRef<std::path::Path>) -> Result<()> {
    let path = path::database_path(root.as_ref(), DEFAULT_DATABASE_FILE)?;
    connection::validate_database_files(&path)?;
    if !path.try_exists()? {
        return Ok(());
    }
    path::validate_private_regular_file(&path)?;
    connection::require_current_database(&path)
}

#[derive(Debug)]
pub(crate) struct AglStore {
    conn: Connection,
    database_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use agl_core::agent::PackageDigest;
    use agl_runtime::inference::{
        DriverBuildDigest, EngineBuildDigest, InferenceFailureKind, PhysicalDeviceDigest,
        RuntimeProfileDigest, WorkerHealth,
    };

    use super::*;

    fn root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("agl-daemon-store-{name}-{}", uuid::Uuid::now_v7()))
    }

    #[test]
    fn incompatible_alpha_database_is_rejected_without_modification() {
        for version in [STORE_BASELINE_VERSION - 1, STORE_BASELINE_VERSION + 1] {
            let root = root("cutover");
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join(DEFAULT_DATABASE_FILE);
            let old = Connection::open(&path).unwrap();
            old.pragma_update(None, "application_id", STORE_APPLICATION_ID)
                .unwrap();
            old.pragma_update(None, "user_version", version).unwrap();
            old.execute_batch("CREATE TABLE legacy_runs(id TEXT);")
                .unwrap();
            drop(old);
            let original = std::fs::read(&path).unwrap();
            let error = StoreHandle::open_at(&root).unwrap_err();
            assert!(matches!(error, StoreError::IncompatibleDatabase {
            version: detected,
            required_version: STORE_BASELINE_VERSION,
            ..
        } if detected == version));
            assert!(error.to_string().contains("agl store rotate"));
            assert_eq!(std::fs::read(&path).unwrap(), original);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn preflight_is_read_only_and_accepts_an_absent_store() {
        let root = root("preflight");
        assert!(preflight_at(&root).is_ok());
        assert!(!root.exists());

        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(DEFAULT_DATABASE_FILE);
        let old = Connection::open(&path).unwrap();
        old.pragma_update(None, "application_id", STORE_APPLICATION_ID)
            .unwrap();
        old.pragma_update(None, "user_version", STORE_BASELINE_VERSION - 1)
            .unwrap();
        drop(old);
        let before = std::fs::read(&path).unwrap();
        assert!(matches!(
            preflight_at(&root),
            Err(StoreError::IncompatibleDatabase { .. })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_database_is_rejected_without_modification() {
        let root = root("corrupt");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(DEFAULT_DATABASE_FILE);
        let original = b"not a SQLite database; preserve forensic evidence";
        std::fs::write(&path, original).unwrap();
        assert!(matches!(
            StoreHandle::open_at(&root),
            Err(StoreError::Sqlite(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn store_rejects_symlink_and_hard_link_managed_paths_without_touching_targets() {
        use std::os::unix::fs::symlink;

        let parent = root("managed-paths");
        let real_root = parent.join("real");
        let linked_root = parent.join("linked");
        std::fs::create_dir_all(&real_root).unwrap();
        symlink(&real_root, &linked_root).unwrap();
        assert!(matches!(
            StoreHandle::open_at(&linked_root),
            Err(StoreError::InvalidPath { .. })
        ));

        let store_root = parent.join("store");
        std::fs::create_dir_all(&store_root).unwrap();
        let outside = parent.join("outside.sqlite3");
        std::fs::write(&outside, b"must remain").unwrap();
        symlink(&outside, store_root.join(DEFAULT_DATABASE_FILE)).unwrap();
        assert!(matches!(
            StoreHandle::open_at(&store_root),
            Err(StoreError::InvalidPath { .. })
        ));
        assert_eq!(std::fs::read(&outside).unwrap(), b"must remain");

        std::fs::remove_file(store_root.join(DEFAULT_DATABASE_FILE)).unwrap();
        std::fs::hard_link(&outside, store_root.join(DEFAULT_DATABASE_FILE)).unwrap();
        assert!(matches!(
            StoreHandle::open_at(&store_root),
            Err(StoreError::InvalidPath { .. })
        ));
        assert_eq!(std::fs::read(&outside).unwrap(), b"must remain");
        let _ = std::fs::remove_dir_all(parent);
    }

    #[test]
    fn store_rejects_linked_sidecars_before_sqlite_can_touch_them() {
        use std::os::unix::fs::symlink;

        for suffix in ["-wal", "-shm"] {
            for hard_link in [false, true] {
                let root = root("sidecar-links");
                drop(StoreHandle::open_at(&root).unwrap());
                let outside = root.join("outside");
                std::fs::write(&outside, b"preserve outside data").unwrap();
                let sidecar = root.join(format!("{DEFAULT_DATABASE_FILE}{suffix}"));
                if hard_link {
                    std::fs::hard_link(&outside, &sidecar).unwrap();
                } else {
                    symlink(&outside, &sidecar).unwrap();
                }
                assert!(matches!(
                    StoreHandle::open_at(&root),
                    Err(StoreError::InvalidPath { .. })
                ));
                assert!(super::rotation::rotate(&root).is_err());
                assert_eq!(std::fs::read(&outside).unwrap(), b"preserve outside data");
                std::fs::remove_dir_all(root).unwrap();
            }
        }
    }

    #[test]
    fn observation_read_does_not_wait_for_writer_mutex() {
        let root = root("readers");
        let handle = StoreHandle::open_at(&root).unwrap();
        let writer = handle.lock().unwrap();
        let reader = handle.clone();
        let (send, receive) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            send.send(reader.agent_event_page(None, 1).unwrap())
                .unwrap();
        });
        assert!(receive.recv_timeout(Duration::from_secs(1)).is_ok());
        drop(writer);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn inference_health_has_one_store_owned_round_trip() {
        let root = root("inference-health");
        let handle = StoreHandle::open_at(&root).unwrap();
        let digest = |byte: char| format!("sha256:{}", byte.to_string().repeat(64));
        let worker = WorkerHealth {
            physical_device: PhysicalDeviceDigest::parse(digest('1')).unwrap(),
            driver_build: DriverBuildDigest::parse(digest('2')).unwrap(),
            engine_build: EngineBuildDigest::parse(digest('3')).unwrap(),
            crash_streak: 2,
            retry_after_ms: 42,
            last_failure_kind: InferenceFailureKind::UnattributedSignal {
                signal: libc::SIGKILL,
            },
        };
        let quarantine = agl_runtime::inference::ResourceQuarantine {
            physical_device: worker.physical_device,
            driver_build: worker.driver_build,
            engine_build: worker.engine_build,
            model: digest('4').parse::<PackageDigest>().unwrap(),
            runtime_profile: RuntimeProfileDigest::parse(digest('5')).unwrap(),
            admitted_host_bytes: 1,
            observed_host_bytes: 2,
            admitted_device_bytes: 3,
            observed_device_bytes: 4,
            admitted_shared_bytes: 5,
            observed_shared_bytes: 6,
            recorded_at_ms: 7,
        };
        handle
            .put_inference_health_updates(&[
                agl_runtime::inference::InferenceHealthUpdate::Worker(worker.clone()),
                agl_runtime::inference::InferenceHealthUpdate::Quarantine(quarantine.clone()),
            ])
            .unwrap();
        let restored = handle.inference_health().unwrap();
        assert_eq!(restored.workers, vec![worker]);
        assert_eq!(restored.quarantines, vec![quarantine]);
        handle
            .read(|connection| {
                let worker_shape: (String, i64, String, i64, String, i64) = connection.query_row(
                    "SELECT typeof(physical_device), length(physical_device),
                            typeof(driver_build), length(driver_build),
                            typeof(engine_build), length(engine_build)
                     FROM inference_worker_health",
                    [],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    },
                )?;
                assert_eq!(
                    worker_shape,
                    ("blob".into(), 32, "blob".into(), 32, "blob".into(), 32)
                );
                let quarantine_shape: (String, i64, String, i64) = connection.query_row(
                    "SELECT typeof(model), length(model),
                            typeof(runtime_profile), length(runtime_profile)
                     FROM inference_resource_quarantine",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )?;
                assert_eq!(quarantine_shape, ("blob".into(), 32, "blob".into(), 32));
                Ok(())
            })
            .unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
