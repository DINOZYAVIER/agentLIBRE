use std::fs::{self, File};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest as _, Sha256};

const REQUIRED_NATIVE_LIBRARIES: [&str; 5] = [
    "libllama-common.so.0",
    "libmtmd.so.0",
    "libllama.so.0",
    "libggml.so.0",
    "libggml-base.so.0",
];
const MAX_NATIVE_BUNDLE_FILES: usize = 64;
const NATIVE_BUNDLE_ID_DOMAIN: &[u8] = b"agl-inference-native-bundle-v1\0";
const NATIVE_BUNDLE_BASE_DIRECTORY: &str = "agl-inference-native";

fn main() {
    let entries = selected_native_libraries(Path::new(agl_llama_cpp_sys::library_dir()));
    let identity = native_bundle_identity(&entries);
    let relative_directory = PathBuf::from(NATIVE_BUNDLE_BASE_DIRECTORY)
        .join(format!("sha256-{}", lowercase_hex(&identity)));
    agl_llama_cpp_sys::build_support::emit_runtime_rpaths(&relative_directory);
    write_native_bundle_manifest(&entries, &identity);
    stage_native_bundle(&entries, &relative_directory);
    println!(
        "cargo:rustc-env=AGL_INFERENCE_NATIVE_RELATIVE_DIR={}",
        relative_directory.display()
    );
}

fn stage_native_bundle(entries: &[NativeBundleEntry], relative_directory: &Path) {
    let profile_dir = profile_output_directory();
    let deps_dir = profile_dir.join("deps");
    fs::create_dir_all(&deps_dir).expect("create Cargo dependency output directory");
    for destination in [
        profile_dir.join(relative_directory),
        deps_dir.join(relative_directory),
    ] {
        stage_native_bundle_at(entries, &destination);
    }
}

fn stage_native_bundle_at(entries: &[NativeBundleEntry], destination: &Path) {
    let bundle_base = destination
        .parent()
        .expect("native bundle destination has a parent directory");
    match fs::symlink_metadata(bundle_base) {
        Ok(_) => validate_bundle_base_identity(bundle_base),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(bundle_base).unwrap_or_else(|error| {
                panic!(
                    "failed to create native bundle base {}: {error}",
                    bundle_base.display()
                )
            });
            validate_bundle_base_identity(bundle_base);
        }
        Err(error) => panic!(
            "failed to inspect native bundle base {}: {error}",
            bundle_base.display()
        ),
    }
    fs::set_permissions(bundle_base, fs::Permissions::from_mode(0o755)).unwrap_or_else(|error| {
        panic!(
            "failed to make native bundle base owner-writable {}: {error}",
            bundle_base.display()
        )
    });
    validate_bundle_base(bundle_base);
    let bundle_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .expect("content-addressed native bundle has a UTF-8 filename");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after the Unix epoch")
        .as_nanos();
    let staging = bundle_base.join(format!(
        ".{bundle_name}.staging-{}-{nonce}",
        std::process::id()
    ));

    fs::create_dir(&staging).unwrap_or_else(|error| {
        panic!(
            "failed to create native bundle staging directory {}: {error}",
            staging.display()
        )
    });
    for entry in entries {
        let staged_file = staging.join(&entry.name);
        fs::copy(&entry.source, &staged_file).unwrap_or_else(|error| {
            panic!(
                "failed to copy native bundle input {} to {}: {error}",
                entry.source.display(),
                staged_file.display()
            )
        });
        fs::set_permissions(&staged_file, fs::Permissions::from_mode(0o555))
            .expect("set native bundle file mode");
    }
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o555))
        .expect("set native bundle directory mode");

    if destination.exists() {
        validate_staged_bundle(entries, destination);
    } else if let Err(error) = fs::rename(&staging, destination) {
        if destination.exists() {
            validate_staged_bundle(entries, destination);
        } else {
            panic!(
                "failed to publish native bundle {}: {error}",
                destination.display()
            );
        }
    }
    if staging.exists() {
        make_tree_writable(&staging);
        let _ = fs::remove_dir_all(&staging);
    }
}

