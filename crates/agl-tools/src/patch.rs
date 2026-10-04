//! `agentlibre.files:file.patch`: apply a text diff with a host-launched,
//! isolated `git apply`.
//!
//! Flow: validate the input, determine and validate the affected paths, run
//! the non-mutating `git apply --check`, then run the mutating `git apply`.
//! There is no staging, backup, rollback or retry. A mutating apply that does
//! not provably complete is reported as an unknown outcome.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agl_core::agent::{ToolFailure, ToolFailureKind};
use agl_core::{CanonicalJson, agent::AgentOperationKey};
use agl_tool_api::{ServiceGit, ServiceGitOutput, ServiceGitRequest, ToolContext};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const MAX_PATCH_INPUT_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_PATCH_PATHS: usize = 64;
const MAX_DETAIL_BYTES: usize = 2048;
const MAX_TIMEOUT_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    patch: String,
}

pub(crate) fn apply(
    root: &Path,
    context: &ToolContext,
    input: Value,
    git: &ServiceGit,
) -> Result<Value, ToolFailure> {
    let args: Args = serde_json::from_value(input).map_err(|_| invalid_patch(None))?;
    if args.patch.trim().is_empty() || args.patch.len() > MAX_PATCH_INPUT_BYTES {
        return Err(invalid_patch(Some(
            "patch is empty or larger than 8 MiB".to_owned(),
        )));
    }
    let header_paths = validate_headers(&args.patch)?;
    let private = PrivateGit::create(&context.operation)?;
    private.init(git, context)?;
    let env = private.env();
    let run = |arguments: &[&str]| -> Result<ServiceGitOutput, ToolFailure> {
        check_running(context)?;
        let timeout_ms = context
            .deadline_at_ms
            .saturating_sub(now_ms())
            .min(MAX_TIMEOUT_MS);
        if timeout_ms <= 0 {
            return Err(no_change(ToolFailureKind::Deadline, None));
        }
        git(ServiceGitRequest {
            args: arguments.iter().map(OsString::from).collect(),
            cwd: root.to_owned(),
            env: env.clone(),
            stdin: args.patch.as_bytes().to_vec(),
            timeout: Duration::from_millis(timeout_ms as u64),
            deadline_at_ms: context.deadline_at_ms,
            cancellation: context.cancellation(),
        })
        .map_err(|_| no_change(ToolFailureKind::Execution, None))
    };

    let numstat = run(&["apply", "--numstat", "-z", "-"])?;
    let numstat = non_mutating_success(numstat)?;
    let paths = affected_paths(&numstat.stdout, header_paths)?;
    for path in &paths {
        validate_path(root, path)?;
    }
    if context.is_cancelled() {
        return Err(no_change(ToolFailureKind::Cancelled, None));
    }

    let check = run(&["apply", "--check", "-"])?;
    non_mutating_success(check)?;
    if context.is_cancelled() {
        return Err(no_change(ToolFailureKind::Cancelled, None));
    }

    // `run` fails only before the process starts (no deadline left or spawn
    // failure), which proves no effect. After it starts, only exit status 0
    // without a deadline kill proves completion.
    let applied = run(&["apply", "-"])?;
    if applied.timed_out || applied.cancelled || applied.exit_code != Some(0) {
        tracing::error!(
            operation = ?context.operation,
            exit_code = ?applied.exit_code,
            timed_out = applied.timed_out,
            "git apply did not provably complete after a successful check"
        );
        return Err(outcome_unknown());
    }
    Ok(json!({"status":"committed","paths":paths}))
}

/// A check or listing run changed nothing, whatever its result.
fn non_mutating_success(output: ServiceGitOutput) -> Result<ServiceGitOutput, ToolFailure> {
    if output.cancelled {
        return Err(no_change(ToolFailureKind::Cancelled, None));
    }
    if output.timed_out {
        return Err(no_change(ToolFailureKind::Deadline, None));
    }
    if output.exit_code != Some(0) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut detail: String = stderr.chars().take(MAX_DETAIL_BYTES).collect();
        if output.stderr_truncated {
            detail.push_str("...");
        }
        return Err(invalid_patch(Some(detail)));
    }
    if output.stdout_truncated {
        return Err(invalid_patch(Some(
            "patch affects too many paths to list".to_owned(),
        )));
    }
    Ok(output)
}

/// Reject binary patches and any mode other than 100644/100755 from the diff
/// headers. Hunk body lines always start with ' ', '+', '-' or '\\', so these
/// prefixes only occur in headers or in text Git ignores; rejecting the
/// latter is the safe direction.
fn validate_headers(patch: &str) -> Result<BTreeSet<String>, ToolFailure> {
    let mut names = BTreeSet::new();
    for line in patch.lines() {
        if line.starts_with("GIT binary patch") || line.starts_with("Binary files ") {
            return Err(invalid_patch(Some(
                "binary patches are not supported".to_owned(),
            )));
        }
        for prefix in [
            "old mode ",
            "new mode ",
            "new file mode ",
            "deleted file mode ",
        ] {
            if let Some(mode) = line.strip_prefix(prefix) {
                allowed_mode(mode.trim())?;
            }
        }
        if let Some(rest) = line.strip_prefix("index ")
            && let Some(mode) = rest.split_whitespace().nth(1)
        {
            allowed_mode(mode)?;
        }
        // `--numstat` omits the source of a pure rename or copy, so take
        // those names from the extended headers too, including Git quoting.
        for prefix in ["rename from ", "rename to ", "copy from ", "copy to "] {
            if let Some(name) = line.strip_prefix(prefix) {
                let name = unquote_path(name)?;
                names.insert(name);
            }
        }
    }
    Ok(names)
}

