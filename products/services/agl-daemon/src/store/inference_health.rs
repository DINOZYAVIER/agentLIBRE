use agl_core::agent::PackageDigest;
use agl_runtime::inference::{
    DriverBuildDigest, EngineBuildDigest, InferenceHealthUpdate, PhysicalDeviceDigest,
    ResourceQuarantine, RestoredInferenceHealth, RuntimeProfileDigest, WorkerHealth,
};
use rusqlite::params;

use crate::store::{StoreError, StoreHandle};

impl StoreHandle {
    pub fn inference_health(&self) -> crate::store::Result<RestoredInferenceHealth> {
        self.read(|connection| {
            let mut workers = Vec::new();
            let mut statement = connection.prepare(
                "SELECT physical_device, driver_build, engine_build, crash_streak,
                        retry_after_ms, last_failure_kind
                 FROM inference_worker_health ORDER BY physical_device, driver_build, engine_build",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, u32>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })?;
            for row in rows {
                let row = row?;
                workers.push(WorkerHealth {
                    physical_device: PhysicalDeviceDigest::from_bytes(digest_bytes(row.0)?),
                    driver_build: DriverBuildDigest::from_bytes(digest_bytes(row.1)?),
                    engine_build: EngineBuildDigest::from_bytes(digest_bytes(row.2)?),
                    crash_streak: row.3,
                    retry_after_ms: row.4,
                    last_failure_kind: serde_json::from_str(&row.5)?,
                });
            }

            let mut quarantines = Vec::new();
            let mut statement = connection.prepare(
                "SELECT physical_device, driver_build, engine_build, model,
                        runtime_profile, admitted_host_bytes, observed_host_bytes,
                        admitted_device_bytes, observed_device_bytes,
                        admitted_shared_bytes, observed_shared_bytes, recorded_at_ms
                 FROM inference_resource_quarantine
                 ORDER BY physical_device, driver_build, engine_build, model, runtime_profile",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, u64>(5)?,
                    row.get::<_, u64>(6)?,
                    row.get::<_, u64>(7)?,
                    row.get::<_, u64>(8)?,
                    row.get::<_, u64>(9)?,
                    row.get::<_, u64>(10)?,
                    row.get::<_, i64>(11)?,
                ))
            })?;
            for row in rows {
                let row = row?;
                quarantines.push(ResourceQuarantine {
                    physical_device: PhysicalDeviceDigest::from_bytes(digest_bytes(row.0)?),
                    driver_build: DriverBuildDigest::from_bytes(digest_bytes(row.1)?),
                    engine_build: EngineBuildDigest::from_bytes(digest_bytes(row.2)?),
                    model: PackageDigest::from_bytes(digest_bytes(row.3)?),
                    runtime_profile: RuntimeProfileDigest::from_bytes(digest_bytes(row.4)?),
                    admitted_host_bytes: row.5,
                    observed_host_bytes: row.6,
                    admitted_device_bytes: row.7,
                    observed_device_bytes: row.8,
                    admitted_shared_bytes: row.9,
                    observed_shared_bytes: row.10,
                    recorded_at_ms: row.11,
                });
            }
            Ok(RestoredInferenceHealth {
                workers,
                quarantines,
            })
        })
    }

    pub fn put_inference_health_updates(
        &self,
        updates: &[InferenceHealthUpdate],
    ) -> crate::store::Result<()> {
        let store = self.lock()?;
        store.transaction(|transaction| {
            for update in updates {
                match update {
                    InferenceHealthUpdate::Worker(worker) => put_worker(transaction, worker)?,
                    InferenceHealthUpdate::Quarantine(quarantine) => {
                        put_quarantine(transaction, quarantine)?
                    }
                }
            }
            Ok(())
        })
    }
}

fn put_worker(
    connection: &rusqlite::Connection,
    health: &WorkerHealth,
) -> crate::store::Result<()> {
    connection.execute(
        "INSERT INTO inference_worker_health (
            physical_device, driver_build, engine_build, crash_streak,
            retry_after_ms, last_failure_kind
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(physical_device, driver_build, engine_build) DO UPDATE SET
            crash_streak=excluded.crash_streak,
            retry_after_ms=excluded.retry_after_ms,
            last_failure_kind=excluded.last_failure_kind",
        params![
            health.physical_device.as_bytes().as_slice(),
            health.driver_build.as_bytes().as_slice(),
            health.engine_build.as_bytes().as_slice(),
            health.crash_streak,
            health.retry_after_ms,
            serde_json::to_string(&health.last_failure_kind)?,
        ],
    )?;
    Ok(())
}

