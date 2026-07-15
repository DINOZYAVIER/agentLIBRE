use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::*;

static FILE_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn write_temp_config(name: &str, content: &str) -> TempConfig {
    let id = FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "agl-config-{name}-{}-{id}.toml",
        std::process::id()
    ));
    std::fs::write(&path, content).unwrap();
    TempConfig(path)
}

struct TempConfig(PathBuf);

impl AsRef<Path> for TempConfig {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn assert_error_contains(error: &anyhow::Error, needle: &str) {
    let formatted = format!("{error:#}");
    assert!(
        formatted.contains(needle),
        "expected error to contain {needle:?}, got:\n{formatted}"
    );
}

#[test]
fn loads_local_inference_config_from_explicit_file() {
    let path = write_temp_config(
        "local-inference",
        r#"
[backend]
kind = "llama_cpp"
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

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
"#,
    );

    let config = load_local_inference_config(&path).unwrap();

    assert_eq!(config.backend.kind, BackendKind::LlamaCpp);
    assert_eq!(config.backend.kind.as_str(), "llama_cpp");
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
    assert_eq!(config.model.dialect, ModelDialect::Qwen3);
    assert_eq!(config.model.tool_call_format, ToolCallFormat::HermesJson);
    assert_eq!(config.prompt.system, SystemPrompt::BuiltinDefault);
    assert!(config.prompt.skills.is_empty());
}

#[test]
fn local_inference_config_accepts_minimal_runtime_fields() {
    let path = write_temp_config(
        "local-inference-minimal-runtime",
        r#"
[backend]
kind = "llama_cpp"
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
    assert_eq!(config.prompt.system, SystemPrompt::BuiltinDefault);
    assert!(config.prompt.skills.is_empty());
}

#[test]
fn local_inference_config_accepts_disabled_system_prompt() {
    let path = write_temp_config(
        "local-inference-system-prompt-none",
        r#"
[backend]
kind = "llama_cpp"
model = "/models/qwen3.6.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"

[prompt]
system = "none"
"#,
    );

    let config = load_local_inference_config(&path).unwrap();

    assert_eq!(config.prompt.system, SystemPrompt::None);
}

#[test]
fn local_inference_config_accepts_builtin_system_prompt() {
    let path = write_temp_config(
        "local-inference-system-prompt-builtin",
        r#"
[backend]
kind = "llama_cpp"
model = "/models/qwen3.6.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"

[prompt]
system = "builtin:default"
"#,
    );

    let config = load_local_inference_config(&path).unwrap();

    assert_eq!(config.prompt.system, SystemPrompt::BuiltinDefault);
}

#[test]
fn local_inference_config_accepts_prompt_skills() {
    let path = write_temp_config(
        "local-inference-prompt-skills",
        r#"
[backend]
kind = "llama_cpp"
model = "/models/qwen3.6.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"

[prompt]
skills = ["task-spec"]
"#,
    );

    let config = load_local_inference_config(&path).unwrap();

    assert_eq!(config.prompt.skills, vec!["task-spec"]);
}

#[test]
fn local_inference_config_rejects_invalid_prompt_skills() {
    let path = write_temp_config(
        "local-inference-prompt-skills-invalid",
        r#"
[backend]
kind = "llama_cpp"
model = "/models/qwen3.6.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"

[prompt]
skills = ["Bad Skill"]
"#,
    );

    let err = load_local_inference_config(&path).unwrap_err();

    assert_error_contains(&err, "prompt skill id is invalid");
}

#[test]
fn local_inference_config_rejects_unknown_system_prompt() {
    let path = write_temp_config(
        "local-inference-system-prompt-unknown",
        r#"
[backend]
kind = "llama_cpp"
model = "/models/qwen3.6.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"

[prompt]
system = "builtin:future"
"#,
    );

    let err = load_local_inference_config(&path).unwrap_err();

    assert_error_contains(&err, "failed to parse config file");
}

#[test]
fn local_inference_config_rejects_unknown_fields() {
    let path = write_temp_config(
        "local-inference-unknown",
        r#"
[backend]
kind = "llama_cpp"
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

    assert_error_contains(&err, "failed to parse config file");
}

#[test]
fn local_inference_config_rejects_backend_binary() {
    let path = write_temp_config(
        "local-inference-backend-binary",
        r#"
[backend]
kind = "llama_cpp"
model = "/models/qwen.gguf"
binary = "/usr/bin/llama-completion"

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

    assert_error_contains(&err, "failed to parse config file");
}

#[test]
fn local_inference_config_does_not_require_paths_to_exist() {
    let path = write_temp_config(
        "local-inference-paths",
        r#"
[backend]
kind = "llama_cpp"
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
        config.backend.model,
        PathBuf::from("/definitely/not/downloaded/qwen.gguf")
    );
}

#[test]
fn local_inference_config_rejects_invalid_numeric_limits() {
    let path = write_temp_config(
        "local-inference-limits",
        r#"
[backend]
kind = "llama_cpp"
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

    assert_error_contains(&err, "context_tokens 0 must be between 1 and 1048576");
}

#[test]
fn local_inference_config_rejects_invalid_batch_limits() {
    let path = write_temp_config(
        "local-inference-batch-limits",
        r#"
[backend]
kind = "llama_cpp"
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

    assert_error_contains(&err, "ubatch_size 512 cannot exceed batch_size 256");
}

#[test]
fn local_inference_config_rejects_ubatch_above_default_batch() {
    let path = write_temp_config(
        "local-inference-ubatch-default-batch",
        r#"
[backend]
kind = "llama_cpp"
model = "/models/qwen.gguf"

[runtime]
gpu_layers = 999
context_tokens = 256
threads = 8
ubatch_size = 512

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
"#,
    );

    let err = load_local_inference_config(&path).unwrap_err();

    assert_error_contains(&err, "ubatch_size 512 cannot exceed context_tokens 256");
}

#[test]
fn local_inference_config_rejects_empty_backend_paths() {
    let path = write_temp_config(
        "local-inference-empty-path",
        r#"
[backend]
kind = "llama_cpp"
model = ""

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

    assert_error_contains(&err, "backend model path cannot be empty");
}

#[test]
fn local_inference_config_rejects_whitespace_backend_paths() {
    let path = write_temp_config(
        "local-inference-whitespace-path",
        r#"
[backend]
kind = "llama_cpp"
model = "   "

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

    assert_error_contains(&err, "backend model path cannot be empty");
}

#[test]
fn local_inference_config_rejects_device_whitespace() {
    let path = write_temp_config(
        "local-inference-device-whitespace",
        r#"
[backend]
kind = "llama_cpp"
model = "/models/qwen.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8
device = " Vulkan0 "

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
"#,
    );

    let err = load_local_inference_config(&path).unwrap_err();

    assert_error_contains(
        &err,
        "runtime device cannot contain leading or trailing whitespace",
    );
}

#[test]
fn local_inference_config_rejects_unsupported_dialect_format_pair() {
    let path = write_temp_config(
        "local-inference-dialect-format",
        r#"
[backend]
kind = "llama_cpp"
model = "/models/qwen.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[model]
dialect = "gemma4"
tool_call_format = "hermes_json"
"#,
    );

    let err = load_local_inference_config(&path).unwrap_err();

    assert_error_contains(
        &err,
        "tool_call_format HermesJson is not supported for dialect Gemma4",
    );
}
