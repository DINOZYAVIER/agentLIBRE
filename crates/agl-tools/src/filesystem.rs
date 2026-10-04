use std::fs;
use std::path::{Component, Path, PathBuf};

use agl_core::EffectId;
use agl_core::agent::{EffectReceipt, ToolFailure, ToolFailureKind};
use agl_tool_api::{ServiceGit, ToolContext};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::{WorkspaceMutation, execution_failure as execution, invalid_result};

pub const READ: &str = "agentlibre.files:file.read";
pub const APPLY_PATCH: &str = "agentlibre.files:file.patch";
pub const WRITE_EFFECT: &str = "agentlibre.files:file.write";

const MAX_READ_LINES: usize = 500;
const MAX_READ_BYTES: usize = 8 * 1024 * 1024;

pub(crate) fn execute(
    id: &str,
    context: &ToolContext,
    input: Value,
    mutations: &WorkspaceMutation,
    git: &ServiceGit,
) -> Result<(Value, Vec<EffectReceipt>), ToolFailure> {
    let workspace = Workspace::new(context)?;
    let write_receipt = if id == APPLY_PATCH {
        Some(workspace.authorize_write(context)?)
    } else {
        None
    };
    let value = match id {
        READ => workspace.read(input, context.result_bytes)?,
        APPLY_PATCH => {
            let workspace_mutation = mutations(&workspace.root);
            let _guard = workspace_mutation.lock().map_err(|_| execution())?;
            crate::patch::apply(&workspace.root, context, input, git)?
        }
        _ => return Err(execution()),
    };
    let receipts = write_receipt.into_iter().collect();
    Ok((value, receipts))
}

struct Workspace {
    root: PathBuf,
    cwd: PathBuf,
}

impl Workspace {
    fn new(context: &ToolContext) -> Result<Self, ToolFailure> {
        let root = context
            .workspace
            .root
            .as_path()
            .canonicalize()
            .map_err(|_| execution())?;
        let cwd = root
            .join(context.workspace.working_directory.as_path())
            .canonicalize()
            .map_err(|_| execution())?;
        if !root.is_dir() || !cwd.is_dir() || !cwd.starts_with(&root) {
            return Err(execution());
        }
        Ok(Self { root, cwd })
    }

    fn authorize_write(&self, context: &ToolContext) -> Result<EffectReceipt, ToolFailure> {
        let effect = EffectId::new(WRITE_EFFECT).expect("static Effect ID");
        let grant = context
            .authority
            .0
            .iter()
            .find(|grant| grant.effect == effect)
            .ok_or_else(unauthorized)?;
        let declared_root = grant
            .scope
            .as_value()
            .as_object()
            .and_then(|scope| scope.get("root"))
            .and_then(Value::as_str)
            .map(Path::new)
            .filter(|path| path.is_absolute())
            .and_then(|path| path.canonicalize().ok())
            .ok_or_else(unauthorized)?;
        if declared_root != self.root {
            return Err(unauthorized());
        }
        Ok(EffectReceipt {
            effect,
            scope: grant.scope.clone(),
        })
    }

