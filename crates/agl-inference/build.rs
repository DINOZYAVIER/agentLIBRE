use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let repo_root = manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .expect("agl-inference must live under crates/");
    let lib_dir = repo_root.join("target/llama-cpp/build/bin");
    let libllama = lib_dir.join("libllama.so");

    if !libllama.exists() {
        let script = repo_root.join("scripts/build-llama-cpp.sh");
        let status = Command::new(&script)
            .status()
            .expect("failed to run scripts/build-llama-cpp.sh");
        assert!(status.success(), "scripts/build-llama-cpp.sh failed");
    }

    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("vendor/llama.cpp").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("scripts/build-llama-cpp.sh").display()
    );
    println!(
        "cargo:rustc-env=AGL_LLAMA_CPP_LIBRARY_DIR={}",
        lib_dir.display()
    );
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=llama");
    println!("cargo:rustc-link-lib=dylib=ggml");
    println!("cargo:rustc-link-lib=dylib=ggml-base");
    println!("cargo:rustc-link-lib=dylib=ggml-cpu");
    println!("cargo:rustc-link-lib=dylib=ggml-vulkan");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
}