fn put_quarantine(
    connection: &rusqlite::Connection,
    quarantine: &ResourceQuarantine,
) -> crate::store::Result<()> {
    connection.execute(
        "INSERT INTO inference_resource_quarantine (
            physical_device, driver_build, engine_build, model, runtime_profile,
            admitted_host_bytes, observed_host_bytes, admitted_device_bytes,
            observed_device_bytes, admitted_shared_bytes, observed_shared_bytes,
            recorded_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
         ON CONFLICT(physical_device, driver_build, engine_build, model, runtime_profile)
         DO UPDATE SET
            admitted_host_bytes=excluded.admitted_host_bytes,
            observed_host_bytes=excluded.observed_host_bytes,
            admitted_device_bytes=excluded.admitted_device_bytes,
            observed_device_bytes=excluded.observed_device_bytes,
            admitted_shared_bytes=excluded.admitted_shared_bytes,
            observed_shared_bytes=excluded.observed_shared_bytes,
            recorded_at_ms=excluded.recorded_at_ms",
        params![
            quarantine.physical_device.as_bytes().as_slice(),
            quarantine.driver_build.as_bytes().as_slice(),
            quarantine.engine_build.as_bytes().as_slice(),
            quarantine.model.as_bytes().as_slice(),
            quarantine.runtime_profile.as_bytes().as_slice(),
            quarantine.admitted_host_bytes,
            quarantine.observed_host_bytes,
            quarantine.admitted_device_bytes,
            quarantine.observed_device_bytes,
            quarantine.admitted_shared_bytes,
            quarantine.observed_shared_bytes,
            quarantine.recorded_at_ms,
        ],
    )?;
    Ok(())
}

pub(crate) fn create_schema(connection: &rusqlite::Connection) -> crate::store::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS inference_worker_health (
            physical_device BLOB NOT NULL CHECK(length(physical_device)=32),
            driver_build BLOB NOT NULL CHECK(length(driver_build)=32),
            engine_build BLOB NOT NULL CHECK(length(engine_build)=32),
            crash_streak INTEGER NOT NULL CHECK(crash_streak >= 0),
            retry_after_ms INTEGER NOT NULL CHECK(retry_after_ms >= 0),
            last_failure_kind TEXT NOT NULL CHECK(json_valid(last_failure_kind)),
            PRIMARY KEY(physical_device, driver_build, engine_build)
        ) STRICT;
        CREATE TABLE IF NOT EXISTS inference_resource_quarantine (
            physical_device BLOB NOT NULL CHECK(length(physical_device)=32),
            driver_build BLOB NOT NULL CHECK(length(driver_build)=32),
            engine_build BLOB NOT NULL CHECK(length(engine_build)=32),
            model BLOB NOT NULL CHECK(length(model)=32),
            runtime_profile BLOB NOT NULL CHECK(length(runtime_profile)=32),
            admitted_host_bytes INTEGER NOT NULL CHECK(admitted_host_bytes >= 0),
            observed_host_bytes INTEGER NOT NULL CHECK(observed_host_bytes >= 0),
            admitted_device_bytes INTEGER NOT NULL CHECK(admitted_device_bytes >= 0),
            observed_device_bytes INTEGER NOT NULL CHECK(observed_device_bytes >= 0),
            admitted_shared_bytes INTEGER NOT NULL CHECK(admitted_shared_bytes >= 0),
            observed_shared_bytes INTEGER NOT NULL CHECK(observed_shared_bytes >= 0),
            recorded_at_ms INTEGER NOT NULL CHECK(recorded_at_ms >= 0),
            PRIMARY KEY(physical_device, driver_build, engine_build, model, runtime_profile)
        ) STRICT;",
    )?;
    Ok(())
}

fn digest_bytes(value: Vec<u8>) -> crate::store::Result<[u8; 32]> {
    value.try_into().map_err(|value: Vec<u8>| {
        invalid(
            format!("{} bytes", value.len()),
            "digest BLOB must contain exactly 32 bytes",
        )
    })
}

fn invalid(value: String, reason: &'static str) -> StoreError {
    StoreError::InvalidValue {
        field: "inference health",
        value,
        reason,
    }
}
