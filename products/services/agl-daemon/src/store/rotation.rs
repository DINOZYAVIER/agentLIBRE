use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags};

use super::DEFAULT_DATABASE_FILE;
use super::connection::{configure_writer, initialize_baseline, validate_database_files};
use super::ownership::StoreOwnership;
use super::path::validate_private_regular_file;

#[derive(Debug)]
pub struct StoreRotation {
    pub active: PathBuf,
    pub retained: PathBuf,
}

pub(crate) fn rotate(root: &Path) -> Result<StoreRotation> {
    let _ownership = StoreOwnership::acquire(root).context("acquire exclusive Store ownership")?;
    let active = root.join(DEFAULT_DATABASE_FILE);
    validate_private_regular_file(&active).context("validate original Store")?;
    validate_database_files(&active).context("validate original Store sidecars")?;
    let original = Connection::open_with_flags(
        &active,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .context("open original Store for retention")?;
    original.execute_batch("PRAGMA trusted_schema=OFF;")?;
    let version: u32 = original
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("read original Store version")?;
    let retained = root.join(format!(
        "agentlibre.v{version}.{}.sqlite3",
        uuid::Uuid::now_v7()
    ));
    rotate_to(root, active, retained, original)
}

fn rotate_to(
    root: &Path,
    active: PathBuf,
    retained: PathBuf,
    original: Connection,
) -> Result<StoreRotation> {
    // Reserve the final retained name without replacing an existing file.
    let replacement = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&retained)
        .context("reserve collision-safe retained Store path")?;
    let prepared = (|| -> Result<()> {
        let clean = Connection::open_with_flags(
            &retained,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        configure_writer(&clean)?;
        initialize_baseline(&clean)?;
        clean.close().map_err(|(_, error)| error)?;
        replacement.sync_all()?;

        // Fold committed WAL data into the original before exchanging database
        // names. DELETE mode also refuses a live SQLite reader/writer outside AGL.
        let mode: String = original
            .query_row("PRAGMA journal_mode=DELETE", [], |row| row.get(0))
            .context("checkpoint original Store and release WAL")?;
        ensure!(
            mode.eq_ignore_ascii_case("delete"),
            "original Store did not release WAL"
        );
        original
            .close()
            .map_err(|(_, error)| error)
            .context("close original Store before retention")?;
        File::open(&active)?.sync_all()?;
        File::open(root)?.sync_all()?;
        Ok(())
    })();
    if let Err(error) = prepared {
        // Only the newly reserved clean database belongs to this failed attempt.
        remove_replacement(&retained);
        return Err(error.context("prepare clean Store; original remains authoritative"));
    }
    if let Err(error) = exchange(&active, &retained) {
        remove_replacement(&retained);
        return Err(error.context(
            "atomically retain original and activate clean Store; original remains authoritative",
        ));
    }
    // The exchange is the commit point: neither name is ever absent, including
    // on process interruption. Never delete the retained original after it.
    if let Err(error) = File::open(root).and_then(|directory| directory.sync_all()) {
        exchange(&active, &retained).context(
            "directory sync failed and rollback failed; inspect both Store paths before restarting",
        )?;
        return Err(error).context("persist Store rotation; restored original as authoritative");
    }
    Ok(StoreRotation { active, retained })
}

fn exchange(active: &Path, retained: &Path) -> Result<()> {
    let active = CString::new(active.as_os_str().as_bytes())?;
    let retained = CString::new(retained.as_os_str().as_bytes())?;
    // SAFETY: both C strings remain live; renameat2 atomically exchanges the two
    // existing names on the same filesystem without following their contents.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            active.as_ptr(),
            libc::AT_FDCWD,
            retained.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn remove_replacement(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let mut candidate = path.as_os_str().to_os_string();
        candidate.push(suffix);
        // No original data has been placed at these names before the exchange.
        let _ = std::fs::remove_file(PathBuf::from(candidate));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{STORE_BASELINE_VERSION, StoreHandle};

    #[test]
    fn rotation_preserves_old_rows_and_version_and_creates_empty_current_store() {
        let root = std::env::temp_dir().join(format!("agl-rotation-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&root).unwrap();
        let active = root.join(DEFAULT_DATABASE_FILE);
        let original = Connection::open(&active).unwrap();
        original
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA user_version=8;
            CREATE TABLE evidence(value TEXT); INSERT INTO evidence VALUES('keep me');",
            )
            .unwrap();
        drop(original);
        let result = rotate(&root).unwrap();
        assert_eq!(result.active, active);
        assert!(
            result
                .retained
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("agentlibre.v8.")
        );
        let retained = Connection::open(&result.retained).unwrap();
        assert_eq!(
            retained
                .query_row("SELECT value FROM evidence", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "keep me"
        );
        assert_eq!(
            retained
                .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
                .unwrap(),
            8
        );
        let clean = StoreHandle::open_at(&root).unwrap();
        assert_eq!(
            clean
                .read(|connection| Ok(
                    connection.query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))?
                ))
                .unwrap(),
            STORE_BASELINE_VERSION
        );
        assert_eq!(
            clean
                .read(|connection| Ok(connection.query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name='evidence'",
                    [],
                    |row| row.get::<_, u32>(0)
                )?))
                .unwrap(),
            0
        );
        assert!(
            rotate(&root)
                .unwrap_err()
                .to_string()
                .contains("exclusive Store ownership")
        );
        assert!(StoreHandle::open_at(&root).is_err());
        drop(clean);
        let second = rotate(&root).unwrap();
        assert_ne!(second.retained, result.retained);
        drop(retained);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn live_sqlite_connection_refuses_rotation_without_losing_wal_rows() {
        let root = std::env::temp_dir().join(format!("agl-rotation-wal-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&root).unwrap();
        let active = root.join(DEFAULT_DATABASE_FILE);
        let original = Connection::open(&active).unwrap();
        original
            .execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE evidence(value TEXT);
            INSERT INTO evidence VALUES('committed WAL data');",
            )
            .unwrap();
        assert!(rotate(&root).is_err());
        assert_eq!(
            original
                .query_row("SELECT value FROM evidence", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "committed WAL data"
        );
        drop(original);
        let result = rotate(&root).unwrap();
        let retained = Connection::open(result.retained).unwrap();
        assert_eq!(
            retained
                .query_row("SELECT value FROM evidence", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "committed WAL data"
        );
        drop(retained);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retained_name_collision_preserves_both_existing_files() {
        let root =
            std::env::temp_dir().join(format!("agl-rotation-collision-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&root).unwrap();
        let active = root.join(DEFAULT_DATABASE_FILE);
        let original = Connection::open(&active).unwrap();
        original
            .execute_batch("CREATE TABLE evidence(value TEXT);")
            .unwrap();
        let before = std::fs::read(&active).unwrap();
        let retained = root.join("already-retained.sqlite3");
        std::fs::write(&retained, b"existing retained data").unwrap();
        assert!(rotate_to(&root, active.clone(), retained.clone(), original).is_err());
        assert_eq!(std::fs::read(active).unwrap(), before);
        assert_eq!(std::fs::read(retained).unwrap(), b"existing retained data");
        std::fs::remove_dir_all(root).unwrap();
    }
}
