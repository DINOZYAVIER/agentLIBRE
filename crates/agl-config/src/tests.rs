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
kv_unified = true
mmap = false

[runtime.mtp]
enabled = false
draft_model = "/models/gemma4-mtp-q4_0.gguf"
draft_tokens = 4
p_min = 0.2
gpu_layers = 999
cache_type_k = "q8_0"
cache_type_v = "q8_0"

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
    assert_eq!(config.runtime.kv_unified, Some(true));
    assert_eq!(config.runtime.mmap, Some(false));
    assert!(!config.runtime.mtp.enabled);
    assert_eq!(
        config.runtime.mtp.draft_model.as_deref(),
        Some(Path::new("/models/gemma4-mtp-q4_0.gguf"))
    );
    assert_eq!(config.runtime.mtp.draft_tokens, 4);
    assert_eq!(
        config.runtime.mtp.p_min,
        MtpProbability::from_f32(0.2).unwrap()
    );
    assert_eq!(config.runtime.mtp.gpu_layers, Some(999));
    assert_eq!(config.runtime.mtp.cache_type_k, Some(KvCacheType::Q8_0));
    assert_eq!(config.runtime.mtp.cache_type_v, Some(KvCacheType::Q8_0));
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
    assert_eq!(config.runtime.kv_unified, None);
    assert_eq!(config.runtime.mtp, MtpRuntimeConfig::default());
    assert_eq!(config.prompt.system, SystemPrompt::BuiltinDefault);
    assert!(config.prompt.skills.is_empty());
}

#[test]
fn local_inference_config_accepts_enabled_mtp_runtime() {
    let path = write_temp_config(
        "local-inference-mtp-runtime",
        r#"
[backend]
kind = "llama_cpp"
model = "/models/gemma4.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[runtime.mtp]
enabled = true
draft_model = "/models/gemma4-mtp-q4_0.gguf"
draft_tokens = 4
p_min = 0.25

[model]
dialect = "gemma4"
tool_call_format = "gemma_function_call"
"#,
    );

    let config = load_local_inference_config(&path).unwrap();

    assert!(config.runtime.mtp.enabled);
    assert_eq!(
        config.runtime.mtp.draft_model.as_deref(),
        Some(Path::new("/models/gemma4-mtp-q4_0.gguf"))
    );
    assert_eq!(config.runtime.mtp.draft_tokens, 4);
    assert_eq!(
        config.runtime.mtp.p_min,
        MtpProbability::from_f32(0.25).unwrap()
    );
}

#[test]
fn local_inference_config_rejects_projector_with_mtp() {
    let path = write_temp_config(
        "local-inference-projector-mtp",
        r#"
[backend]
kind = "llama_cpp"
model = "/models/gemma4.gguf"
multimodal_projector = "/models/mmproj.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[runtime.mtp]
enabled = true
draft_model = "/models/gemma4-mtp-q4_0.gguf"
draft_tokens = 4

[model]
dialect = "gemma4"
tool_call_format = "gemma_function_call"
"#,
    );

    let error = load_local_inference_config(&path).unwrap_err();

    assert_error_contains(
        &error,
        "multimodal projector profiles cannot enable speculative MTP",
    );
}

#[test]
fn local_inference_config_rejects_enabled_mtp_without_draft_model() {
    let path = write_temp_config(
        "local-inference-mtp-missing-draft",
        r#"
[backend]
kind = "llama_cpp"
model = "/models/gemma4.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[runtime.mtp]
enabled = true
draft_tokens = 4

[model]
dialect = "gemma4"
tool_call_format = "gemma_function_call"
"#,
    );

    let err = load_local_inference_config(&path).unwrap_err();

    assert_error_contains(&err, "runtime.mtp enabled requires draft_model");
}

#[test]
fn local_inference_config_rejects_enabled_mtp_without_draft_tokens() {
    let path = write_temp_config(
        "local-inference-mtp-missing-draft-tokens",
        r#"
[backend]
kind = "llama_cpp"
model = "/models/gemma4.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[runtime.mtp]
enabled = true
draft_model = "/models/gemma4-mtp-q4_0.gguf"

[model]
dialect = "gemma4"
tool_call_format = "gemma_function_call"
"#,
    );

    let err = load_local_inference_config(&path).unwrap_err();

    assert_error_contains(&err, "runtime.mtp draft_tokens must be between 1 and 64");
}

#[test]
fn local_inference_config_rejects_mtp_draft_tokens_above_limit() {
    let path = write_temp_config(
        "local-inference-mtp-draft-token-limit",
        r#"
[backend]
kind = "llama_cpp"
model = "/models/gemma4.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[runtime.mtp]
enabled = false
draft_model = "/models/gemma4-mtp-q4_0.gguf"
draft_tokens = 65

[model]
dialect = "gemma4"
tool_call_format = "gemma_function_call"
"#,
    );

    let err = load_local_inference_config(&path).unwrap_err();

    assert_error_contains(&err, "runtime.mtp draft_tokens 65 exceeds maximum 64");
}

