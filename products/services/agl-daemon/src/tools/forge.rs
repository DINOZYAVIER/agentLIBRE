use std::fs;
use std::path::{Component, Path, PathBuf};

use agl_core::agent::{ToolFailure, ToolFailureKind};
use agl_runtime::extension::ToolContext;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

pub const READ: &str = "agentlibre.builtins:forge_read";
const MAX_READ_LINES: usize = 500;
const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    cursor: usize,
    limit_lines: usize,
}

pub(crate) fn execute(context: &ToolContext, input: Value) -> Result<Value, ToolFailure> {
    let args: Args = serde_json::from_value(input).map_err(|_| invalid_input())?;
    let (entity_id, relative) = parse_virtual_path(&args.path).ok_or_else(invalid_input)?;
    let artifacts = agl_runtime::resolve_workspace_artifacts(context.workspace.root.as_path())
        .map_err(|_| unavailable())?;
    let root = if artifacts.memory.id == entity_id {
        artifacts.memory.materialized_path
    } else {
        artifacts
            .documents
            .iter()
            .find(|artifact| artifact.id == entity_id)
            .map(|artifact| artifact.materialized_path.clone())
            .ok_or_else(|| not_found(&args.path))?
    };
    let path = contained_file(&root, &relative).ok_or_else(|| not_found(&args.path))?;
    let bytes = fs::read(&path).map_err(|_| unavailable())?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(result_too_large());
    }
    let content = std::str::from_utf8(&bytes).map_err(|_| invalid_result())?;
    let cursor = args.cursor.max(1);
    let limit = args.limit_lines.clamp(1, MAX_READ_LINES);
    let all = content.lines().collect::<Vec<_>>();
    let mut lines = all
        .iter()
        .enumerate()
        .skip(cursor - 1)
        .take(limit)
        .map(|(index, line)| json!({"line": index + 1, "text": line}))
        .collect::<Vec<_>>();
    loop {
        let next = cursor
            .checked_add(lines.len())
            .filter(|value| *value <= all.len());
        let value = json!({
            "status": "ok",
            "path": args.path,
            "digest": digest(&bytes),
            "start_line": cursor,
            "end_line": cursor.saturating_add(lines.len()).saturating_sub(1),
            "total_lines": all.len(),
            "truncated": next.is_some(),
            "next_cursor": next,
            "lines": lines,
        });
        if fits(&value, context.result_bytes) {
            if next.is_some() && lines.is_empty() {
                return Err(result_too_large());
            }
            return Ok(value);
        }
        if lines.pop().is_none() {
            return Err(result_too_large());
        }
    }
}

fn parse_virtual_path(value: &str) -> Option<(String, PathBuf)> {
    let remainder = value.strip_prefix("forge:")?;
    let (id, path) = remainder.split_once('/')?;
    if id.is_empty()
        || path.is_empty()
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-')
        })
    {
        return None;
    }
    let path = PathBuf::from(path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some((id.to_owned(), path))
}

fn contained_file(root: &Path, relative: &Path) -> Option<PathBuf> {
    let root_metadata = fs::symlink_metadata(root).ok()?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return None;
    }
    let root = root.canonicalize().ok()?;
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    let canonical = path.canonicalize().ok()?;
    canonical.starts_with(&root).then_some(canonical)
}

fn fits(value: &Value, limit: u64) -> bool {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len() as u64 <= limit.min(agl_core::agent::MAX_TOOL_RESULT_BYTES))
        .unwrap_or(false)
}

fn digest(bytes: &[u8]) -> String {
    let mut value = String::from("sha256:");
    for byte in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        write!(&mut value, "{byte:02x}").expect("String formatting cannot fail");
    }
    value
}

fn invalid_input() -> ToolFailure {
    ToolFailure::no_effect(
        ToolFailureKind::InvalidInput,
        Some("path"),
        &["Use a verified Forge path such as forge:agentlibre.memory/INDEX.md."],
    )
}

fn invalid_result() -> ToolFailure {
    ToolFailure::no_effect(
        ToolFailureKind::InvalidResult,
        Some("content"),
        &["Request a UTF-8 text artifact."],
    )
}

fn result_too_large() -> ToolFailure {
    ToolFailure::no_effect(
        ToolFailureKind::ResultTooLarge,
        Some("result_bytes"),
        &["Reduce limit_lines and continue with next_cursor."],
    )
}

fn unavailable() -> ToolFailure {
    ToolFailure::no_effect(
        ToolFailureKind::Execution,
        Some("forge"),
        &["Verify the workspace agentLIBRE.toml and Forge lock."],
    )
}

fn not_found(_path: &str) -> ToolFailure {
    ToolFailure::no_effect(
        ToolFailureKind::InvalidInput,
        Some("path"),
        &["Use an artifact ID declared by the workspace and an existing relative file."],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_paths_require_a_declared_style_id_and_contained_relative_file() {
        assert_eq!(
            parse_virtual_path("forge:agentlibre.memory/INDEX.md"),
            Some(("agentlibre.memory".to_owned(), PathBuf::from("INDEX.md")))
        );
        assert!(parse_virtual_path("memory/INDEX.md").is_none());
        assert!(parse_virtual_path("forge:agentlibre.memory/../secret").is_none());
        assert!(parse_virtual_path("forge:Agentlibre.memory/INDEX.md").is_none());
        assert!(parse_virtual_path("forge:agentlibre.memory/").is_none());
    }

    #[test]
    fn contained_file_rejects_symlink_and_escape() {
        let root = std::env::temp_dir().join(format!("agl-forge-tool-{}", uuid::Uuid::now_v7()));
        let outside = root.with_file_name(format!("{}-outside", root.display()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("ok.md"), "ok\n").unwrap();
        std::fs::write(outside.join("secret.md"), "secret\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.md"), root.join("link.md")).unwrap();
        assert!(contained_file(&root, Path::new("ok.md")).is_some());
        assert!(contained_file(&root, Path::new("link.md")).is_none());
        assert!(contained_file(&root, Path::new("../secret.md")).is_none());
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }
}