fn allowed_mode(mode: &str) -> Result<(), ToolFailure> {
    if matches!(mode, "100644" | "100755") {
        Ok(())
    } else {
        Err(invalid_patch(Some(format!(
            "file mode {mode} is not allowed; only regular files (100644, 100755) are supported"
        ))))
    }
}

/// Parse `git apply --numstat -z`: `added\tdeleted\tpath\0`, or for renames
/// and copies `added\tdeleted\t\0old\0new\0`. Binary entries use `-` counts.
fn affected_paths(stdout: &[u8], mut paths: BTreeSet<String>) -> Result<Vec<String>, ToolFailure> {
    let mut fields = stdout
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    while let Some(entry) = fields.next() {
        let entry = std::str::from_utf8(entry).map_err(|_| invalid_patch(None))?;
        let mut parts = entry.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid_patch(None));
        };
        if added == "-" || deleted == "-" {
            return Err(invalid_patch(Some(
                "binary patches are not supported".to_owned(),
            )));
        }
        if path.is_empty() {
            for _ in 0..2 {
                let name = fields.next().ok_or_else(|| invalid_patch(None))?;
                let name = std::str::from_utf8(name).map_err(|_| invalid_patch(None))?;
                paths.insert(name.to_owned());
            }
        } else {
            paths.insert(path.to_owned());
        }
    }
    if paths.is_empty() {
        return Err(invalid_patch(Some("no file changes found".to_owned())));
    }
    if paths.len() > MAX_PATCH_PATHS {
        return Err(invalid_patch(Some(format!(
            "patch affects {} paths; the limit is {MAX_PATCH_PATHS}",
            paths.len()
        ))));
    }
    Ok(paths.into_iter().collect())
}

/// Validate a workspace-root-relative path with lstat: normal components
/// only, no `.git` component, no symlink among existing components, and any
/// existing target must be a regular file.
fn validate_path(root: &Path, raw: &str) -> Result<(), ToolFailure> {
    let relative = Path::new(raw);
    let components = relative.components().collect::<Vec<_>>();
    let rejected = |reason: &str| invalid_patch(Some(format!("path {raw:?}: {reason}")));
    if raw.is_empty() || relative.is_absolute() {
        return Err(rejected("must be a non-empty workspace-relative path"));
    }
    for component in &components {
        match component {
            Component::Normal(name) if name.to_string_lossy().eq_ignore_ascii_case(".git") => {
                return Err(rejected("`.git` components are not allowed"));
            }
            Component::Normal(_) => {}
            _ => {
                return Err(rejected(
                    "`.`, `..` and absolute components are not allowed",
                ));
            }
        }
    }
    let mut cursor = root.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            unreachable!("components were validated above");
        };
        cursor.push(name);
        let last = index + 1 == components.len();
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(rejected("symlinks are not allowed"));
            }
            Ok(metadata) if last && !metadata.is_file() => {
                return Err(rejected("only regular files are supported"));
            }
            Ok(metadata) if !last && !metadata.is_dir() => {
                return Err(rejected("a parent component is not a directory"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return Err(no_change(ToolFailureKind::Execution, None)),
        }
    }
    Ok(())
}

/// Private, empty Git directory and HOME that replace every ambient Git
/// configuration source. Removed on drop.
struct PrivateGit {
    dir: PathBuf,
}

static PRIVATE_COUNTER: AtomicU64 = AtomicU64::new(0);

