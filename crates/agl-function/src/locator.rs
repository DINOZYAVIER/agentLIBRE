use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
#[cfg(test)]
use anyhow::{bail, ensure};
use serde::{Deserialize, Serialize};

use crate::loader::load_function;
use crate::manifest::FUNCTION_FILE_NAME;
#[cfg(test)]
use crate::validation::validate_function_id;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionPackageSource {
    Explicit,
    Workspace,
    Global,
    Builtin,
}

impl FunctionPackageSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Workspace => "workspace",
            Self::Global => "global",
            Self::Builtin => "builtin",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FunctionPackageLocation {
    pub reference: String,
    pub source: FunctionPackageSource,
    pub path: PathBuf,
    pub root_dir: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FunctionListEntry {
    pub source: FunctionPackageSource,
    pub id: String,
    pub path: PathBuf,
    pub valid: bool,
    pub title: Option<String>,
    pub error: Option<String>,
}

pub fn workspace_functions_root(workspace_root: impl AsRef<Path>) -> PathBuf {
    workspace_root.as_ref().join(".agl").join("functions")
}

pub fn global_functions_root(config_dir: impl AsRef<Path>) -> PathBuf {
    config_dir.as_ref().join("functions")
}

#[cfg(test)]
pub fn resolve_function_package(
    reference: &str,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
) -> Result<FunctionPackageLocation> {
    ensure!(
        !reference.trim().is_empty(),
        "function reference cannot be empty"
    );
    if looks_like_path(reference) {
        let path = normalize_function_file_path(PathBuf::from(reference));
        let root_dir = path
            .parent()
            .map(Path::to_path_buf)
            .with_context(|| format!("function path has no parent: {}", path.display()))?;
        return Ok(FunctionPackageLocation {
            reference: reference.to_string(),
            source: FunctionPackageSource::Explicit,
            path,
            root_dir,
        });
    }

    validate_function_id("function id", reference)?;
    let workspace = workspace_functions_root(&workspace_root)
        .join(reference)
        .join(FUNCTION_FILE_NAME);
    let global = global_functions_root(&config_dir)
        .join(reference)
        .join(FUNCTION_FILE_NAME);
    let (source, path) = if workspace.is_file() {
        (FunctionPackageSource::Workspace, workspace)
    } else if global.is_file() {
        (FunctionPackageSource::Global, global)
    } else if agl_assets::BUILTIN_PACKAGES
        .iter()
        .any(|package| package.type_id == "function" && package.id == reference)
    {
        (
            FunctionPackageSource::Builtin,
            builtin_package_path(reference),
        )
    } else {
        bail!("function package not found: {reference}")
    };
    Ok(FunctionPackageLocation {
        reference: reference.to_string(),
        source,
        root_dir: path
            .parent()
            .expect("function path has parent")
            .to_path_buf(),
        path,
    })
}

pub fn list_functions(
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
) -> Result<Vec<FunctionListEntry>> {
    let mut entries = Vec::new();
    collect_function_entries(
        FunctionPackageSource::Workspace,
        workspace_functions_root(&workspace_root),
        &mut entries,
    )?;
    collect_function_entries(
        FunctionPackageSource::Global,
        global_functions_root(&config_dir),
        &mut entries,
    )?;
    collect_builtin_entries(&mut entries);
    entries.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.source.as_str().cmp(right.source.as_str()))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(entries)
}

pub(crate) fn collect_function_entries(
    source: FunctionPackageSource,
    root: PathBuf,
    entries: &mut Vec<FunctionListEntry>,
) -> Result<()> {
    let read_dir = match std::fs::read_dir(&root) {
        Ok(read_dir) => read_dir,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err)
                .with_context(|| format!("failed to read functions root {}", root.display()));
        }
    };
    for entry in read_dir {
        let entry = entry.with_context(|| format!("failed to read {}", root.display()))?;
        let file_type = entry.file_type().with_context(|| {
            format!(
                "failed to read function entry type {}",
                entry.path().display()
            )
        })?;
        if !file_type.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        let path = entry.path().join(FUNCTION_FILE_NAME);
        if !path.is_file() {
            continue;
        }
        let locator = FunctionPackageLocation {
            reference: id.clone(),
            source,
            path: path.clone(),
            root_dir: entry.path(),
        };
        match load_function(locator) {
            Ok(function) => entries.push(FunctionListEntry {
                source,
                id: function.front_matter.id().to_owned(),
                path,
                valid: true,
                title: Some(function.front_matter.title),
                error: None,
            }),
            Err(err) => entries.push(FunctionListEntry {
                source,
                id,
                path,
                valid: false,
                title: None,
                error: Some(format!("{err:#}")),
            }),
        }
    }
    Ok(())
}

pub(crate) fn collect_builtin_entries(entries: &mut Vec<FunctionListEntry>) {
    for function in agl_assets::BUILTIN_PACKAGES {
        if function.type_id != "function" {
            continue;
        }
        let path = builtin_package_path(function.id);
        let locator = FunctionPackageLocation {
            reference: function.id.to_string(),
            source: FunctionPackageSource::Builtin,
            path: path.clone(),
            root_dir: path
                .parent()
                .expect("builtin function path has parent")
                .to_path_buf(),
        };
        match load_function(locator) {
            Ok(loaded) => entries.push(FunctionListEntry {
                source: FunctionPackageSource::Builtin,
                id: function.id.to_string(),
                path: path.clone(),
                valid: true,
                title: Some(loaded.front_matter.title),
                error: None,
            }),
            Err(err) => entries.push(FunctionListEntry {
                source: FunctionPackageSource::Builtin,
                id: function.id.to_string(),
                path,
                valid: false,
                title: None,
                error: Some(format!("{err:#}")),
            }),
        }
    }
}

fn builtin_package_path(id: &str) -> PathBuf {
    PathBuf::from(format!("builtin:function/{id}/{FUNCTION_FILE_NAME}"))
}

#[cfg(test)]
pub(crate) fn looks_like_path(reference: &str) -> bool {
    reference.contains('/')
        || reference.contains('\\')
        || reference.ends_with(".md")
        || reference.starts_with('.')
}

#[cfg(test)]
pub(crate) fn normalize_function_file_path(path: PathBuf) -> PathBuf {
    if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
        path
    } else {
        path.join(FUNCTION_FILE_NAME)
    }
}
