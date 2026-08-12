use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Deserializer, Serialize};

pub const MODEL_BINDINGS_FILE_NAME: &str = "models.toml";

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(transparent)]
pub struct ModelId(String);

impl ModelId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        ensure!(!value.is_empty(), "model id cannot be empty");
        ensure!(
            value.trim() == value,
            "model id cannot contain leading or trailing whitespace"
        );
        ensure!(
            value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.')),
            "model id `{value}` contains unsupported characters"
        );
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ModelId {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

impl<'de> Deserialize<'de> for ModelId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelBinding {
    pub path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelBindings {
    pub version: u32,
    pub models: BTreeMap<ModelId, ModelBinding>,
}

impl ModelBindings {
    pub fn empty() -> Self {
        Self {
            version: 1,
            models: BTreeMap::new(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
            "unsupported model bindings version {}",
            self.version
        );
        for (id, binding) in &self.models {
            validate_model_path_value(id, &binding.path)?;
        }
        Ok(())
    }
}

pub fn model_bindings_path(config_dir: impl AsRef<Path>) -> PathBuf {
    config_dir.as_ref().join(MODEL_BINDINGS_FILE_NAME)
}

pub fn load_model_bindings(path: impl AsRef<Path>) -> Result<ModelBindings> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read model bindings {}", path.display()))?;
    let bindings: ModelBindings = toml::from_str(&text)
        .with_context(|| format!("failed to parse model bindings {}", path.display()))?;
    bindings
        .validate()
        .with_context(|| format!("invalid model bindings {}", path.display()))?;
    Ok(bindings)
}

pub fn load_model_bindings_or_empty(path: impl AsRef<Path>) -> Result<ModelBindings> {
    let path = path.as_ref();
    if !path.exists() {
        return Ok(ModelBindings::empty());
    }
    load_model_bindings(path)
}

pub fn write_model_bindings(path: impl AsRef<Path>, bindings: &ModelBindings) -> Result<()> {
    let path = path.as_ref();
    bindings.validate()?;
    let text = toml::to_string_pretty(bindings).context("failed to serialize model bindings")?;
    atomic_write(path, text.as_bytes())
        .with_context(|| format!("failed to write model bindings {}", path.display()))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("path {} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("model bindings filename must be UTF-8")?;
    let temporary = parent.join(format!(".{file_name}.{}.tmp", std::process::id()));
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.into());
    }
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

fn validate_model_path_value(id: &ModelId, path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty() && !path.as_os_str().to_string_lossy().trim().is_empty(),
        "model `{id}` path cannot be blank"
    );
    Ok(())
}
