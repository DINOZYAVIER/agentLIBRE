use std::path::{Path, PathBuf};

use agl_artifact::{ArtifactPackageRef, ArtifactResolver, ArtifactSourceTier};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::adapter::{builtin_source, directory_function_source, function_adapter_registry};
use crate::loader::load_function;
use crate::manifest::FUNCTION_FILE_NAME;
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
    let registry = function_adapter_registry()?;
    let sources = vec![
        directory_function_source(
            "workspace".parse()?,
            ArtifactSourceTier::Workspace,
            workspace_root.as_ref().join(".agl"),
            registry.clone(),
        ),
        directory_function_source(
            "global".parse()?,
            ArtifactSourceTier::User,
            config_dir.as_ref(),
            registry.clone(),
        ),
        builtin_source()?,
    ];
    let resolver = ArtifactResolver::new(registry, sources);
    let package_ref = ArtifactPackageRef::parse(&format!("function:{reference}@*"))?;
    let graph = resolver.resolve_and_validate(&package_ref, None)?;
    let node = graph
        .nodes
        .get(&graph.root)
        .context("resolved function graph is missing its root")?;
    let (source, path) = match node.candidate.tier {
        ArtifactSourceTier::Workspace => (
            FunctionPackageSource::Workspace,
            workspace_functions_root(&workspace_root)
                .join(reference)
                .join(FUNCTION_FILE_NAME),
        ),
        ArtifactSourceTier::User => (
            FunctionPackageSource::Global,
            global_functions_root(&config_dir)
                .join(reference)
                .join(FUNCTION_FILE_NAME),
        ),
        ArtifactSourceTier::Builtin => (
            FunctionPackageSource::Builtin,
            builtin_package_path(reference),
        ),
        tier => bail!("unsupported Function source tier {tier:?}"),
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
    for function in agl_assets::BUILTIN_ARTIFACT_PACKAGES {
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
