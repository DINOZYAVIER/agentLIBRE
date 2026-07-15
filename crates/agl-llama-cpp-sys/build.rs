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

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR must be set by Cargo"),
    );
    let repo_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("crate must live under workspace crates/");
    let build_dir = env::var_os("AGL_LLAMA_CPP_BUILD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root.join("target/llama-cpp/build"));
    let lib_dir = build_dir.join("bin");

    if missing_required_library(&lib_dir).is_some() && env_flag("AGL_LLAMA_CPP_AUTO_BUILD") {
        run_llama_cpp_build(repo_root);
    }
    if let Some(library) = missing_required_library(&lib_dir) {
        panic!(
            "missing llama.cpp library {library} in {}. Run scripts/build-llama-cpp.sh before building, or set AGL_LLAMA_CPP_AUTO_BUILD=1 to let the build script run it.",
            lib_dir.display()
        );
    }

    for path in [
        repo_root.join("vendor/llama.cpp"),
        repo_root.join("scripts/build-llama-cpp.sh"),
        manifest_dir.join("src/native/chat_template_bridge.cpp"),
        manifest_dir.join("src/native/mtp_speculative_bridge.cpp"),
        manifest_dir.join("src/native/mtmd_bridge.cpp"),
        manifest_dir.join("src/native/abi_guard.cpp"),
    ] {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    for name in [
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
        println!("cargo:rerun-if-env-changed={name}");
    }

    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .file(manifest_dir.join("src/native/chat_template_bridge.cpp"))
        .file(manifest_dir.join("src/native/mtp_speculative_bridge.cpp"))
        .file(manifest_dir.join("src/native/mtmd_bridge.cpp"))
        .file(manifest_dir.join("src/native/abi_guard.cpp"))
        .include(repo_root.join("vendor/llama.cpp/include"))
        .include(repo_root.join("vendor/llama.cpp/common"))
        .include(repo_root.join("vendor/llama.cpp/ggml/include"))
        .include(repo_root.join("vendor/llama.cpp/vendor"))
        .include(repo_root.join("vendor/llama.cpp/tools/mtmd"))
        .warnings(false)
        .compile("agl_llama_cpp_bridge");

    println!(
        "cargo:rustc-env=AGL_LLAMA_CPP_LIBRARY_DIR={}",
        lib_dir.display()
    );
    println!("cargo:metadata=library_dir={}", lib_dir.display());
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    for library in [
        "llama-common",
        "mtmd",
        "llama",
        "ggml",
        "ggml-base",
        "ggml-cpu",
        "ggml-vulkan",
    ] {
        println!("cargo:rustc-link-lib=dylib={library}");
    }
}

fn missing_required_library(lib_dir: &Path) -> Option<&'static str> {
    REQUIRED_LIBRARIES
        .into_iter()
        .find(|library| !lib_dir.join(library).is_file())
}

fn env_flag(name: &str) -> bool {
    matches!(
        env::var(name).ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

fn run_llama_cpp_build(repo_root: &Path) {
    let script = repo_root.join("scripts/build-llama-cpp.sh");
    let status = Command::new(&script)
        .status()
        .expect("failed to run scripts/build-llama-cpp.sh");
    assert!(status.success(), "scripts/build-llama-cpp.sh failed");
}