fn validate_staged_bundle(entries: &[NativeBundleEntry], directory: &Path) {
    let metadata = fs::symlink_metadata(directory).unwrap_or_else(|error| {
        panic!(
            "failed to inspect content-addressed native bundle {}: {error}",
            directory.display()
        )
    });
    assert!(
        !metadata.file_type().is_symlink() && metadata.is_dir(),
        "content-addressed native bundle is not an exact directory: {}",
        directory.display()
    );
    assert_eq!(
        metadata.permissions().mode() & 0o7777,
        0o555,
        "content-addressed native bundle directory has an unsafe mode: {}",
        directory.display()
    );
    assert_trusted_owner(directory, &metadata);
    let identity = native_bundle_identity(entries);
    let expected_name = format!("sha256-{}", lowercase_hex(&identity));
    assert_eq!(
        directory.file_name().and_then(|name| name.to_str()),
        Some(expected_name.as_str()),
        "content-addressed native bundle directory does not match its manifest"
    );
    let actual = fs::read_dir(directory)
        .unwrap_or_else(|error| {
            panic!(
                "failed to inspect existing content-addressed native bundle {}: {error}",
                directory.display()
            )
        })
        .map(|entry| {
            entry
                .expect("read existing content-addressed native bundle entry")
                .file_name()
        })
        .collect::<std::collections::BTreeSet<_>>();
    let expected = entries
        .iter()
        .map(|entry| entry.name.as_str().into())
        .collect::<std::collections::BTreeSet<std::ffi::OsString>>();
    assert_eq!(
        actual,
        expected,
        "content-addressed native bundle contains unexpected files: {}",
        directory.display()
    );
    for entry in entries {
        let path = directory.join(&entry.name);
        let metadata = fs::symlink_metadata(&path).unwrap_or_else(|error| {
            panic!(
                "failed to inspect content-addressed native bundle file {}: {error}",
                path.display()
            )
        });
        assert!(metadata.is_file() && !metadata.file_type().is_symlink());
        assert_eq!(
            metadata.nlink(),
            1,
            "content-addressed native bundle file has multiple links: {}",
            path.display()
        );
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o555);
        assert_trusted_owner(&path, &metadata);
        assert_eq!(
            sha256_file(&path),
            entry.sha256,
            "content-addressed native bundle digest mismatch: {}",
            path.display()
        );
    }
}

fn validate_bundle_base(bundle_base: &Path) {
    validate_bundle_base_identity(bundle_base);
    let metadata = fs::symlink_metadata(bundle_base).unwrap_or_else(|error| {
        panic!(
            "failed to inspect native bundle base {}: {error}",
            bundle_base.display()
        )
    });
    assert_eq!(
        metadata.permissions().mode() & 0o7777,
        0o755,
        "native bundle base has an unsafe mode: {}",
        bundle_base.display()
    );
}

fn validate_bundle_base_identity(bundle_base: &Path) {
    let metadata = fs::symlink_metadata(bundle_base).unwrap_or_else(|error| {
        panic!(
            "failed to inspect native bundle base {}: {error}",
            bundle_base.display()
        )
    });
    assert!(
        !metadata.file_type().is_symlink() && metadata.is_dir(),
        "native bundle base is not an exact directory: {}",
        bundle_base.display()
    );
    assert_trusted_owner(bundle_base, &metadata);
}

fn assert_trusted_owner(path: &Path, metadata: &fs::Metadata) {
    let effective_uid = unsafe { libc::geteuid() };
    assert!(
        metadata.uid() == effective_uid || metadata.uid() == 0,
        "native bundle path has an unexpected owner: {}",
        path.display()
    );
}

struct NativeBundleEntry {
    name: String,
    source: PathBuf,
    sha256: [u8; 32],
}

fn selected_native_libraries(source_dir: &Path) -> Vec<NativeBundleEntry> {
    let mut names = REQUIRED_NATIVE_LIBRARIES
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut plugins = fs::read_dir(source_dir)
        .unwrap_or_else(|error| {
            panic!(
                "failed to list llama.cpp library directory {}: {error}",
                source_dir.display()
            )
        })
        .map(|entry| entry.expect("read llama.cpp library directory entry"))
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            ((name.starts_with("libggml-cpu-") && name.ends_with(".so"))
                || name == "libggml-vulkan.so")
                .then_some(name)
        })
        .collect::<Vec<_>>();
    plugins.sort();
    plugins.dedup();
    assert!(
        plugins.iter().any(|name| name.starts_with("libggml-cpu-")),
        "native inference bundle requires at least one CPU backend plugin"
    );
    names.extend(plugins);
    names.sort();
    assert!(
        names.len() <= MAX_NATIVE_BUNDLE_FILES,
        "native inference bundle exceeds its file-count bound"
    );
    names
        .into_iter()
        .map(|name| {
            let source = source_dir.join(&name);
            println!("cargo:rerun-if-changed={}", source.display());
            assert!(
                fs::metadata(&source).is_ok_and(|metadata| metadata.is_file()),
                "native inference bundle input {} is not a regular file",
                source.display()
            );
            let source = fs::canonicalize(&source).unwrap_or_else(|error| {
                panic!(
                    "failed to resolve native bundle input {}: {error}",
                    source.display()
                )
            });
            NativeBundleEntry {
                name,
                sha256: sha256_file(&source),
                source,
            }
        })
        .collect()
}

