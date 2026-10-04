use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Component, PathBuf};

use agl_core::agent::{ToolFailure, ToolFailureKind};
use agl_tool_api::ToolContext;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

pub const READ: &str = "agentlibre.forge:artifact.read";
const MAX_READ_LINES: usize = 500;
const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    cursor: usize,
    limit_lines: usize,
}

pub(crate) fn execute(
    context: &ToolContext,
    input: Value,
    artifacts: &crate::ArtifactResolver,
) -> Result<(Value, Vec<agl_core::ImageRef>), ToolFailure> {
    let args: Args = serde_json::from_value(input).map_err(|_| invalid_input())?;
    let (entity_id, relative) = parse_virtual_path(&args.path).ok_or_else(invalid_input)?;
    let mut reader = artifacts(context.workspace.root.as_path(), &entity_id, &relative)?;
    let verification_status = reader.verification_status();
    let read_only_path = reader.read_only_path().map_err(|_| unavailable())?;
    let size = reader.seek(SeekFrom::End(0)).map_err(|_| unavailable())?;
    reader.seek(SeekFrom::Start(0)).map_err(|_| unavailable())?;
    let mut prefix = vec![0u8; size.min(96) as usize];
    reader.read_exact(&mut prefix).map_err(|_| unavailable())?;
    let mut image_format = agl_tool_api::image::detect_format(&prefix);
    if image_format.is_none() && size >= 26 {
        reader.seek(SeekFrom::End(-26)).map_err(|_| unavailable())?;
        let mut footer = [0u8; 26];
        reader.read_exact(&mut footer).map_err(|_| unavailable())?;
        if footer.ends_with(b"TRUEVISION-XFILE.\0") {
            image_format = Some(agl_tool_api::image::ImageFormat::Tga);
        }
    }
    reader.seek(SeekFrom::Start(0)).map_err(|_| unavailable())?;
    if let Some(format) = image_format {
        let mut hash = Sha256::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            if context.is_cancelled()
                || chrono::Utc::now().timestamp_millis() >= context.deadline_at_ms
            {
                return Err(unavailable());
            }
            let count = reader.read(&mut chunk).map_err(|_| unavailable())?;
            if count == 0 {
                break;
            }
            hash.update(&chunk[..count]);
        }
        let image = agl_core::ImageRef::new(
            agl_core::ImageSource::Forge {
                workspace: context.workspace.root.clone(),
                id: entity_id,
                relative: relative
                    .to_str()
                    .ok_or_else(invalid_input)?
                    .to_owned()
                    .try_into()
                    .map_err(|_| invalid_input())?,
            },
            agl_core::agent::PackageDigest::from_bytes(hash.finalize().into()),
        )
        .map_err(|_| invalid_input())?;
        let value = json!({"status":"image", "size":size, "digest":image.digest(),
            "path":args.path, "media_type":format.media_type(),
            "verification_status":verification_status.as_str()});
        return if fits(&value, context.result_bytes) {
            Ok((value, vec![image]))
        } else {
            Err(result_too_large())
        };
    }
    let bytes = read_bounded(reader)?;
    let content = match std::str::from_utf8(&bytes) {
        Ok(content) => content,
        Err(_) => {
            let value = json!({
                "status": "binary", "size": bytes.len(), "digest": digest(&bytes),
                "path": read_only_path, "verification_status": verification_status.as_str(),
            });
            return if fits(&value, context.result_bytes) {
                Ok((value, Vec::new()))
            } else {
                Err(result_too_large())
            };
        }
    };
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
            "verification_status": verification_status.as_str(),
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
            return Ok((value, Vec::new()));
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

fn read_bounded(mut reader: agl_tool_api::ForgeFile) -> Result<Vec<u8>, ToolFailure> {
    use std::io::{Seek, SeekFrom};
    let size = reader.seek(SeekFrom::End(0)).map_err(|_| unavailable())?;
    if size > MAX_FILE_BYTES as u64 {
        return Err(result_too_large());
    }
    reader.seek(SeekFrom::Start(0)).map_err(|_| unavailable())?;
    let mut bytes = Vec::new();
    reader
        .take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable())?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(result_too_large());
    }
    Ok(bytes)
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
    fn text_read_rejects_large_sparse_reader_before_allocating_contents() {
        let path = std::env::temp_dir().join(format!("agl-forge-bounded-{}", uuid::Uuid::now_v7()));
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(1 << 40).unwrap();
        drop(file);
        let reader = agl_tool_api::ForgeFile::new(
            std::fs::File::open(&path).unwrap(),
            Box::new(()),
            agl_tool_api::VerificationStatus::Unchecked,
        );
        assert!(read_bounded(reader).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