impl PrivateGit {
    fn create(operation: &AgentOperationKey) -> Result<Self, ToolFailure> {
        let dir = std::env::temp_dir().join(format!(
            "agl-file-patch-{}-{}-{}",
            operation.run_id,
            operation.ordinal,
            PRIVATE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let created = {
            use std::os::unix::fs::DirBuilderExt as _;
            fs::DirBuilder::new().mode(0o700).create(&dir)
        };
        created.map_err(|_| no_change(ToolFailureKind::Execution, None))?;
        let private = Self { dir };
        fs::create_dir(private.dir.join("home"))
            .map_err(|_| no_change(ToolFailureKind::Execution, None))?;
        Ok(private)
    }

    fn repo(&self) -> PathBuf {
        self.dir.join("repo.git")
    }

    /// Environment containing only `PATH` plus the isolation variables. Git
    /// reads no system or global config and never discovers the workspace's
    /// own `.git`. `GIT_DIR` is an empty private bare repository and there is
    /// deliberately no `GIT_WORK_TREE`, so Git sees no work tree and reads no
    /// `.gitattributes` from the workspace; no filter or conversion runs.
    /// (`GIT_ATTR_SOURCE` is not used: Git 2.54 `git apply` crashes with it.)
    fn env(&self) -> Vec<(OsString, OsString)> {
        vec![
            (
                OsString::from("PATH"),
                std::env::var_os("PATH").unwrap_or_default(),
            ),
            ("HOME".into(), self.dir.join("home").into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_ATTR_NOSYSTEM".into(), "1".into()),
            ("GIT_DIR".into(), self.repo().into()),
            ("LC_ALL".into(), "C".into()),
        ]
    }

    /// `git init --bare` into the private directory.
    fn init(&self, git: &ServiceGit, context: &ToolContext) -> Result<(), ToolFailure> {
        check_running(context)?;
        let output = git(ServiceGitRequest {
            args: vec![
                "init".into(),
                "--quiet".into(),
                "--bare".into(),
                "--template=".into(),
                self.repo().into(),
            ],
            cwd: self.dir.clone(),
            env: vec![
                (
                    OsString::from("PATH"),
                    std::env::var_os("PATH").unwrap_or_default(),
                ),
                ("HOME".into(), self.dir.join("home").into()),
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
                ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
                ("LC_ALL".into(), "C".into()),
            ],
            stdin: Vec::new(),
            timeout: Duration::from_millis(context.deadline_at_ms.saturating_sub(now_ms()) as u64),
            deadline_at_ms: context.deadline_at_ms,
            cancellation: context.cancellation(),
        })
        .map_err(|_| no_change(ToolFailureKind::Execution, None))?;
        if output.timed_out || output.exit_code != Some(0) {
            return Err(no_change(ToolFailureKind::Execution, None));
        }
        Ok(())
    }
}

impl Drop for PrivateGit {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn no_change(kind: ToolFailureKind, detail: Option<String>) -> ToolFailure {
    let actions = ["Nothing was changed. Check the workspace, then submit a corrected call."];
    match detail.and_then(|detail| CanonicalJson::new(json!({"git": detail})).ok()) {
        Some(details) => {
            ToolFailure::no_effect_with_details(kind, Some("patch"), &actions, details)
        }
        None => ToolFailure::no_effect(kind, Some("patch"), &actions),
    }
}

fn invalid_patch(detail: Option<String>) -> ToolFailure {
    let actions = [
        "Nothing was changed. Submit {\"patch\": \"<unified diff>\"} with paths relative to the workspace root (-p1 style a/ b/ prefixes), text files only, at most 64 paths. Re-read the target files with agentlibre.files:file.read so the diff context matches exactly.",
    ];
    match detail.and_then(|detail| CanonicalJson::new(json!({"git": detail})).ok()) {
        Some(details) => ToolFailure::no_effect_with_details(
            ToolFailureKind::InvalidInput,
            Some("patch"),
            &actions,
            details,
        ),
        None => ToolFailure::no_effect(ToolFailureKind::InvalidInput, Some("patch"), &actions),
    }
}

/// The mutating apply may have changed files. The agent must re-read the
/// affected files before another edit; the patch is never retried.
fn outcome_unknown() -> ToolFailure {
    ToolFailure::unknown(ToolFailureKind::OutcomeUnknown)
}

#[cfg(test)]
mod tests;

fn check_running(context: &ToolContext) -> Result<(), ToolFailure> {
    if context.is_cancelled() {
        return Err(no_change(ToolFailureKind::Cancelled, None));
    }
    if now_ms() >= context.deadline_at_ms {
        return Err(no_change(ToolFailureKind::Deadline, None));
    }
    Ok(())
}
/// Git uses C quoting with octal escapes for bytes in extended path headers.
fn unquote_path(name: &str) -> Result<String, ToolFailure> {
    if !name.starts_with('"') {
        return Ok(name.to_owned());
    }
    let body = name
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or_else(|| invalid_patch(None))?;
    let mut bytes = body.bytes();
    let mut result = Vec::new();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            result.push(byte);
            continue;
        }
        let escaped = bytes.next().ok_or_else(|| invalid_patch(None))?;
        result.push(match escaped {
            b'a' => 7,
            b'b' => 8,
            b't' => 9,
            b'n' => 10,
            b'v' => 11,
            b'f' => 12,
            b'r' => 13,
            b'"' => b'"',
            b'\\' => b'\\',
            b'0'..=b'3' => {
                let a = bytes
                    .next()
                    .filter(|b| (b'0'..=b'7').contains(b))
                    .ok_or_else(|| invalid_patch(None))?;
                let b = bytes
                    .next()
                    .filter(|b| (b'0'..=b'7').contains(b))
                    .ok_or_else(|| invalid_patch(None))?;
                (escaped - b'0') * 64 + (a - b'0') * 8 + b - b'0'
            }
            _ => return Err(invalid_patch(None)),
        });
    }
    String::from_utf8(result).map_err(|_| invalid_patch(None))
}
