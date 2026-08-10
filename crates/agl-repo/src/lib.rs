#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agl_artifact::ArtifactHandle;
use agl_package::{PackageLock, PackageSourceDeclaration, PackageSourceKind, WorkspaceManifest};
use anyhow::{Context, Result, bail, ensure};

mod git_artifact;

pub use git_artifact::*;

pub const AGL_DIR: &str = ".agl";
pub const WORKSPACE_MANIFEST_PATH: &str = ".agl/workspace.toml";
pub const PACKAGE_LOCK_PATH: &str = ".agl/package-lock.toml";
pub const DEFAULT_FUNCTION: &str = "function:gemma4-e4b@^1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitSourceProvenance {
    pub revision: String,
    pub tree: String,
}

pub fn resolve_repo_root(start: impl AsRef<Path>) -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(start.as_ref())
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("failed to run git rev-parse")?;
    if !output.status.success() {
        bail!("not inside a Git repository");
    }
    Ok(PathBuf::from(String::from_utf8(output.stdout)?.trim()))
}

pub fn git_source_provenance(root: impl AsRef<Path>) -> Result<GitSourceProvenance> {
    let root = root.as_ref();
    Ok(GitSourceProvenance {
        revision: git_output(root, &["rev-parse", "HEAD"])?,
        tree: git_output(root, &["rev-parse", "HEAD^{tree}"])?,
    })
}

pub fn verified_git_source_provenance(
    root: impl AsRef<Path>,
    revision: &str,
) -> Result<GitSourceProvenance> {
    let root = root.as_ref();
    let head = git_output(root, &["rev-parse", "HEAD"])?;
    let revision = git_output(
        root,
        &["rev-parse", "--verify", &format!("{revision}^{{commit}}")],
    )?;
    ensure!(
        head == revision,
        "Git package source HEAD `{head}` does not match declared revision `{revision}`"
    );
    let dirty = git_output(
        root,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    )?;
    ensure!(
        dirty.is_empty(),
        "Git package source has uncommitted or ignored files"
    );
    git_source_provenance(root)
}

pub fn resolve_package_source_root(
    workspace_root: impl AsRef<Path>,
    source: &PackageSourceDeclaration,
) -> Result<PathBuf> {
    let workspace_root = workspace_root.as_ref().canonicalize()?;
    ensure!(
        source.kind == PackageSourceKind::Directory,
        "only a materialized directory package source has a direct root"
    );
    let relative = source
        .path
        .as_ref()
        .context("package source path is missing")?;
    let candidate = workspace_root.join(relative);
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("failed to resolve package source {}", source.id))?;
    ensure!(
        canonical.starts_with(&workspace_root),
        "package source escapes workspace"
    );
    Ok(canonical)
}

pub fn materialize_package_source(
    workspace_root: impl AsRef<Path>,
    source: &PackageSourceDeclaration,
) -> Result<PathBuf> {
    let workspace_root = workspace_root.as_ref().canonicalize()?;
    match source.kind {
        PackageSourceKind::Directory => resolve_package_source_root(&workspace_root, source),
        PackageSourceKind::Git => {
            let workspace_root = resolve_repo_root(&workspace_root)?;
            let url = source
                .url
                .as_deref()
                .context("Git package source URL is missing")?;
            let rev = source
                .rev
                .as_deref()
                .context("Git package source revision is missing")?;
            let target = workspace_root.join(".agl/sources").join(source.id.as_str());
            if target.exists() {
                git_output(&target, &["fetch", "--tags", "--quiet"])?;
            } else {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                let output = Command::new("git")
                    .arg("-c")
                    .arg("protocol.file.allow=always")
                    .arg("clone")
                    .arg("--quiet")
                    .arg(url)
                    .arg(&target)
                    .output()?;
                if !output.status.success() {
                    bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
                }
            }
            git_output(&target, &["checkout", "--quiet", rev])?;
            Ok(target.canonicalize()?)
        }
        PackageSourceKind::Embedded => bail!("embedded package sources cannot be materialized"),
    }
}

pub fn read_workspace_manifest(path: impl AsRef<Path>) -> Result<WorkspaceManifest> {
    WorkspaceManifest::from_toml(&fs::read_to_string(path.as_ref())?).map_err(Into::into)
}

pub fn write_workspace_manifest(
    path: impl AsRef<Path>,
    manifest: &WorkspaceManifest,
) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, manifest.to_toml()?)?;
    Ok(())
}

pub fn read_optional_package_lock(path: impl AsRef<Path>) -> Result<Option<PackageLock>> {
    match fs::read_to_string(path.as_ref()) {
        Ok(value) => Ok(Some(PackageLock::from_toml(&value)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub fn write_package_lock(path: impl AsRef<Path>, lock: &PackageLock) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    lock.write_atomic(path)?;
    Ok(())
}

pub fn read_workspace_default_function(start: impl AsRef<Path>) -> Result<Option<String>> {
    let path = start.as_ref().join(WORKSPACE_MANIFEST_PATH);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(
        read_workspace_manifest(path)?.default_function.to_string(),
    ))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSpecVerifyOptions {
    pub strict: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSpecValidation {
    pub missing_sections: Vec<String>,
}

impl TaskSpecValidation {
    pub fn is_valid(&self) -> bool {
        self.missing_sections.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSpecVerifyReport {
    pub files: Vec<String>,
    pub errors: Vec<String>,
}

impl TaskSpecVerifyReport {
    pub fn should_fail(&self, strict: bool) -> bool {
        !self.errors.is_empty() || (strict && self.files.is_empty())
    }
}

pub fn validate_task_spec_markdown(markdown: &str) -> TaskSpecValidation {
    const REQUIRED: [&str; 7] = [
        "Problem",
        "Goal",
        "Scope",
        "Non-goals",
        "Implementation",
        "Acceptance Criteria",
        "Verification",
    ];
    let headings = markdown
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let level = line.bytes().take_while(|byte| *byte == b'#').count();
            if !(1..=6).contains(&level) || line.as_bytes().get(level) != Some(&b' ') {
                return None;
            }
            Some(line[level + 1..].trim().to_ascii_lowercase())
        })
        .collect::<std::collections::BTreeSet<_>>();
    TaskSpecValidation {
        missing_sections: REQUIRED
            .into_iter()
            .filter(|section| !headings.contains(&section.to_ascii_lowercase()))
            .map(str::to_owned)
            .collect(),
    }
}

pub fn verify_task_specs(
    handle: &ArtifactHandle,
    options: &TaskSpecVerifyOptions,
) -> Result<TaskSpecVerifyReport> {
    ensure!(
        handle.id().as_str() == "core.repo:tasks",
        "task validator requires core.repo:tasks"
    );
    handle.require_access(agl_kernel::ArtifactAccess::ReadTree)?;
    let mut files = Vec::new();
    let mut errors = Vec::new();
    for path in handle.files()? {
        if !path.as_str().ends_with("/00_overview.md") {
            continue;
        }
        let markdown = String::from_utf8(handle.read(path.clone())?)?;
        if !markdown.contains("status: planned") {
            continue;
        }
        let validation = validate_task_spec_markdown(&markdown);
        if !validation.is_valid() {
            errors.push(format!(
                "{}: missing {}",
                path,
                validation.missing_sections.join(", ")
            ));
        }
        files.push(path.to_string());
    }
    if options.strict && files.is_empty() {
        errors.push("core.repo:tasks contains no planned task overview".to_owned());
    }
    Ok(TaskSpecVerifyReport { files, errors })
}

fn git_output(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}
