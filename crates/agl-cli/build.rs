use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let repo_root = manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .expect("agl-cli must live under crates/");
    let lib_dir = repo_root.join("target/llama-cpp/build/bin");

    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("vendor/llama.cpp").display()
    );
    println!(
        "cargo:rustc-link-arg-bin=agl=-Wl,-rpath,{}",
        lib_dir.display()
    );
}