    fn read(&self, input: Value, result_bytes: u64) -> Result<Value, ToolFailure> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Args {
            path: String,
            cursor: usize,
            limit_lines: usize,
        }
        let args: Args = decode(input)?;
        let Some(path) = self.existing_path(&args.path)? else {
            return Ok(not_found(&args.path));
        };
        if !path.is_file() {
            return Err(invalid_input());
        }
        let bytes = fs::read(&path).map_err(|_| execution())?;
        if bytes.len() > MAX_READ_BYTES {
            return Err(invalid_result());
        }
        let content = std::str::from_utf8(&bytes).map_err(|_| invalid_result())?;
        let offset = args.cursor.clamp(1, usize::MAX);
        let limit = args.limit_lines.clamp(1, MAX_READ_LINES);
        let all = content.lines().collect::<Vec<_>>();
        let mut lines = all
            .iter()
            .enumerate()
            .skip(offset - 1)
            .take(limit)
            .map(|(index, line)| json!({"line":index + 1,"text":line}))
            .collect::<Vec<_>>();
        loop {
            let next = offset
                .checked_add(lines.len())
                .filter(|next| *next <= all.len());
            let value = json!({
                "path": self.display(&path), "digest": digest(&bytes),
                "start_line": offset, "end_line": offset.saturating_add(lines.len()).saturating_sub(1),
                "total_lines": all.len(), "truncated": next.is_some(),
                "next_cursor": next, "lines": lines
            });
            if value_fits(&value, result_bytes)? {
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

    fn existing_path(&self, raw: &str) -> Result<Option<PathBuf>, ToolFailure> {
        let path = self.join(raw)?;
        if let Some(parent) = path
            .parent()
            .filter(|parent| parent.starts_with(&self.root))
        {
            reject_existing_symlinks(&self.root, parent)?;
        }
        match fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(execution()),
        }
        reject_symlinks(&self.root, &path)?;
        let canonical = path.canonicalize().map_err(|_| execution())?;
        if !canonical.starts_with(&self.root) {
            return Err(invalid_input());
        }
        Ok(Some(canonical))
    }

    fn join(&self, raw: &str) -> Result<PathBuf, ToolFailure> {
        let path = Path::new(raw);
        if path.is_absolute()
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
        {
            return Err(invalid_input());
        }
        let joined = self.cwd.join(path);
        if !joined.starts_with(&self.root) {
            return Err(invalid_input());
        }
        Ok(joined)
    }

    fn display(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }
}

fn decode<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, ToolFailure> {
    serde_json::from_value(value).map_err(|_| invalid_input())
}

fn value_fits(value: &Value, limit: u64) -> Result<bool, ToolFailure> {
    let text = serde_json::to_string(value).map_err(|_| invalid_result())?;
    let content = agl_core::Content::text(text).map_err(|_| invalid_result())?;
    let result = agl_core::agent::ToolResult {
        content,
        effect_receipts: Vec::new(),
    };
    let bytes = serde_json::to_vec(&result).map_err(|_| invalid_result())?;
    Ok((bytes.len() as u64) <= limit.min(agl_core::agent::MAX_TOOL_RESULT_BYTES))
}

fn invalid_input() -> ToolFailure {
    ToolFailure::no_effect(
        ToolFailureKind::InvalidInput,
        Some("input"),
        &["Inspect the input schema and use workspace-relative paths."],
    )
}

fn result_too_large() -> ToolFailure {
    ToolFailure::no_effect(
        ToolFailureKind::ResultTooLarge,
        Some("result_bytes"),
        &["Narrow the requested path or range and reduce the result limit."],
    )
}

fn not_found(path: &str) -> Value {
    json!({"status":"error","error":{"kind":"not_found","path":path}})
}

fn unauthorized() -> ToolFailure {
    ToolFailure::no_effect(
        ToolFailureKind::Unauthorized,
        Some("path"),
        &["Use an admitted workspace-relative path and only granted filesystem operations."],
    )
}

fn digest(bytes: &[u8]) -> String {
    let value = Sha256::digest(bytes);
    let mut encoded = String::with_capacity("sha256:".len() + value.len() * 2);
    encoded.push_str("sha256:");
    for byte in value {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}

fn reject_symlinks(root: &Path, target: &Path) -> Result<(), ToolFailure> {
    let relative = target.strip_prefix(root).map_err(|_| invalid_input())?;
    let mut cursor = root.to_path_buf();
    for part in relative.components() {
        if let Component::Normal(part) = part {
            cursor.push(part);
            if fs::symlink_metadata(&cursor)
                .map_err(|_| invalid_input())?
                .file_type()
                .is_symlink()
            {
                return Err(invalid_input());
            }
        }
    }
    Ok(())
}

fn reject_existing_symlinks(root: &Path, target: &Path) -> Result<(), ToolFailure> {
    let relative = target.strip_prefix(root).map_err(|_| invalid_input())?;
    let mut cursor = root.to_path_buf();
    for part in relative.components() {
        if let Component::Normal(part) = part {
            cursor.push(part);
            match fs::symlink_metadata(&cursor) {
                Ok(metadata) if metadata.file_type().is_symlink() => return Err(invalid_input()),
                Ok(metadata) if !metadata.is_dir() => return Err(invalid_input()),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(_) => return Err(execution()),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agl_core::AgentRunId;
    use agl_core::agent::{
        AbsolutePath, AgentOperationKey, AuthorityGrantSet, RelativePath, WorkspaceScope,
    };
    use agl_tool_api::{ToolCancellation, ToolContext};
    use std::num::NonZeroU32;

    #[test]
    fn read_is_contained() {
        let root = std::env::temp_dir().join(format!("agl-fs-{}", AgentRunId::generate()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "before\n").unwrap();
        let workspace = Workspace::new(&context(&root, AuthorityGrantSet::default())).unwrap();
        let read = workspace
            .read(json!({"path":"a.txt","cursor":1,"limit_lines":200}), 65_536)
            .unwrap();
        assert_eq!(read["digest"], digest(b"before\n"));
        assert!(
            workspace
                .read(
                    json!({"path":"../outside","cursor":1,"limit_lines":200}),
                    65_536,
                )
                .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn read_requires_cursor_and_missing_paths_are_observations() {
        let root = std::env::temp_dir().join(format!("agl-fs-read-{}", AgentRunId::generate()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Cargo.toml"), "[workspace]\nresolver = \"3\"\n").unwrap();
        let workspace = Workspace::new(&context(&root, AuthorityGrantSet::default())).unwrap();

        assert!(
            workspace
                .read(json!({"path":"Cargo.toml"}), 65_536)
                .is_err()
        );
        assert_eq!(
            workspace
                .read(
                    json!({"path":"absent","cursor":1,"limit_lines":200}),
                    65_536,
                )
                .unwrap(),
            json!({"status":"error","error":{"kind":"not_found","path":"absent"}})
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn read_never_follows_workspace_symlinks() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!("agl-fs-link-{}", AgentRunId::generate()));
        let outside = std::env::temp_dir().join(format!("agl-fs-out-{}", AgentRunId::generate()));
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret"), "secret").unwrap();
        symlink(&outside, root.join("link")).unwrap();
        let workspace = Workspace::new(&context(&root, AuthorityGrantSet::default())).unwrap();
        assert!(
            workspace
                .read(
                    json!({"path":"link/secret","cursor":1,"limit_lines":200}),
                    65_536,
                )
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }

    pub(crate) fn context(root: &Path, authority: AuthorityGrantSet) -> ToolContext {
        ToolContext::new(
            AgentOperationKey {
                run_id: AgentRunId::generate(),
                ordinal: NonZeroU32::MIN,
            },
            None,
            WorkspaceScope {
                root: AbsolutePath::try_from(root.to_string_lossy().into_owned()).unwrap(),
                working_directory: RelativePath::try_from(".".to_owned()).unwrap(),
            },
            authority,
            i64::MAX,
            65_536,
            ToolCancellation::new(),
        )
    }
}
