use std::path::Path;

use crate::loader::parse_function_document;
use crate::*;
#[test]
fn resolves_builtin_gemma4_function_with_embedded_config() {
    let root = std::env::temp_dir().join(format!(
        "agl-functions-builtin-gemma4-{}",
        std::process::id()
    ));
    let workspace = root.join("workspace");
    let config = root.join("config");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&workspace).unwrap();

    let locator = resolve_function_reference("gemma4-12b", &workspace, &config).unwrap();
    assert_eq!(locator.source, FunctionSource::Builtin);

    let loaded = load_function(locator).unwrap();
    assert_eq!(loaded.front_matter.id, "gemma4-12b");
    assert_eq!(
        loaded.inference_config_path.as_deref(),
        Some(Path::new("assets/functions/gemma4-12b/inference.toml"))
    );
    assert!(
        loaded
            .inference_config_toml
            .as_deref()
            .unwrap()
            .contains("tool_call_format = \"gemma_function_call\"")
    );

    let runtime = resolve_runtime_function("gemma4-12b", &workspace, &config).unwrap();
    assert_eq!(runtime.source, FunctionSource::Builtin);
    assert_eq!(runtime.model_profile, None);
    assert_eq!(runtime.profile_path, None);
    assert!(runtime.inference_config_toml.is_some());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn lists_builtin_functions() {
    let root =
        std::env::temp_dir().join(format!("agl-functions-list-builtin-{}", std::process::id()));
    let workspace = root.join("workspace");
    let config = root.join("config");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&workspace).unwrap();

    let functions = list_functions(&workspace, &config).unwrap();

    assert!(functions.iter().any(|function| {
        function.id == "gemma4-12b" && function.source == FunctionSource::Builtin && function.valid
    }));
    assert!(functions.iter().any(|function| {
        function.id == "gemma4-26b" && function.source == FunctionSource::Builtin && function.valid
    }));
    assert!(functions.iter().any(|function| {
        function.id == "gemma4-31b" && function.source == FunctionSource::Builtin && function.valid
    }));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rejects_function_body_in_manifest() {
    let root = std::env::temp_dir().join(format!("agl-functions-body-{}", std::process::id()));
    let function_root = root.join("coding");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&function_root).unwrap();
    std::fs::write(
        function_root.join(FUNCTION_SYSTEM_PROMPT_FILE_NAME),
        "Code.\n",
    )
    .unwrap();
    std::fs::write(
        function_root.join(FUNCTION_FILE_NAME),
        r#"---
schema: agentfunction/v1
id: coding
title: Coding
---

# Mission

Code.
"#,
    )
    .unwrap();
    let locator = FunctionLocator {
        reference: "coding".to_string(),
        source: FunctionSource::Workspace,
        path: function_root.join(FUNCTION_FILE_NAME),
        root_dir: function_root,
    };

    let err = load_function(locator).unwrap_err();

    assert!(
        err.to_string()
            .contains("FUNCTION.md body is not supported")
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rejects_prompt_field_in_manifest() {
    let content = r#"---
schema: agentfunction/v1
id: coding
title: Coding
prompt:
  system: SYSTEM.md
---
"#;

    let (front_matter, _) = parse_function_document(content).unwrap();
    let err = front_matter.validate().unwrap_err();

    assert!(
        err.to_string()
            .contains("unknown function front matter field `prompt`")
    );
}

#[test]
fn rejects_missing_system_prompt_file() {
    let root = std::env::temp_dir().join(format!("agl-functions-system-{}", std::process::id()));
    let function_root = root.join("coding");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&function_root).unwrap();
    std::fs::write(
        function_root.join(FUNCTION_FILE_NAME),
        r#"---
schema: agentfunction/v1
id: coding
title: Coding
---
"#,
    )
    .unwrap();
    let locator = FunctionLocator {
        reference: "coding".to_string(),
        source: FunctionSource::Workspace,
        path: function_root.join(FUNCTION_FILE_NAME),
        root_dir: function_root,
    };

    let err = load_function(locator).unwrap_err();

    assert!(
        err.to_string()
            .contains("function system prompt file not found")
    );
    let _ = std::fs::remove_dir_all(&root);
}
