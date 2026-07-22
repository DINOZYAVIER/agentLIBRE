use std::path::{Path, PathBuf};
use std::process::Command;

/// Emits the runtime search paths required by a final executable that links
/// the shared llama.cpp libraries owned by this package.
pub fn emit_runtime_rpaths(native_bundle_directory: &Path) {
    let library_dir = PathBuf::from(crate::library_dir());
    println!("cargo:rustc-link-search=native={}", library_dir.display());
    // The build-tree directory is link-time input only. Production lookup is
    // constrained to the immutable native bundle beside the exact worker.
    println!(
        "cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/{}",
        native_bundle_directory.display()
    );

    if let Some(cxx_runtime_dir) = cxx_runtime_library_dir() {
        println!(
            "cargo:rustc-link-search=native={}",
            cxx_runtime_dir.display()
        );
        emit_rpath(&cxx_runtime_dir);
    }

    println!("cargo:rerun-if-env-changed=CXX");
}

fn emit_rpath(path: &std::path::Path) {
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", path.display());
}

fn cxx_runtime_library_dir() -> Option<PathBuf> {
    let compiler = std::env::var_os("CXX").unwrap_or_else(|| "c++".into());
    let output = Command::new(compiler)
        .arg("-print-file-name=libstdc++.so.6")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let path = String::from_utf8(output.stdout).ok()?;
    let path = PathBuf::from(path.trim());
    if path.is_file() {
        path.parent().map(PathBuf::from)
    } else {
        None
    }
}
