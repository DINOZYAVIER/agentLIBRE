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
        .expect("agl-inference must live under crates/");
    let build_dir = env::var_os("AGL_LLAMA_CPP_BUILD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root.join("target/llama-cpp/build"));
    let lib_dir = build_dir.join("bin");

    if missing_required_libraries(&lib_dir).is_some() {
        let script = repo_root.join("scripts/build-llama-cpp.sh");
        let status = Command::new(&script)
            .status()
            .expect("failed to run scripts/build-llama-cpp.sh");
        assert!(status.success(), "scripts/build-llama-cpp.sh failed");
    }
    if let Some(library) = missing_required_libraries(&lib_dir) {
        panic!(
            "missing llama.cpp library {} after build in {}",
            library,
            lib_dir.display()
        );
    }

    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("vendor/llama.cpp").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        repo_root.join("scripts/build-llama-cpp.sh").display()
    );
    for env_name in [
        "AGL_LLAMA_CPP_SOURCE_DIR",
        "AGL_LLAMA_CPP_BUILD_DIR",
        "AGL_LLAMA_CPP_BUILD_JOBS",
        "AGL_LLAMA_CPP_VULKAN_INCLUDE_DIR",
        "AGL_LLAMA_CPP_VULKAN_LIBRARY",
        "AGL_LLAMA_CPP_VULKAN_GLSLC",
        "AGL_LLAMA_CPP_VULKAN_GLSLANG_VALIDATOR",
        "AGL_LLAMA_CPP_SPIRV_INCLUDE_DIR",
    ] {
        println!("cargo:rerun-if-env-changed={env_name}");
    }
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

fn missing_required_libraries(lib_dir: &std::path::Path) -> Option<&'static str> {
    [
        "libllama.so",
        "libggml.so",
        "libggml-base.so",
        "libggml-cpu.so",
        "libggml-vulkan.so",
    ]
    .into_iter()
    .find(|library| !lib_dir.join(library).is_file())
}
