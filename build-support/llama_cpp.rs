use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const REQUIRED_LIBRARIES: [&str; 7] = [
    "libllama-common.so",
    "libmtmd.so",
    "libllama.so",
    "libggml.so",
    "libggml-base.so",
    "libggml-cpu.so",
    "libggml-vulkan.so",
];

pub fn cargo_manifest_dir() -> PathBuf {
    PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by Cargo"),
    )
}

#[allow(dead_code)]
pub fn repo_root_from_cargo_manifest_dir() -> PathBuf {
    repo_root(&cargo_manifest_dir())
}

pub fn repo_root(manifest_dir: &Path) -> PathBuf {
    manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .expect("crate must live under workspace crates/")
        .to_path_buf()
}

pub fn build_dir(repo_root: &Path) -> PathBuf {
    env::var_os("AGL_LLAMA_CPP_BUILD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root.join("target/llama-cpp/build"))
}

pub fn lib_dir(repo_root: &Path) -> PathBuf {
    build_dir(repo_root).join("bin")
}

#[allow(dead_code)]
pub fn missing_required_library(lib_dir: &Path) -> Option<&'static str> {
    REQUIRED_LIBRARIES
        .into_iter()
        .find(|library| !lib_dir.join(library).is_file())
}

#[allow(dead_code)]
pub fn env_flag(name: &str) -> bool {
    matches!(
        env::var(name).ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

pub fn emit_build_support_rerun(repo_root: &Path) {
    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("build-support/llama_cpp.rs").display()
    );
}

#[allow(dead_code)]
pub fn emit_llama_cpp_link_reruns(repo_root: &Path) {
    emit_build_support_rerun(repo_root);
    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("vendor/llama.cpp").display()
    );
    println!("cargo:rerun-if-env-changed=AGL_LLAMA_CPP_BUILD_DIR");
    println!("cargo:rerun-if-env-changed=CXX");
}

#[allow(dead_code)]
pub fn emit_llama_cpp_env_reruns() {
    for env_name in [
        "AGL_LLAMA_CPP_AUTO_BUILD",
        "AGL_LLAMA_CPP_SOURCE_DIR",
        "AGL_LLAMA_CPP_BUILD_DIR",
        "AGL_LLAMA_CPP_BUILD_JOBS",
        "AGL_LLAMA_CPP_VULKAN_INCLUDE_DIR",
        "AGL_LLAMA_CPP_VULKAN_LIBRARY",
        "AGL_LLAMA_CPP_VULKAN_GLSLC",
        "AGL_LLAMA_CPP_VULKAN_GLSLANG_VALIDATOR",
        "AGL_LLAMA_CPP_SPIRV_INCLUDE_DIR",
        "CXX",
    ] {
        println!("cargo:rerun-if-env-changed={env_name}");
    }
}

pub fn emit_link_search_and_rpaths(lib_dir: &Path) {
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    emit_rpath(lib_dir);
    if let Some(cxx_runtime_dir) = cxx_runtime_library_dir() {
        println!(
            "cargo:rustc-link-search=native={}",
            cxx_runtime_dir.display()
        );
        emit_rpath(&cxx_runtime_dir);
    }
}

#[allow(dead_code)]
pub fn emit_link_search_and_rpaths_from_env() {
    let repo_root = repo_root_from_cargo_manifest_dir();
    let lib_dir = lib_dir(&repo_root);

    emit_llama_cpp_link_reruns(&repo_root);
    emit_link_search_and_rpaths(&lib_dir);
}

#[allow(dead_code)]
pub fn run_llama_cpp_build(repo_root: &Path) {
    let script = repo_root.join("scripts/build-llama-cpp.sh");
    let status = Command::new(&script)
        .status()
        .expect("failed to run scripts/build-llama-cpp.sh");
    assert!(status.success(), "scripts/build-llama-cpp.sh failed");
}

fn emit_rpath(path: &Path) {
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", path.display());
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
