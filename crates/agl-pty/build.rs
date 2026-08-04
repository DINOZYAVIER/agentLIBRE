use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

const BUILD_ID_DOMAIN: &[u8] = b"agl-pty-launcher-build-identity-v1\0";
const BUILD_INPUTS: [&str; 8] = [
    "Cargo.toml",
    "Cargo.lock",
    "crates/agl-exec/Cargo.toml",
    "crates/agl-exec/src",
    "crates/agl-pty/Cargo.toml",
    "crates/agl-pty/build.rs",
    "crates/agl-pty/src",
    "crates/agl-process/Cargo.toml",
];
const BUILD_ENV_INPUTS: [&str; 6] = [
    "TARGET",
    "PROFILE",
    "CARGO_CFG_TARGET_ARCH",
    "CARGO_CFG_TARGET_OS",
    "CARGO_CFG_TARGET_ENV",
    "CARGO_CFG_TARGET_FEATURE",
];

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR")
            .expect("Cargo must provide CARGO_MANIFEST_DIR to agl-pty/build.rs"),
    );
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("agl-pty must remain below the workspace crates directory");
    let mut inputs = Vec::new();

    for relative in BUILD_INPUTS {
        let path = workspace_root.join(relative);
        println!("cargo:rerun-if-changed={}", path.display());
        collect_regular_files(workspace_root, &path, &mut inputs);
    }
    inputs.sort_by(|left, right| left.0.cmp(&right.0));

    let mut environment = BUILD_ENV_INPUTS
        .into_iter()
        .map(|name| {
            println!("cargo:rerun-if-env-changed={name}");
            (name.to_owned(), std::env::var(name).unwrap_or_default())
        })
        .collect::<Vec<_>>();
    environment.extend(
        std::env::vars()
            .filter(|(name, _)| name.starts_with("CARGO_FEATURE_"))
            .inspect(|(name, _)| println!("cargo:rerun-if-env-changed={name}")),
    );
    environment.sort();

    let mut digest = Sha256::new();
    digest.update(BUILD_ID_DOMAIN);
    for (relative, path) in inputs {
        let contents = fs::read(&path).unwrap_or_else(|error| {
            panic!("failed to read build input {}: {error}", path.display())
        });
        hash_framed(&mut digest, relative.as_bytes());
        hash_framed(&mut digest, &contents);
    }
    for (name, value) in environment {
        hash_framed(&mut digest, name.as_bytes());
        hash_framed(&mut digest, value.as_bytes());
    }

    let build_id = lowercase_hex(&digest.finalize());
    println!("cargo:rustc-env=AGL_PROCESS_BUILD_ID=sha256:{build_id}");
}

fn collect_regular_files(root: &Path, path: &Path, inputs: &mut Vec<(String, PathBuf)>) {
    let metadata = fs::symlink_metadata(path).unwrap_or_else(|error| {
        panic!("failed to inspect build input {}: {error}", path.display())
    });
    if metadata.file_type().is_symlink() {
        panic!(
            "agl-pty build identity input must not be a symlink: {}",
            path.display()
        );
    }
    if metadata.is_file() {
        let relative = path
            .strip_prefix(root)
            .expect("build identity input must remain below the package root")
            .components()
            .map(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .expect("build identity input paths must be UTF-8")
            })
            .collect::<Vec<_>>()
            .join("/");
        inputs.push((relative, path.to_owned()));
        return;
    }
    if !metadata.is_dir() {
        panic!(
            "agl-pty build identity input must be a regular file or directory: {}",
            path.display()
        );
    }

    let mut children = fs::read_dir(path)
        .unwrap_or_else(|error| panic!("failed to list build input {}: {error}", path.display()))
        .map(|entry| {
            entry
                .unwrap_or_else(|error| {
                    panic!(
                        "failed to read build input below {}: {error}",
                        path.display()
                    )
                })
                .path()
        })
        .collect::<Vec<_>>();
    children.sort();
    for child in children {
        collect_regular_files(root, &child, inputs);
    }
}

fn hash_framed(digest: &mut Sha256, value: &[u8]) {
    let length = u64::try_from(value.len()).expect("build identity input exceeds u64::MAX bytes");
    digest.update(length.to_be_bytes());
    digest.update(value);
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    let mut encoded = Vec::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(DIGITS[usize::from(byte >> 4)]);
        encoded.push(DIGITS[usize::from(byte & 0x0f)]);
    }
    String::from_utf8(encoded).expect("hexadecimal build identity must be UTF-8")
}
