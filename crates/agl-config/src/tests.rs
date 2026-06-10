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
fn loads_turn_policy_config_from_explicit_file() {
    let path = write_temp_config(
        "turn-policy",
        r#"
[model]
dialect = "qwen3"
tool_call_format = "hermes_json"

[tools]
max_tool_calls = 4
require_visible_tool = true

[response]
reasoning = "strip"
boundary = "truncate"
max_final_answer_retries = 1
"#,
    );

    let config = load_turn_policy_config(&path).unwrap();

    assert_eq!(config.model.dialect, ModelDialect::Qwen3);
    assert_eq!(config.model.tool_call_format, ToolCallFormat::HermesJson);
    assert_eq!(config.tools.max_tool_calls, 4);
    assert!(config.tools.require_visible_tool);
    assert_eq!(config.response.reasoning, ReasoningPolicy::Strip);
    assert_eq!(config.response.boundary, BoundaryPolicy::Truncate);
    assert_eq!(config.response.max_final_answer_retries, 1);

    std::fs::remove_file(path).unwrap();
}

#[test]
fn empty_turn_policy_file_uses_safe_defaults() {
    let path = write_temp_config("empty-turn-policy", "");

    let config = load_turn_policy_config(&path).unwrap();

    assert_eq!(config.model, ModelConfig::default());
    assert_eq!(config.tools.max_tool_calls, 0);
    assert!(config.tools.require_visible_tool);
    assert_eq!(config.response.reasoning, ReasoningPolicy::Preserve);
    assert_eq!(config.response.boundary, BoundaryPolicy::Stop);

    std::fs::remove_file(path).unwrap();
}

#[test]
fn unknown_config_keys_are_rejected() {
    let path = write_temp_config(
        "unknown-key",
        r#"
[tools]
max_tool_calls = 1
surprise = true
"#,
    );

    let err = load_turn_policy_config(&path).unwrap_err();

    assert!(
        err.to_string().contains("failed to parse config file"),
        "unexpected error: {err}"
    );

    std::fs::remove_file(path).unwrap();
}

#[test]
fn validation_rejects_unbounded_tool_call_limits() {
    let path = write_temp_config(
        "tool-limit",
        r#"
[tools]
max_tool_calls = 65
"#,
    );

    let err = load_turn_policy_config(&path).unwrap_err();

    assert!(
        err.to_string()
            .contains("max_tool_calls 65 exceeds maximum 64"),
        "unexpected error: {err}"
    );

    std::fs::remove_file(path).unwrap();
}

#[test]
fn loads_model_config_from_explicit_file() {
    let path = write_temp_config(
        "model-format",
        r#"
dialect = "gemma4"
tool_call_format = "gemma4"
"#,
    );

    let config = load_model_config(&path).unwrap();

    assert_eq!(config.dialect, ModelDialect::Gemma4);
    assert_eq!(config.tool_call_format, ToolCallFormat::Gemma4);

    std::fs::remove_file(path).unwrap();
}
