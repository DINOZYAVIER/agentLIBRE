use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::loader::load_function;
use crate::manifest::FUNCTION_FILE_NAME;
use crate::validation::validate_function_id;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionSource {
    Explicit,
    Workspace,
    Global,
    Builtin,
}

impl FunctionSource {
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
pub struct FunctionLocator {
    pub reference: String,
    pub source: FunctionSource,
    pub path: PathBuf,
    pub root_dir: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FunctionListEntry {
    pub source: FunctionSource,
    pub id: String,
    pub path: PathBuf,
    pub valid: bool,
    pub title: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProfileResolution {
    pub profile: String,
    pub selected_path: Option<PathBuf>,
    pub candidates: Vec<PathBuf>,
}

pub fn workspace_functions_root(workspace_root: impl AsRef<Path>) -> PathBuf {
    workspace_root.as_ref().join(".agl").join("functions")
}

pub fn global_functions_root(config_dir: impl AsRef<Path>) -> PathBuf {
    config_dir.as_ref().join("functions")
}

pub fn workspace_profile_path(workspace_root: impl AsRef<Path>, profile: &str) -> PathBuf {
    workspace_root
        .as_ref()
        .join(".agl")
        .join("inference")
        .join("profiles")
        .join(format!("{profile}.toml"))
}

pub fn global_profile_path(config_dir: impl AsRef<Path>, profile: &str) -> PathBuf {
    config_dir
        .as_ref()
        .join("inference")
        .join("profiles")
        .join(format!("{profile}.toml"))
}

pub fn default_local_profile_path(config_dir: impl AsRef<Path>) -> PathBuf {
    config_dir.as_ref().join("inference").join("local.toml")
}

pub fn resolve_profile(
    profile: &str,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
) -> Result<ProfileResolution> {
    validate_function_id("model.profile", profile)?;
    if profile == "local" {
        let path = default_local_profile_path(config_dir);
        return Ok(ProfileResolution {
            profile: profile.to_string(),
            selected_path: Some(path.clone()),
            candidates: vec![path],
        });
    }

    let candidates = vec![
        workspace_profile_path(&workspace_root, profile),
        global_profile_path(&config_dir, profile),
    ];
    let selected_path = candidates.iter().find(|path| path.is_file()).cloned();
    Ok(ProfileResolution {
        profile: profile.to_string(),
        selected_path,
        candidates,
    })
}

pub fn resolve_function_reference(
    reference: &str,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
) -> Result<FunctionLocator> {
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
        return Ok(FunctionLocator {
            reference: reference.to_string(),
            source: FunctionSource::Explicit,
            path,
            root_dir,
        });
    }

    validate_function_id("function id", reference)?;
    let workspace_path = workspace_functions_root(&workspace_root)
        .join(reference)
        .join(FUNCTION_FILE_NAME);
    if workspace_path.is_file() {
        return Ok(FunctionLocator {
            reference: reference.to_string(),
            source: FunctionSource::Workspace,
            root_dir: workspace_path
                .parent()
                .expect("workspace function path has parent")
                .to_path_buf(),
            path: workspace_path,
        });
    }

    let global_path = global_functions_root(&config_dir)
        .join(reference)
        .join(FUNCTION_FILE_NAME);
    if global_path.is_file() {
        return Ok(FunctionLocator {
            reference: reference.to_string(),
            source: FunctionSource::Global,
            root_dir: global_path
                .parent()
                .expect("global function path has parent")
                .to_path_buf(),
            path: global_path,
        });
    }

    if let Some(function) = agl_assets::builtin_function(reference) {
        return Ok(FunctionLocator {
            reference: reference.to_string(),
            source: FunctionSource::Builtin,
            path: PathBuf::from(function.function_md.source_path),
            root_dir: PathBuf::from(function.function_md.source_path)
                .parent()
                .expect("builtin function source path has parent")
                .to_path_buf(),
        });
    }

    bail!(
        "function `{reference}` not found; checked {}, {}, and builtin functions",
        workspace_path.display(),
        global_path.display()
    )
}

pub fn list_functions(
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
) -> Result<Vec<FunctionListEntry>> {
    let mut entries = Vec::new();
    collect_function_entries(
        FunctionSource::Workspace,
        workspace_functions_root(&workspace_root),
        &mut entries,
    )?;
    collect_function_entries(
        FunctionSource::Global,
        global_functions_root(&config_dir),
        &mut entries,
    )?;
    collect_builtin_function_entries(&mut entries);
    entries.sort_by(|left, right| {
        left.id
            .cmp(&right.id)
            .then_with(|| left.source.as_str().cmp(right.source.as_str()))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(entries)
}

pub(crate) fn collect_function_entries(
    source: FunctionSource,
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
        let locator = FunctionLocator {
            reference: id.clone(),
            source,
            path: path.clone(),
            root_dir: entry.path(),
        };
        match load_function(locator) {
            Ok(function) => entries.push(FunctionListEntry {
                source,
                id: function.front_matter.id,
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

pub(crate) fn collect_builtin_function_entries(entries: &mut Vec<FunctionListEntry>) {
    for function in agl_assets::BUILTIN_FUNCTIONS {
        let locator = FunctionLocator {
            reference: function.id.to_string(),
            source: FunctionSource::Builtin,
            path: PathBuf::from(function.function_md.source_path),
            root_dir: PathBuf::from(function.function_md.source_path)
                .parent()
                .expect("builtin function source path has parent")
                .to_path_buf(),
        };
        match load_function(locator) {
            Ok(loaded) => entries.push(FunctionListEntry {
                source: FunctionSource::Builtin,
                id: function.id.to_string(),
                path: PathBuf::from(function.function_md.source_path),
                valid: true,
                title: Some(loaded.front_matter.title),
                error: None,
            }),
            Err(err) => entries.push(FunctionListEntry {
                source: FunctionSource::Builtin,
                id: function.id.to_string(),
                path: PathBuf::from(function.function_md.source_path),
                valid: false,
                title: None,
                error: Some(format!("{err:#}")),
            }),
        }
    }
}

pub(crate) fn looks_like_path(reference: &str) -> bool {
    reference.contains('/')
        || reference.contains('\\')
        || reference.ends_with(".md")
        || reference.starts_with('.')
}

pub(crate) fn normalize_function_file_path(path: PathBuf) -> PathBuf {
    if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
        path
    } else {
        path.join(FUNCTION_FILE_NAME)
    }
}
