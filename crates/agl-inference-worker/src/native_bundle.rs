use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

const NATIVE_BUNDLE_DIRECTORY: &str = env!("AGL_INFERENCE_NATIVE_RELATIVE_DIR");
const MAX_NATIVE_BUNDLE_FILES: usize = 64;
const MAX_NATIVE_BUNDLE_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_NATIVE_BUNDLE_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;

include!(concat!(env!("OUT_DIR"), "/native_bundle_manifest.rs"));

pub(crate) const fn expected_identity() -> &'static str {
    EXPECTED_NATIVE_BUNDLE_ID
}

pub(crate) struct ValidatedNativeBundle {
    directory: PathBuf,
}

impl ValidatedNativeBundle {
    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }
}

pub(crate) fn validate_for_current_executable() -> Result<ValidatedNativeBundle, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("failed to resolve exact worker executable: {error}"))?;
    let parent = executable
        .parent()
        .ok_or_else(|| "exact worker executable has no sibling directory".to_string())?;
    validate_directory(&parent.join(NATIVE_BUNDLE_DIRECTORY))
}

fn validate_directory(directory: &Path) -> Result<ValidatedNativeBundle, String> {
    let expected_leaf = EXPECTED_NATIVE_BUNDLE_ID
        .strip_prefix("sha256:")
        .map(|digest| format!("sha256-{digest}"))
        .ok_or_else(|| "compile-time native bundle identity is malformed".to_string())?;
    if directory.file_name().and_then(|name| name.to_str()) != Some(expected_leaf.as_str()) {
        return Err(
            "native inference bundle directory does not match the compile-time manifest"
                .to_string(),
        );
    }
    let metadata = fs::symlink_metadata(directory).map_err(|error| {
        format!(
            "native inference bundle {} is unavailable: {error}",
            directory.display()
        )
    })?;
    validate_metadata(directory, &metadata, true)?;

    let expected = EXPECTED_NATIVE_BUNDLE
        .iter()
        .map(|(name, _)| (*name).to_owned())
        .collect::<BTreeSet<_>>();
    let mut actual = BTreeSet::new();
    for entry in fs::read_dir(directory)
        .map_err(|error| format!("failed to list native inference bundle: {error}"))?
    {
        if actual.len() >= MAX_NATIVE_BUNDLE_FILES {
            return Err("native inference bundle exceeds its file-count bound".to_string());
        }
        let name = entry
            .map_err(|error| format!("failed to read native bundle entry: {error}"))?
            .file_name()
            .into_string()
            .map_err(|_| "native bundle contains a non-UTF-8 filename".to_string())?;
        actual.insert(name);
    }
    if actual != expected {
        return Err("native inference bundle has missing or unexpected files".to_string());
    }

    let mut total_bytes = 0_u64;
    for (name, expected_digest) in EXPECTED_NATIVE_BUNDLE {
        let path = directory.join(name);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("failed to inspect native bundle file {name}: {error}"))?;
        validate_metadata(&path, &metadata, false)?;
        if metadata.len() > MAX_NATIVE_BUNDLE_FILE_BYTES {
            return Err(format!("native inference bundle file is oversized: {name}"));
        }
        total_bytes = total_bytes
            .checked_add(metadata.len())
            .ok_or_else(|| "native inference bundle byte count overflowed".to_string())?;
        if total_bytes > MAX_NATIVE_BUNDLE_TOTAL_BYTES {
            return Err("native inference bundle exceeds its total byte bound".to_string());
        }
        let actual_digest = sha256_file(&path)?;
        if &actual_digest != expected_digest {
            return Err(format!(
                "native inference bundle digest mismatch for {name}"
            ));
        }
    }

    Ok(ValidatedNativeBundle {
        directory: directory.to_path_buf(),
    })
}

