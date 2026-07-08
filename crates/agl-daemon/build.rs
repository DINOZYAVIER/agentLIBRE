#[path = "../../build-support/llama_cpp.rs"]
mod llama_cpp_build;

fn main() {
    llama_cpp_build::emit_link_search_and_rpaths_from_env();
}
