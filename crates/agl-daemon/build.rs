use std::env;
use std::path::PathBuf;

#[path = "../../build-support/llama_cpp.rs"]
mod llama_cpp_build;

use llama_cpp_build::LinkScope;

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by Cargo"),
    );
    let repo_root = llama_cpp_build::repo_root(&manifest_dir);
    let lib_dir = llama_cpp_build::lib_dir(&repo_root);

    llama_cpp_build::emit_build_support_rerun(&repo_root);
    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("vendor/llama.cpp").display()
    );
    println!("cargo:rerun-if-env-changed=AGL_LLAMA_CPP_BUILD_DIR");
    println!("cargo:rerun-if-env-changed=CXX");
    llama_cpp_build::emit_link_search_and_rpaths(&lib_dir, LinkScope::AllTargets);
}
