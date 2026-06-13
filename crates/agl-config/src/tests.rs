use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::*;

static FILE_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn write_temp_config(name: &str, content: &str) -> PathBuf {
    let id = FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "agl-config-{name}-{}-{id}.toml",
        std::process::id()
    ));
    std::fs::write(&path, content).unwrap();
    path
}

#[test]
fn loads_model_config_from_explicit_file() {
    let path = write_temp_config(
        "model-format",
        r#"
dialect = "gemma4"
tool_call_format = "gemma_function_call"
"#,
    );

    let config = load_model_config(&path).unwrap();

    assert_eq!(config.dialect, ModelDialect::Gemma4);
    assert_eq!(config.tool_call_format, ToolCallFormat::GemmaFunctionCall);

    std::fs::remove_file(path).unwrap();
}

#[test]
fn loads_local_inference_config_from_explicit_file() {
    let path = write_temp_config(
        "local-inference",
        r#"
[backend]
kind = "llama_cpp"
binary = "/opt/llama.cpp/build/bin/llama-cli"
model = "/models/qwen3.6.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8
device = "Vulkan0"
batch_size = 1024
ubatch_size = 256
flash_attention = "on"
cache_type_k = "q8_0"
cache_type_v = "q8_0"
mmap = false
jinja = true
conversation = false
simple_io = true
display_prompt = false

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
"#,
    );

    let config = load_local_inference_config(&path).unwrap();

    assert_eq!(config.backend.kind, BackendKind::LlamaCpp);
    assert_eq!(
        config.backend.binary,
        PathBuf::from("/opt/llama.cpp/build/bin/llama-cli")
    );
    assert_eq!(config.backend.model, PathBuf::from("/models/qwen3.6.gguf"));
    assert_eq!(config.runtime.gpu_layers, 999);
    assert_eq!(config.runtime.context_tokens, 32768);
    assert_eq!(config.runtime.threads, 8);
    assert_eq!(config.runtime.device.as_deref(), Some("Vulkan0"));
    assert_eq!(config.runtime.batch_size, Some(1024));
    assert_eq!(config.runtime.ubatch_size, Some(256));
    assert_eq!(config.runtime.flash_attention, Some(RuntimeSwitch::On));
    assert_eq!(config.runtime.cache_type_k, Some(KvCacheType::Q8_0));
    assert_eq!(config.runtime.cache_type_v, Some(KvCacheType::Q8_0));
    assert_eq!(config.runtime.mmap, Some(false));
    assert_eq!(config.runtime.jinja, Some(true));
    assert_eq!(config.runtime.conversation, Some(false));
    assert!(config.runtime.simple_io);
    assert_eq!(config.runtime.display_prompt, Some(false));
    assert_eq!(config.model.dialect, ModelDialect::Qwen3);
    assert_eq!(config.model.tool_call_format, ToolCallFormat::HermesJson);

    std::fs::remove_file(path).unwrap();
}

#[test]
fn local_inference_config_accepts_legacy_minimal_runtime_fields() {
    let path = write_temp_config(
        "local-inference-minimal-runtime",
        r#"
[backend]
kind = "llama_cpp"
binary = "/opt/llama.cpp/build/bin/llama-completion"
model = "/models/qwen3.6.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
"#,
    );

    let config = load_local_inference_config(&path).unwrap();

    assert_eq!(config.runtime.device, None);
    assert_eq!(config.runtime.batch_size, None);
    assert_eq!(config.runtime.ubatch_size, None);
    assert_eq!(config.runtime.flash_attention, None);
    assert_eq!(config.runtime.cache_type_k, None);
    assert!(!config.runtime.simple_io);

    std::fs::remove_file(path).unwrap();
}

#[test]
fn local_inference_config_rejects_unknown_fields() {
    let path = write_temp_config(
        "local-inference-unknown",
        r#"
[backend]
kind = "llama_cpp"
binary = "/bin/llama-cli"
model = "/models/qwen.gguf"
surprise = true

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
"#,
    );

    let err = load_local_inference_config(&path).unwrap_err();

    assert!(
        err.to_string().contains("failed to parse config file"),
        "unexpected error: {err}"
    );

    std::fs::remove_file(path).unwrap();
}

#[test]
fn local_inference_config_does_not_require_paths_to_exist() {
    let path = write_temp_config(
        "local-inference-paths",
        r#"
[backend]
kind = "llama_cpp"
binary = "/definitely/not/installed/llama-cli"
model = "/definitely/not/downloaded/qwen.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
"#,
    );

    let config = load_local_inference_config(&path).unwrap();

    assert_eq!(
        config.backend.binary,
        PathBuf::from("/definitely/not/installed/llama-cli")
    );
    assert_eq!(
        config.backend.model,
        PathBuf::from("/definitely/not/downloaded/qwen.gguf")
    );

    std::fs::remove_file(path).unwrap();
}

#[test]
fn local_inference_config_rejects_invalid_numeric_limits() {
    let path = write_temp_config(
        "local-inference-limits",
        r#"
[backend]
kind = "llama_cpp"
binary = "/bin/llama-cli"
model = "/models/qwen.gguf"

[runtime]
gpu_layers = 999
context_tokens = 0
threads = 8

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
"#,
    );

    let err = load_local_inference_config(&path).unwrap_err();

    assert!(
        err.to_string()
            .contains("context_tokens 0 must be between 1 and 1048576"),
        "unexpected error: {err}"
    );

    std::fs::remove_file(path).unwrap();
}

#[test]
fn local_inference_config_rejects_invalid_batch_limits() {
    let path = write_temp_config(
        "local-inference-batch-limits",
        r#"
[backend]
kind = "llama_cpp"
binary = "/bin/llama-cli"
model = "/models/qwen.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8
batch_size = 256
ubatch_size = 512

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
"#,
    );

    let err = load_local_inference_config(&path).unwrap_err();

    assert!(
        err.to_string()
            .contains("ubatch_size 512 cannot exceed batch_size 256"),
        "unexpected error: {err}"
    );

    std::fs::remove_file(path).unwrap();
}

#[test]
fn local_inference_config_rejects_empty_backend_paths() {
    let path = write_temp_config(
        "local-inference-empty-path",
        r#"
[backend]
kind = "llama_cpp"
binary = ""
model = "/models/qwen.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
"#,
    );

    let err = load_local_inference_config(&path).unwrap_err();

    assert!(
        err.to_string()
            .contains("backend binary path cannot be empty"),
        "unexpected error: {err}"
    );

    std::fs::remove_file(path).unwrap();
}
