use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agl_runtime::package::PackageId;
use anyhow::{Context, Result, bail, ensure};
use semver::VersionReq;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

pub(crate) const CONFIG_FILE_NAME: &str = "agentLIBRE.toml";
const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

#[derive(Clone, Debug, Deserialize, garde::Validate)]
#[serde(deny_unknown_fields)]
pub(crate) struct HumanConfig {
    #[garde(dive)]
    pub chat: ChatConfig,
    #[serde(default)]
    #[garde(dive)]
    pub inference: InferenceConfig,
    #[serde(default)]
    #[garde(dive)]
    pub integrations: IntegrationConfig,
    #[serde(default)]
    #[garde(dive)]
    pub credentials: BTreeMap<String, CredentialConfig>,
    #[serde(default)]
    #[garde(dive)]
    pub repl: ReplConfig,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, garde::Validate)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ReplConfig {
    #[garde(skip)]
    pub decorations_default: Decorations,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum Decorations {
    Off,
    #[default]
    Default,
    Full,
}

impl<'de> Deserialize<'de> for Decorations {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "off" => Ok(Self::Off),
            "default" => Ok(Self::Default),
            "full" => Ok(Self::Full),
            value => Err(serde::de::Error::custom(format!(
                "decorations_default must be off, default, or full, got {value:?}"
            ))),
        }
    }
}

#[derive(Clone, Debug, Deserialize, garde::Validate)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChatConfig {
    #[garde(skip)]
    pub default_function: FunctionRequirement,
}

#[derive(Clone, Debug, Default, Deserialize, garde::Validate)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct InferenceConfig {
    #[garde(skip)]
    pub executable: Option<PathBuf>,
    #[garde(dive)]
    pub memory: MemoryConfig,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, garde::Validate)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct MemoryConfig {
    #[garde(skip)]
    pub keep_free_ram: Option<BinarySize>,
    #[garde(skip)]
    pub keep_free_vram: Option<BinarySize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BinarySize(pub u64);

impl<'de> Deserialize<'de> for BinarySize {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse_binary_size(&value)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Default, Deserialize, garde::Validate)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct IntegrationConfig {
    #[garde(dive)]
    pub search: Option<SearchIntegration>,
}

#[derive(Clone, Debug, Deserialize, garde::Validate)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchIntegration {
    #[garde(skip)]
    pub required: bool,
    #[garde(ascii, length(min = 1, max = 128))]
    pub credential: String,
}

