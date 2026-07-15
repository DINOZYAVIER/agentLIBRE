use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=CXX");
    if let Some(cxx_runtime_dir) = cxx_runtime_library_dir() {
        println!(
            "cargo:rustc-link-arg=-Wl,-rpath,{}",
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
