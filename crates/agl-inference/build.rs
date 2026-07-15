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
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir
            .join("src/llama_cpp/chat_template_bridge.cpp")
            .display()
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
        "CXX",
    ] {
        println!("cargo:rerun-if-env-changed={env_name}");
    }

    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .file(manifest_dir.join("src/llama_cpp/chat_template_bridge.cpp"))
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
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    if let Some(cxx_runtime_dir) = cxx_runtime_library_dir() {
        println!(
            "cargo:rustc-link-search=native={}",
            cxx_runtime_dir.display()
        );
        println!(
            "cargo:rustc-link-arg=-Wl,-rpath,{}",
            cxx_runtime_dir.display()
        );
    }
    println!("cargo:rustc-link-lib=dylib=llama-common");
    println!("cargo:rustc-link-lib=dylib=llama");
    println!("cargo:rustc-link-lib=dylib=ggml");
    println!("cargo:rustc-link-lib=dylib=ggml-base");
    println!("cargo:rustc-link-lib=dylib=ggml-cpu");
    println!("cargo:rustc-link-lib=dylib=ggml-vulkan");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir.display());
}

fn missing_required_libraries(lib_dir: &std::path::Path) -> Option<&'static str> {
    [
        "libllama-common.so",
        "libllama.so",
        "libggml.so",
        "libggml-base.so",
        "libggml-cpu.so",
        "libggml-vulkan.so",
    ]
    .into_iter()
    .find(|library| !lib_dir.join(library).is_file())
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