fn validate_metadata(path: &Path, metadata: &fs::Metadata, directory: bool) -> Result<(), String> {
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
        || (!directory && metadata.nlink() != 1)
    {
        return Err(format!(
            "native inference bundle path is not an exact regular {}: {}",
            if directory { "directory" } else { "file" },
            path.display()
        ));
    }
    let mode = metadata.permissions().mode();
    if mode & 0o7777 != 0o555 {
        return Err(format!(
            "native inference bundle path is not exact mode 0555: {}",
            path.display()
        ));
    }
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid && metadata.uid() != 0 {
        return Err(format!(
            "native inference bundle path has an unexpected owner: {}",
            path.display()
        ));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<[u8; 32], String> {
    let mut file = File::open(path).map_err(|error| {
        format!(
            "failed to open native bundle file {}: {error}",
            path.display()
        )
    })?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            format!(
                "failed to hash native bundle file {}: {error}",
                path.display()
            )
        })?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "agl-worker-native-bundle-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create native bundle fixture root");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            make_tree_owner_writable(&self.0);
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn make_tree_owner_writable(path: &Path) {
        let Ok(metadata) = fs::symlink_metadata(path) else {
            return;
        };
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    make_tree_owner_writable(&entry.path());
                }
            }
        } else if !metadata.file_type().is_symlink() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o644));
        }
    }

    fn copied_bundle_fixture() -> (TestDirectory, PathBuf) {
        let fixture = TestDirectory::new();
        let source_executable = std::env::current_exe().expect("resolve test executable");
        let source = source_executable
            .parent()
            .expect("test executable has a parent")
            .join(NATIVE_BUNDLE_DIRECTORY);
        let destination = fixture.0.join(NATIVE_BUNDLE_DIRECTORY);
        fs::create_dir_all(
            destination
                .parent()
                .expect("native bundle fixture has a base directory"),
        )
        .expect("create native bundle fixture base");
        fs::create_dir(&destination).expect("create native bundle fixture leaf");
        for (name, _) in EXPECTED_NATIVE_BUNDLE {
            let path = destination.join(name);
            fs::copy(source.join(name), &path).expect("copy exact native bundle fixture file");
            fs::set_permissions(&path, fs::Permissions::from_mode(0o555))
                .expect("seal native bundle fixture file");
        }
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o555))
            .expect("seal native bundle fixture leaf");
        (fixture, destination)
    }

    #[test]
    fn staged_bundle_matches_compile_time_manifest() {
        let bundle = validate_for_current_executable().expect("validate staged native bundle");
        assert!(bundle.directory().ends_with(NATIVE_BUNDLE_DIRECTORY));
    }

    #[test]
    fn exact_manifest_rejects_substitution_and_unsafe_metadata() {
        let (_fixture, bundle) = copied_bundle_fixture();
        validate_directory(&bundle).expect("accept exact copied bundle");

        let base = bundle.parent().expect("bundle has a base directory");
        let other = base.join(format!("sha256-{}", "0".repeat(64)));
        fs::create_dir(&other).expect("create validly named other leaf");
        assert!(validate_directory(&other).is_err());

        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o755))
            .expect("make bundle writable for extra-file fixture");
        fs::write(bundle.join("unexpected.so"), b"unexpected")
            .expect("write unexpected bundle file");
        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o555))
            .expect("reseal bundle with unexpected file");
        assert!(validate_directory(&bundle).is_err());
        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o755))
            .expect("make bundle writable to remove extra file");
        fs::remove_file(bundle.join("unexpected.so")).expect("remove unexpected bundle file");

        let first = bundle.join(EXPECTED_NATIVE_BUNDLE[0].0);
        fs::set_permissions(&first, fs::Permissions::from_mode(0o755))
            .expect("make bundle file writable for digest fixture");
        fs::write(&first, b"substituted bytes").expect("substitute bundle file bytes");
        fs::set_permissions(&first, fs::Permissions::from_mode(0o555))
            .expect("reseal substituted bundle file");
        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o555))
            .expect("reseal digest-mismatch bundle");
        assert!(validate_directory(&bundle).is_err());

        let (_fixture, bundle) = copied_bundle_fixture();
        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o755))
            .expect("make selected bundle directory writable");
        assert!(validate_directory(&bundle).is_err());

        let (_fixture, bundle) = copied_bundle_fixture();
        let first = bundle.join(EXPECTED_NATIVE_BUNDLE[0].0);
        let outside = bundle
            .parent()
            .expect("bundle has a base")
            .join("linked.so");
        fs::hard_link(&first, &outside).expect("add a second link to native bundle file");
        assert!(validate_directory(&bundle).is_err());

        let (_fixture, bundle) = copied_bundle_fixture();
        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o755))
            .expect("make bundle writable for symlink fixture");
        let first = bundle.join(EXPECTED_NATIVE_BUNDLE[0].0);
        let outside = bundle
            .parent()
            .expect("bundle has a base")
            .join("target.so");
        fs::rename(&first, &outside).expect("move exact bundle file outside selected leaf");
        symlink(&outside, &first).expect("substitute native bundle symlink");
        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o555))
            .expect("reseal symlink-substituted bundle");
        assert!(validate_directory(&bundle).is_err());
    }
}
