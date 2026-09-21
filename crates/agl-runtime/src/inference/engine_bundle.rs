use std::fs::{self, File};
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use sha2::{Digest as _, Sha256};

use crate::inference::EngineBuildDigest;

const ENGINE_BUILD_DOMAIN: &[u8] = b"agentlibre.private-engine-build.v1\0";
const ENGINE_EXECUTABLE: &str = "llama-server";
const MAX_COMPONENT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Hashes the canonical private llama.cpp runtime bundle beside `llama-server`.
///
/// The identity covers the launcher and every sibling `lib*.so*` entry. Logical
/// paths, lengths and file digests are length-delimited and ordered by path, so
/// a rebuilt implementation library necessarily changes the engine identity.
pub fn private_engine_build_digest(executable: impl AsRef<Path>) -> Result<EngineBuildDigest> {
    let executable = executable.as_ref();
    ensure!(
        executable.file_name().and_then(|name| name.to_str()) == Some(ENGINE_EXECUTABLE),
        "private engine executable must be named {ENGINE_EXECUTABLE}"
    );
    let directory = executable
        .parent()
        .context("private engine executable has no parent directory")?
        .canonicalize()
        .context("failed to resolve private engine bundle directory")?;

    let mut names = vec![ENGINE_EXECUTABLE.to_owned()];
    for entry in fs::read_dir(&directory).context("failed to inspect private engine bundle")? {
        let name = entry?
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("private engine component name is not UTF-8"))?;
        if name.starts_with("lib") && name.contains(".so") {
            names.push(name);
        }
    }
    names.sort();
    names.dedup();
    ensure!(
        names.len() > 1,
        "private engine bundle has no shared libraries"
    );

    let mut digest = Sha256::new();
    digest.update(ENGINE_BUILD_DOMAIN);
    digest.update((names.len() as u64).to_be_bytes());
    for name in names {
        hash_component(&mut digest, &directory, &name)?;
    }
    Ok(EngineBuildDigest::from_bytes(digest.finalize().into()))
}

fn hash_component(digest: &mut Sha256, directory: &Path, name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && !name.contains('/') && !name.contains('\\'),
        "private engine component name is not canonical"
    );
    let path = directory.join(name);
    let resolved = path
        .canonicalize()
        .with_context(|| format!("failed to resolve private engine component {name}"))?;
    ensure!(
        resolved.parent() == Some(directory),
        "private engine component escapes its bundle: {name}"
    );
    let metadata = fs::metadata(&resolved)
        .with_context(|| format!("failed to inspect private engine component {name}"))?;
    ensure!(
        metadata.is_file(),
        "private engine component is not a file: {name}"
    );
    ensure!(
        metadata.len() <= MAX_COMPONENT_BYTES,
        "private engine component is oversized: {name}"
    );

    let mut file = File::open(&resolved)
        .with_context(|| format!("failed to open private engine component {name}"))?;
    let mut component_digest = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    let mut bytes_read = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        bytes_read = bytes_read
            .checked_add(read as u64)
            .context("private engine component length overflow")?;
        ensure!(
            bytes_read <= MAX_COMPONENT_BYTES,
            "private engine component is oversized: {name}"
        );
        component_digest.update(&buffer[..read]);
    }
    ensure!(
        bytes_read == metadata.len(),
        "private engine component changed while hashing: {name}"
    );

    hash_field(digest, name.as_bytes());
    digest.update(bytes_read.to_be_bytes());
    digest.update(component_digest.finalize());
    Ok(())
}

fn hash_field(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "agl-engine-bundle-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::write(path.join(ENGINE_EXECUTABLE), b"launcher\n").unwrap();
            fs::write(path.join("libllama-server-impl.so"), b"implementation\n").unwrap();
            fs::write(path.join("libggml.so.1"), b"ggml\n").unwrap();
            Self(path)
        }

        fn executable(&self) -> PathBuf {
            self.0.join(ENGINE_EXECUTABLE)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn digest_covers_launcher_and_every_private_library() {
        let fixture = Fixture::new();
        let initial = private_engine_build_digest(fixture.executable()).unwrap();

        fs::write(
            fixture.0.join("libllama-server-impl.so"),
            b"changed implementation\n",
        )
        .unwrap();
        let implementation_changed = private_engine_build_digest(fixture.executable()).unwrap();
        assert_ne!(initial, implementation_changed);

        fs::write(fixture.executable(), b"changed launcher\n").unwrap();
        assert_ne!(
            implementation_changed,
            private_engine_build_digest(fixture.executable()).unwrap()
        );
    }

    #[test]
    fn digest_ignores_non_engine_siblings_and_requires_a_library() {
        let fixture = Fixture::new();
        let initial = private_engine_build_digest(fixture.executable()).unwrap();
        fs::write(fixture.0.join("runtime-manifest.json"), b"{}\n").unwrap();
        assert_eq!(
            initial,
            private_engine_build_digest(fixture.executable()).unwrap()
        );

        fs::remove_file(fixture.0.join("libllama-server-impl.so")).unwrap();
        fs::remove_file(fixture.0.join("libggml.so.1")).unwrap();
        assert!(private_engine_build_digest(fixture.executable()).is_err());
    }
}
