use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by Cargo"),
    );
    let repo_root = manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .expect("agl-daemon must live under crates/");
    let build_dir = env::var_os("AGL_LLAMA_CPP_BUILD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root.join("target/llama-cpp/build"));
    let lib_dir = build_dir.join("bin");

    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("vendor/llama.cpp").display()
    );
    println!("cargo:rerun-if-env-changed=AGL_LLAMA_CPP_BUILD_DIR");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
}
