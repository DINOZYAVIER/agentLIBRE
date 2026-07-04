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

    if llama_cpp_build::missing_required_library(&lib_dir).is_some()
        && llama_cpp_build::env_flag("AGL_LLAMA_CPP_AUTO_BUILD")
    {
        llama_cpp_build::run_llama_cpp_build(&repo_root);
    }
    if let Some(library) = llama_cpp_build::missing_required_library(&lib_dir) {
        panic!(
            "missing llama.cpp library {} in {}. Run scripts/build-llama-cpp.sh before building, or set AGL_LLAMA_CPP_AUTO_BUILD=1 to let the build script run it.",
            library,
            lib_dir.display()
        );
    }

    llama_cpp_build::emit_build_support_rerun(&repo_root);
    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("vendor/llama.cpp").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("scripts/build-llama-cpp.sh").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir
            .join("src/llama_cpp/chat_template_bridge.cpp")
            .display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir
            .join("src/llama_cpp/mtp_speculative_bridge.cpp")
            .display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("src/llama_cpp/abi_guard.cpp").display()
    );
    llama_cpp_build::emit_llama_cpp_env_reruns();

    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .file(manifest_dir.join("src/llama_cpp/chat_template_bridge.cpp"))
        .file(manifest_dir.join("src/llama_cpp/mtp_speculative_bridge.cpp"))
        .file(manifest_dir.join("src/llama_cpp/abi_guard.cpp"))
        .include(repo_root.join("vendor/llama.cpp/include"))
        .include(repo_root.join("vendor/llama.cpp/common"))
        .include(repo_root.join("vendor/llama.cpp/ggml/include"))
        .include(repo_root.join("vendor/llama.cpp/vendor"))
        .warnings(false)
        .compile("agl_llama_chat_template_bridge");

    println!(
        "cargo:rustc-env=AGL_LLAMA_CPP_LIBRARY_DIR={}",
        lib_dir.display()
    );
    llama_cpp_build::emit_link_search_and_rpaths(&lib_dir, LinkScope::AllTargets);
    println!("cargo:rustc-link-lib=dylib=llama-common");
    println!("cargo:rustc-link-lib=dylib=llama");
    println!("cargo:rustc-link-lib=dylib=ggml");
    println!("cargo:rustc-link-lib=dylib=ggml-base");
    println!("cargo:rustc-link-lib=dylib=ggml-cpu");
    println!("cargo:rustc-link-lib=dylib=ggml-vulkan");
}