#[derive(Clone, Debug, Deserialize, garde::Validate)]
#[serde(deny_unknown_fields)]
pub(crate) struct CredentialConfig {
    #[garde(skip)]
    pub client_certificate: PathBuf,
    #[garde(skip)]
    pub client_private_key: PathBuf,
    #[garde(skip)]
    pub private_ca: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FunctionRequirement {
    pub id: PackageId,
    pub version: VersionReq,
}

impl<'de> Deserialize<'de> for FunctionRequirement {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        parse_function_requirement(&String::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FunctionLocator {
    Requirement(FunctionRequirement),
    Directory(PathBuf),
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedFunction {
    pub directory: PathBuf,
}

pub(crate) fn load_document(config_root: &Path) -> Result<(HumanConfig, Vec<u8>)> {
    let path = config_root.join(CONFIG_FILE_NAME);
    let bytes = super::read_regular_file(&path, MAX_CONFIG_BYTES)?;
    let config: HumanConfig =
        toml::from_slice(&bytes).with_context(|| format!("invalid {}", path.display()))?;
    garde::Validate::validate(&config)
        .map_err(|error| anyhow::anyhow!("invalid {}: {error}", path.display()))?;
    config.validate(&path)?;
    Ok((config, bytes))
}

#[derive(Clone, Debug)]
pub(crate) struct ConfigPlan {
    pub source_digest: String,
    pub functions: Vec<PreparedFunction>,
    pub executable: PathBuf,
    pub search: Option<ActiveSearch>,
    pub keep_free_ram: Option<u64>,
    pub keep_free_vram: Option<u64>,
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedFunction {
    pub directory: PathBuf,
    pub id: String,
    pub version: String,
    pub digest: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActiveConfig {
    pub schema: String,
    pub source_digest: String,
    pub functions: Vec<ActiveFunction>,
    pub executable: PathBuf,
    pub search: Option<ActiveSearch>,
    pub keep_free_ram: Option<u64>,
    pub keep_free_vram: Option<u64>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActiveFunction {
    pub directory: PathBuf,
    pub id: String,
    pub version: String,
    pub digest: String,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ActiveSearch {
    pub required: bool,
    pub client_certificate: PathBuf,
    pub client_private_key: PathBuf,
    pub private_ca: PathBuf,
}

pub(crate) fn plan(config_root: &Path, extra_functions: &[String]) -> Result<ConfigPlan> {
    let (human, bytes) = load_document(config_root)?;
    let workspace = std::env::current_dir()?.canonicalize()?;
    let default = resolve_requirement(&human.chat.default_function, &workspace)?;
    let mut directories = vec![default.directory];
    for locator in extra_functions {
        let parsed = parse_locator(locator, &std::env::current_dir()?)?;
        let directory = match parsed {
            FunctionLocator::Directory(directory) => directory,
            FunctionLocator::Requirement(requirement) => {
                resolve_requirement(&requirement, &workspace)?.directory
            }
        };
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    let functions = directories
        .into_iter()
        .map(|directory| {
            let identity = agl_runtime::function_identity(&directory, &workspace)?;
            Ok(PreparedFunction {
                directory: identity.directory,
                id: identity.id.to_string(),
                version: identity.version.to_string(),
                digest: identity.digest.to_string(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let executable = human
        .inference
        .executable
        .clone()
        .unwrap_or_else(default_engine_path);
    validate_executable(&executable)?;
    let search = human.integrations.search.as_ref().map(|search| {
        let credential = human
            .credentials
            .get(&search.credential)
            .expect("validated credential reference");
        ActiveSearch {
            required: search.required,
            client_certificate: credential.client_certificate.clone(),
            client_private_key: credential.client_private_key.clone(),
            private_ca: credential.private_ca.clone(),
        }
    });
    Ok(ConfigPlan {
        source_digest: digest(&bytes),
        functions,
        executable,
        search,
        keep_free_ram: human.inference.memory.keep_free_ram.map(|value| value.0),
        keep_free_vram: human.inference.memory.keep_free_vram.map(|value| value.0),
    })
}

pub(crate) fn active(plan: &ConfigPlan) -> ActiveConfig {
    ActiveConfig {
        schema: "agentlibre.active-config/v1".to_owned(),
        source_digest: plan.source_digest.clone(),
        functions: plan
            .functions
            .iter()
            .map(|function| ActiveFunction {
                directory: function.directory.clone(),
                id: function.id.clone(),
                version: function.version.clone(),
                digest: function.digest.clone(),
            })
            .collect(),
        executable: plan.executable.clone(),
        search: plan.search.clone(),
        keep_free_ram: plan.keep_free_ram,
        keep_free_vram: plan.keep_free_vram,
    }
}

pub(crate) fn active_path(data_root: &Path) -> PathBuf {
    data_root.join("config/active.json")
}

pub(crate) fn read_active(data_root: &Path) -> Result<ActiveConfig> {
    let path = active_path(data_root);
    let bytes = super::read_regular_file(&path, MAX_CONFIG_BYTES)?;
    let active: ActiveConfig = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid generated config {}", path.display()))?;
    ensure!(
        active.schema == "agentlibre.active-config/v1",
        "unsupported generated config schema"
    );
    Ok(active)
}

pub(crate) fn write_active(data_root: &Path, active: &ActiveConfig) -> Result<()> {
    let path = active_path(data_root);
    let parent = path.parent().context("generated config has no parent")?;
    std::fs::create_dir_all(parent)?;
    let staged = parent.join(format!(".active.json.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(active)?;
    {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&staged)?;
        use std::io::Write as _;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&staged, &path)?;
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut value = String::from("sha256:");
    for byte in digest {
        value.push_str(&format!("{byte:02x}"));
    }
    value
}

fn default_engine_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .map(|prefix| prefix.join("libexec/agentlibre/llama-server"))
        .unwrap_or_else(|| PathBuf::from("/usr/libexec/agentlibre/llama-server"))
}

fn validate_executable(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inference executable {} is unavailable", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "inference executable {} must be a regular non-symlink file",
        path.display()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        ensure!(
            metadata.permissions().mode() & 0o111 != 0,
            "inference executable {} is not executable",
            path.display()
        );
    }
    Ok(())
}

impl HumanConfig {
    fn validate(&self, path: &Path) -> Result<()> {
        if let Some(executable) = &self.inference.executable {
            ensure!(
                executable.is_absolute(),
                "inference.executable in {} must be absolute",
                path.display()
            );
        }
        for (id, credential) in &self.credentials {
            validate_logical_id(id).with_context(|| format!("invalid credentials.{id}"))?;
            for (field, value) in [
                ("client_certificate", &credential.client_certificate),
                ("client_private_key", &credential.client_private_key),
                ("private_ca", &credential.private_ca),
            ] {
                ensure!(
                    value.is_absolute(),
                    "credentials.{id}.{field} in {} must be absolute",
                    path.display()
                );
            }
        }
        if let Some(search) = &self.integrations.search {
            validate_logical_id(&search.credential)
                .context("integrations.search.credential is invalid")?;
            ensure!(
                self.credentials.contains_key(&search.credential),
                "integrations.search.credential references missing credentials.{}",
                search.credential
            );
        }
        Ok(())
    }
}

pub(crate) fn parse_locator(value: &str, cwd: &Path) -> Result<FunctionLocator> {
    if value.starts_with("function:") {
        return Ok(FunctionLocator::Requirement(parse_function_requirement(
            value,
        )?));
    }
    let candidate = if Path::new(value).is_absolute() {
        PathBuf::from(value)
    } else {
        cwd.join(value)
    };
    if candidate.is_dir() {
        return Ok(FunctionLocator::Directory(candidate.canonicalize()?));
    }
    bail!(
        "Function locator must be a typed function:ID@VERSION requirement or an existing Function directory"
    )
}

pub(crate) fn resolve_requirement(
    requirement: &FunctionRequirement,
    workspace: &Path,
) -> Result<ResolvedFunction> {
    Ok(ResolvedFunction {
        directory: agl_runtime::resolve_function_requirement(
            &requirement.id,
            &requirement.version,
            workspace,
        )?
        .directory,
    })
}

fn parse_function_requirement(value: &str) -> Result<FunctionRequirement> {
    let body = value
        .strip_prefix("function:")
        .context("Function requirement must begin with function:")?;
    let (id, version) = body
        .split_once('@')
        .context("Function requirement must contain @VERSION")?;
    ensure!(!version.is_empty(), "Function version requirement is empty");
    ensure!(
        !version.contains('@'),
        "Function requirement contains multiple @ separators"
    );
    Ok(FunctionRequirement {
        id: PackageId::new(id)?,
        version: VersionReq::parse(version)?,
    })
}

fn parse_binary_size(value: &str) -> Result<u64> {
    let (number, multiplier) = if let Some(number) = value.strip_suffix(" MiB") {
        (number, MIB)
    } else if let Some(number) = value.strip_suffix(" GiB") {
        (number, GIB)
    } else {
        bail!("memory reserve must be a positive integer followed by ` MiB` or ` GiB`");
    };
    let number: u64 = number
        .parse()
        .context("memory reserve must use a positive base-10 integer")?;
    ensure!(number > 0, "memory reserve must be positive");
    number
        .checked_mul(multiplier)
        .context("memory reserve exceeds u64")
}

fn validate_logical_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')),
        "logical ID must contain 1 to 128 ASCII letters, digits, `.`, `_`, or `-`"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_human_config_requires_typed_function_and_explicit_integration_criticality() {
        let valid: HumanConfig = toml::from_str(
            r#"
[chat]
default_function = "function:agentlibre.search@^1.2"

[inference.memory]
keep_free_ram = "12 GiB"
keep_free_vram = "2048 MiB"

[integrations.search]
required = false
credential = "searxng-main"

[credentials.searxng-main]
client_certificate = "/run/credentials/client.crt"
client_private_key = "/run/credentials/client.key"
private_ca = "/run/credentials/ca.crt"
"#,
        )
        .unwrap();
        valid
            .validate(Path::new("/config/agentLIBRE.toml"))
            .unwrap();
        assert_eq!(
            valid.inference.memory.keep_free_ram,
            Some(BinarySize(12 * GIB))
        );

        assert!(toml::from_str::<HumanConfig>(
            "[chat]\ndefault_function='function:test@1'\n[integrations.search]\ncredential='missing'\n"
        )
        .is_err());
        assert!(
            toml::from_str::<HumanConfig>(
                "[chat]\ndefault_function='function:test@1'\nunknown=true\n"
            )
            .is_err()
        );
        assert!(
            toml::from_str::<HumanConfig>("[chat]\ndefault_function='/tmp/function'\n").is_err()
        );
    }

    #[test]
    fn memory_reserve_accepts_only_positive_binary_units() {
        assert_eq!(parse_binary_size("1 MiB").unwrap(), MIB);
        assert_eq!(parse_binary_size("2 GiB").unwrap(), 2 * GIB);
        for invalid in ["0 GiB", "12 GB", "1024", "1.5 GiB", "-1 GiB"] {
            assert!(parse_binary_size(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn locator_never_guesses_a_bare_package_id() {
        let cwd = std::env::temp_dir();
        assert!(matches!(
            parse_locator("function:agentlibre.chat@^1", &cwd).unwrap(),
            FunctionLocator::Requirement(_)
        ));
        assert!(parse_locator("agentlibre.chat", &cwd).is_err());
    }

    #[test]
    fn repl_decorations_default_is_default_and_strict() {
        let config: HumanConfig =
            toml::from_str("[chat]\ndefault_function='function:test@1'\n").unwrap();
        assert_eq!(config.repl.decorations_default, Decorations::Default);
        let config: HumanConfig = toml::from_str(
            "[chat]\ndefault_function='function:test@1'\n[repl]\ndecorations_default='default'\n",
        )
        .unwrap();
        assert_eq!(config.repl.decorations_default, Decorations::Default);
        let config: HumanConfig = toml::from_str(
            "[chat]\ndefault_function='function:test@1'\n[repl]\ndecorations_default='full'\n",
        )
        .unwrap();
        assert_eq!(config.repl.decorations_default, Decorations::Full);
        assert!(toml::from_str::<HumanConfig>(
            "[chat]\ndefault_function='function:test@1'\n[repl]\ndecorations_default='minimal'\n"
        )
        .is_err());
    }
}
