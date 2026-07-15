use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by Cargo"),
    );
    let repo_root = manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .expect("agl-cli must live under crates/");
    let build_dir = env::var_os("AGL_LLAMA_CPP_BUILD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root.join("target/llama-cpp/build"));
    let lib_dir = build_dir.join("bin");

    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("vendor/llama.cpp").display()
    );
    println!("cargo:rerun-if-env-changed=AGL_LLAMA_CPP_BUILD_DIR");
    println!("cargo:rerun-if-env-changed=CXX");
    println!(
        "cargo:rustc-link-arg-bin=agl=-Wl,-rpath,{}",
        lib_dir.display()
    );
    if let Some(cxx_runtime_dir) = cxx_runtime_library_dir() {
        println!(
            "cargo:rustc-link-arg-bin=agl=-Wl,-rpath,{}",
            cxx_runtime_dir.display()
        );
    }
}

fn cxx_runtime_library_dir() -> Option<PathBuf> {
    let compiler = env::var_os("CXX").unwrap_or_else(|| "c++".into());
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