#[test]
fn local_inference_config_rejects_mtp_p_min_above_one() {
    let path = write_temp_config(
        "local-inference-mtp-p-min-limit",
        r#"
[backend]
kind = "llama_cpp"
model = "/models/gemma4.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[runtime.mtp]
enabled = false
draft_model = "/models/gemma4-mtp-q4_0.gguf"
draft_tokens = 4
p_min = 1.01

[model]
dialect = "gemma4"
tool_call_format = "gemma_function_call"
"#,
    );

    let err = load_local_inference_config(&path).unwrap_err();

    assert_error_contains(&err, "runtime.mtp p_min must be between 0.0 and 1.0");
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

#[test]
fn portable_preset_resolves_main_projector_and_draft_bindings() {
    let root = std::env::temp_dir().join(format!(
        "agl-config-model-bindings-{}-{}",
        std::process::id(),
        FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let model = root.join("model.gguf");
    let projector = root.join("mmproj.gguf");
    let draft = root.join("draft.gguf");
    for path in [&model, &projector, &draft] {
        std::fs::write(path, []).unwrap();
    }
    let bindings_path = root.join("models.toml");
    std::fs::write(
        &bindings_path,
        format!(
            "version = 1\n\n[models.main]\npath = {:?}\n\n[models.projector]\npath = {:?}\n\n[models.draft]\npath = {:?}\n",
            model, projector, draft
        ),
    )
    .unwrap();
    let vision_preset = load_inference_preset_from_str(
        "vision fixture",
        &preset_text("multimodal_projector_id = \"projector\"", ""),
    )
    .unwrap();
    let vision = resolve_inference_preset(vision_preset, &bindings_path).unwrap();
    assert_eq!(vision.backend.model, model);
    assert_eq!(
        vision.backend.multimodal_projector.as_deref(),
        Some(projector.as_path())
    );

    let mtp_preset = load_inference_preset_from_str(
        "MTP fixture",
        &preset_text(
            "",
            "[runtime.mtp]\nenabled = true\ndraft_model_id = \"draft\"\ndraft_tokens = 4",
        ),
    )
    .unwrap();
    let mtp = resolve_inference_preset(mtp_preset, &bindings_path).unwrap();
    assert_eq!(
        mtp.runtime.mtp.draft_model.as_deref(),
        Some(draft.as_path())
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn portable_preset_names_a_missing_model_binding() {
    let root = std::env::temp_dir().join(format!(
        "agl-config-missing-binding-{}-{}",
        std::process::id(),
        FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let bindings_path = root.join("models.toml");
    std::fs::write(&bindings_path, "version = 1\nmodels = {}\n").unwrap();
    let preset = load_inference_preset_from_str("fixture", &preset_text("", "")).unwrap();

    let error = resolve_inference_preset(preset, &bindings_path).unwrap_err();

    assert!(error.to_string().contains("model `main` is not configured"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn portable_preset_ignores_an_unrequired_missing_model_file() {
    let root = std::env::temp_dir().join(format!(
        "agl-config-unused-missing-binding-{}-{}",
        std::process::id(),
        FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let model = root.join("model.gguf");
    std::fs::write(&model, []).unwrap();
    let missing = root.join("unused.gguf");
    let bindings_path = root.join("models.toml");
    std::fs::write(
        &bindings_path,
        format!(
            "version = 1\n\n[models.main]\npath = {:?}\n\n[models.unused]\npath = {:?}\n",
            model, missing
        ),
    )
    .unwrap();
    let preset = load_inference_preset_from_str("fixture", &preset_text("", "")).unwrap();

    let resolved = resolve_inference_preset(preset, &bindings_path).unwrap();

    assert_eq!(resolved.backend.model, model);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn portable_preset_names_a_required_missing_model_file() {
    let root = std::env::temp_dir().join(format!(
        "agl-config-required-missing-model-{}-{}",
        std::process::id(),
        FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let missing = root.join("main.gguf");
    let bindings_path = root.join("models.toml");
    std::fs::write(
        &bindings_path,
        format!("version = 1\n\n[models.main]\npath = {:?}\n", missing),
    )
    .unwrap();
    let preset = load_inference_preset_from_str("fixture", &preset_text("", "")).unwrap();

    let error = resolve_inference_preset(preset, &bindings_path).unwrap_err();
    let diagnostic = format!("{error:#}");

    assert!(diagnostic.contains("model `main` file does not exist"));
    assert!(diagnostic.contains(&bindings_path.display().to_string()));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn model_bindings_reject_a_blank_path() {
    let root = std::env::temp_dir().join(format!(
        "agl-config-blank-model-binding-{}-{}",
        std::process::id(),
        FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let bindings_path = root.join("models.toml");
    std::fs::write(
        &bindings_path,
        "version = 1\n\n[models.main]\npath = \"   \"\n",
    )
    .unwrap();

    let error = load_model_bindings(&bindings_path).unwrap_err();

    assert!(format!("{error:#}").contains("model `main` path cannot be blank"));
    let _ = std::fs::remove_dir_all(root);
}

fn preset_text(backend_extra: &str, runtime_extra: &str) -> String {
    format!(
        r#"
[backend]
kind = "llama_cpp"
model_id = "main"
{backend_extra}

[runtime]
gpu_layers = 0
context_tokens = 4096
threads = 2
{runtime_extra}

[model]
dialect = "gemma4"
tool_call_format = "gemma_function_call"
"#
    )
}