fn sha256_file(path: &Path) -> [u8; 32] {
    let mut file = File::open(path).unwrap_or_else(|error| {
        panic!(
            "failed to open native bundle input {}: {error}",
            path.display()
        )
    });
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer).unwrap_or_else(|error| {
            panic!(
                "failed to hash native bundle input {}: {error}",
                path.display()
            )
        });
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    digest.finalize().into()
}

fn write_native_bundle_manifest(entries: &[NativeBundleEntry], identity: &[u8; 32]) {
    let output = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo must provide OUT_DIR"));
    let path = output.join("native_bundle_manifest.rs");
    let mut file = File::create(&path).unwrap_or_else(|error| {
        panic!(
            "failed to create native bundle manifest {}: {error}",
            path.display()
        )
    });
    writeln!(
        file,
        "pub(crate) const EXPECTED_NATIVE_BUNDLE: &[(&str, [u8; 32])] = &["
    )
    .expect("write native bundle manifest header");
    for entry in entries {
        let bytes = entry
            .sha256
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(file, "    ({:?}, [{}]),", entry.name, bytes)
            .expect("write native bundle manifest entry");
    }
    writeln!(file, "];").expect("write native bundle manifest footer");
    writeln!(
        file,
        "pub(crate) const EXPECTED_NATIVE_BUNDLE_ID: &str = \"sha256:{}\";",
        lowercase_hex(identity)
    )
    .expect("write native bundle identity");
}

fn native_bundle_identity(entries: &[NativeBundleEntry]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(NATIVE_BUNDLE_ID_DOMAIN);
    for entry in entries {
        hash_framed(&mut digest, entry.name.as_bytes());
        hash_framed(&mut digest, &entry.sha256);
    }
    digest.finalize().into()
}

fn hash_framed(digest: &mut Sha256, value: &[u8]) {
    digest.update(
        u64::try_from(value.len())
            .expect("native bundle identity input exceeds u64")
            .to_be_bytes(),
    );
    digest.update(value);
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = Vec::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[usize::from(byte >> 4)]);
        encoded.push(DIGITS[usize::from(byte & 0x0f)]);
    }
    String::from_utf8(encoded).expect("native bundle hexadecimal identity is UTF-8")
}

fn profile_output_directory() -> PathBuf {
    let output = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo must provide OUT_DIR"));
    output
        .ancestors()
        .nth(3)
        .expect("worker OUT_DIR must be below a Cargo profile directory")
        .to_path_buf()
}

fn make_tree_writable(path: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.is_dir() {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o755));
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    make_tree_writable(&entry.path());
                }
            }
        } else {
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o644));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "agl-native-bundle-build-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).expect("create native bundle build fixture");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            make_tree_writable(&self.0);
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entry(source: PathBuf, name: &str) -> NativeBundleEntry {
        NativeBundleEntry {
            sha256: sha256_file(&source),
            source,
            name: name.to_owned(),
        }
    }

    fn destination(base: &Path, entries: &[NativeBundleEntry]) -> PathBuf {
        base.join(format!(
            "sha256-{}",
            lowercase_hex(&native_bundle_identity(entries))
        ))
    }

    #[test]
    fn distinct_build_manifests_publish_without_replacing_each_other() {
        let fixture = TestDirectory::new();
        let base = fixture.0.join(NATIVE_BUNDLE_BASE_DIRECTORY);
        let first_source = fixture.0.join("first.so");
        let second_source = fixture.0.join("second.so");
        fs::write(&first_source, b"first immutable build variant")
            .expect("write first build variant");
        fs::write(&second_source, b"second immutable build variant")
            .expect("write second build variant");
        let first = vec![entry(first_source, "libggml-cpu-fixture.so")];
        let second = vec![entry(second_source, "libggml-cpu-fixture.so")];
        let first_destination = destination(&base, &first);
        let second_destination = destination(&base, &second);
        assert_ne!(first_destination, second_destination);

        stage_native_bundle_at(&first, &first_destination);
        stage_native_bundle_at(&second, &second_destination);
        validate_staged_bundle(&first, &first_destination);
        validate_staged_bundle(&second, &second_destination);

        assert!(first_destination.is_dir());
        assert!(second_destination.is_dir());
        assert_eq!(
            fs::read(first_destination.join("libggml-cpu-fixture.so"))
                .expect("read first published build variant"),
            b"first immutable build variant"
        );
        assert_eq!(
            fs::read(second_destination.join("libggml-cpu-fixture.so"))
                .expect("read second published build variant"),
            b"second immutable build variant"
        );
    }
}
