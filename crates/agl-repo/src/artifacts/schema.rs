use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::validate_task_spec_markdown;

pub(super) fn validate_artifact_schema(
    workspace_root: &Path,
    root: &Path,
    schema: Option<&str>,
    strict: bool,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) {
    let Some(schema) = schema else {
        return;
    };
    let absolute_root = workspace_root.join(root);
    let mut schema_errors = Vec::new();
    match schema {
        "agl.task_spec.v1" | "agl.task_spec_legacy.v1" => {
            validate_task_spec_schema(workspace_root, &absolute_root, &mut schema_errors)
        }
        "agl.review_pack.v1" => {
            validate_review_pack_schema(workspace_root, &absolute_root, &mut schema_errors)
        }
        "agl.decision_doc.v1" | "agl.decision_doc_legacy.v1" => {
            validate_decision_doc_schema(workspace_root, &absolute_root, &mut schema_errors)
        }
        "agl.handoff_markdown.v1" => {
            validate_handoff_schema(workspace_root, &absolute_root, &mut schema_errors)
        }
        "agl.smoke.v1"
        | "agl.smoke_legacy.v1"
        | "agl.skill_source.v1"
        | "agl.skill_source_legacy.v1" => {}
        _ => warnings.push(format!("schema_validator_unknown: {schema}")),
    }
    if strict {
        errors.extend(schema_errors);
    } else {
        warnings.extend(schema_errors);
    }
}

fn validate_task_spec_schema(workspace_root: &Path, root: &Path, errors: &mut Vec<String>) {
    let mut files = Vec::new();
    if collect_files(root, &mut files).is_err() {
        return;
    }
    for file in files {
        if !file
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        {
            continue;
        }
        match fs::read_to_string(&file) {
            Ok(content) => {
                let validation = validate_task_spec_markdown(&content);
                if !validation.is_valid() {
                    errors.push(format!(
                        "schema_invalid: {} missing_sections={}",
                        display_relative(workspace_root, &file).display(),
                        validation.missing_sections.join("|")
                    ));
                }
            }
            Err(err) => errors.push(format!(
                "schema_read_failed: {}: {err}",
                display_relative(workspace_root, &file).display()
            )),
        }
    }
}

fn validate_review_pack_schema(workspace_root: &Path, root: &Path, errors: &mut Vec<String>) {
    let mut files = Vec::new();
    if collect_files(root, &mut files).is_err() {
        return;
    }
    for file in files {
        if file.file_name().and_then(|name| name.to_str()) == Some("review-manifest.json") {
            validate_json_file(workspace_root, &file, errors);
        }
    }
}

fn validate_decision_doc_schema(workspace_root: &Path, root: &Path, errors: &mut Vec<String>) {
    let mut files = Vec::new();
    if collect_files(root, &mut files).is_err() {
        return;
    }
    for file in files {
        if file
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        {
            validate_json_file(workspace_root, &file, errors);
        }
    }
}

fn validate_handoff_schema(workspace_root: &Path, root: &Path, errors: &mut Vec<String>) {
    let mut files = Vec::new();
    if collect_files(root, &mut files).is_err() {
        return;
    }
    for file in files {
        if !file
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        {
            continue;
        }
        match fs::read_to_string(&file) {
            Ok(content) if content.trim().is_empty() => errors.push(format!(
                "schema_invalid: {} empty_handoff",
                display_relative(workspace_root, &file).display()
            )),
            Ok(_) => {}
            Err(err) => errors.push(format!(
                "schema_read_failed: {}: {err}",
                display_relative(workspace_root, &file).display()
            )),
        }
    }
}

fn validate_json_file(workspace_root: &Path, file: &Path, errors: &mut Vec<String>) {
    match fs::read_to_string(file) {
        Ok(content) => {
            if let Err(err) = serde_json::from_str::<serde_json::Value>(&content) {
                errors.push(format!(
                    "schema_invalid: {} json_parse_failed: {err}",
                    display_relative(workspace_root, file).display()
                ));
            }
        }
        Err(err) => errors.push(format!(
            "schema_read_failed: {}: {err}",
            display_relative(workspace_root, file).display()
        )),
    }
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_name() == ".git" {
            continue;
        }
        if entry.file_type()?.is_dir() {
            collect_files(&path, files)?;
        } else if entry.file_type()?.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn display_relative(workspace_root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(workspace_root)
        .map(PathBuf::from)
        .unwrap_or_else(|_| path.to_path_buf())
}
