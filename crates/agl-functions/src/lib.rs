use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use agl_capabilities::CapabilityId;
pub use agl_capabilities::FunctionToolPolicy;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

pub const FUNCTION_SCHEMA: &str = "agentfunction/v1";
pub const SUBAGENT_SCHEMA: &str = "agentlibre/subagent/v1";
pub const FUNCTION_FILE_NAME: &str = "FUNCTION.md";
pub const FUNCTION_SYSTEM_PROMPT_FILE_NAME: &str = "SYSTEM.md";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FunctionToolMode {
    ReadOnly,
    Write,
    Execute,
    Approve,
    Admin,
}

impl FunctionToolMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Write => "write",
            Self::Execute => "execute",
            Self::Approve => "approve",
            Self::Admin => "admin",
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentFunctionFrontMatter {
    pub schema: String,
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub model: Option<FunctionModel>,
    #[serde(default)]
    pub runtime: Option<FunctionRuntime>,
    #[serde(default)]
    pub skills: Option<SelectionBlock>,
    #[serde(default)]
    pub tools: Option<FunctionTools>,
    #[serde(default)]
    pub subagents: Option<SelectionBlock>,
    #[serde(default)]
    pub memory: Option<FunctionMemory>,
    #[serde(default)]
    pub artifacts: Option<FunctionArtifacts>,
    #[serde(default)]
    pub doctor: Option<FunctionDoctor>,
    #[serde(default)]
    pub contracts: Option<FunctionContracts>,
    #[serde(flatten, default)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

impl AgentFunctionFrontMatter {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == FUNCTION_SCHEMA,
            "unsupported function schema `{}`; expected `{FUNCTION_SCHEMA}`",
            self.schema
        );
        validate_function_id("function id", &self.id)?;
        ensure!(
            !self.title.trim().is_empty(),
            "function title cannot be empty"
        );
        validate_extensions("function", &self.extensions)?;
        if let Some(model) = &self.model {
            model.validate()?;
        }
        if let Some(runtime) = &self.runtime {
            runtime.validate()?;
        }
        if let Some(skills) = &self.skills {
            skills.validate("skills.use")?;
        }
        if let Some(tools) = &self.tools {
            tools.validate()?;
        }
        if let Some(subagents) = &self.subagents {
            subagents.validate("subagents.use")?;
            for subagent in &subagents.use_ {
                validate_function_id("subagent id", subagent)?;
            }
        }
        if let Some(memory) = &self.memory {
            memory.validate()?;
        }
        if let Some(artifacts) = &self.artifacts {
            artifacts.validate()?;
        }
        if let Some(doctor) = &self.doctor {
            doctor.validate()?;
        }
        if let Some(contracts) = &self.contracts {
            contracts.validate()?;
        }
        Ok(())
    }

    pub fn model_profile(&self) -> Option<&str> {
        self.model
            .as_ref()
            .and_then(|model| model.profile.as_deref())
    }

    pub fn model_config_path(&self) -> Option<&str> {
        self.model
            .as_ref()
            .and_then(|model| model.config.as_deref())
    }

    pub fn runtime_tool_mode(&self) -> Option<FunctionToolMode> {
        self.runtime.as_ref().and_then(|runtime| runtime.tool_mode)
    }

    pub fn runtime_max_output_tokens(&self) -> Option<u32> {
        self.runtime
            .as_ref()
            .and_then(|runtime| runtime.max_output_tokens)
    }

    pub fn tool_policy(&self) -> Option<FunctionToolPolicy> {
        self.tools.as_ref().map(FunctionTools::to_runtime_policy)
    }

    pub fn selected_skills(&self) -> &[String] {
        self.skills
            .as_ref()
            .map(|skills| skills.use_.as_slice())
            .unwrap_or_default()
    }

    pub fn selected_subagents(&self) -> &[String] {
        self.subagents
            .as_ref()
            .map(|subagents| subagents.use_.as_slice())
            .unwrap_or_default()
    }

    pub fn enables_memory_context(&self) -> bool {
        self.memory
            .as_ref()
            .map(|memory| !memory.read.is_empty())
            .unwrap_or(false)
    }

    pub fn identity_contract(&self) -> Option<RuntimeIdentityContract> {
        self.contracts
            .as_ref()
            .and_then(|contracts| contracts.identity.as_ref())
            .map(FunctionIdentityContract::to_runtime)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FunctionModel {
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub config: Option<String>,
    #[serde(flatten, default)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

impl FunctionModel {
    fn validate(&self) -> Result<()> {
        validate_extensions("model", &self.extensions)?;
        ensure!(
            !(self.profile.is_some() && self.config.is_some()),
            "model.profile and model.config cannot both be set"
        );
        if let Some(profile) = &self.profile {
            validate_function_id("model.profile", profile)?;
        }
        if let Some(config) = &self.config {
            validate_relative_function_file_path("model.config", config)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FunctionRuntime {
    #[serde(default)]
    pub tool_mode: Option<FunctionToolMode>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(flatten, default)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

impl FunctionRuntime {
    fn validate(&self) -> Result<()> {
        validate_extensions("runtime", &self.extensions)?;
        if let Some(max_output_tokens) = self.max_output_tokens {
            ensure!(
                max_output_tokens > 0,
                "runtime.max_output_tokens must be greater than zero"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SelectionBlock {
    #[serde(default, rename = "use")]
    pub use_: Vec<String>,
    #[serde(flatten, default)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

impl SelectionBlock {
    fn validate(&self, field: &str) -> Result<()> {
        validate_extensions(field, &self.extensions)?;
        validate_unique_non_empty(field, &self.use_)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionTools {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(flatten, default)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

impl FunctionTools {
    fn validate(&self) -> Result<()> {
        validate_extensions("tools", &self.extensions)?;
        validate_unique_non_empty("tools.allow", &self.allow)?;
        validate_unique_non_empty("tools.deny", &self.deny)?;
        for id in self.allow.iter().chain(&self.deny) {
            CapabilityId::new(id.clone())
                .with_context(|| format!("invalid function tool capability ID `{id}`"))?;
        }
        Ok(())
    }

    fn to_runtime_policy(&self) -> FunctionToolPolicy {
        FunctionToolPolicy::new(
            self.allow.iter().map(|id| {
                CapabilityId::new(id.clone())
                    .expect("validated function allow capability ID must remain valid")
            }),
            self.deny.iter().map(|id| {
                CapabilityId::new(id.clone())
                    .expect("validated function deny capability ID must remain valid")
            }),
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionMemory {
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
    #[serde(flatten, default)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

impl FunctionMemory {
    fn validate(&self) -> Result<()> {
        validate_extensions("memory", &self.extensions)?;
        validate_unique_non_empty("memory.read", &self.read)?;
        validate_unique_non_empty("memory.write", &self.write)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionArtifacts {
    #[serde(default)]
    pub keep_runs: Option<bool>,
    #[serde(flatten, default)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

impl FunctionArtifacts {
    fn validate(&self) -> Result<()> {
        validate_extensions("artifacts", &self.extensions)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionDoctor {
    #[serde(default)]
    pub smoke_prompt: Option<String>,
    #[serde(flatten, default)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

impl FunctionDoctor {
    fn validate(&self) -> Result<()> {
        validate_extensions("doctor", &self.extensions)?;
        if let Some(prompt) = &self.smoke_prompt {
            ensure!(
                !prompt.trim().is_empty(),
                "doctor.smoke_prompt cannot be empty"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionContracts {
    #[serde(default)]
    pub identity: Option<FunctionIdentityContract>,
    #[serde(flatten, default)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

impl FunctionContracts {
    fn validate(&self) -> Result<()> {
        validate_extensions("contracts", &self.extensions)?;
        if let Some(identity) = &self.identity {
            identity.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityContractMode {
    Off,
    ValidateClaims,
    Require,
}

impl IdentityContractMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::ValidateClaims => "validate_claims",
            Self::Require => "require",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeIdentityContract {
    pub mode: IdentityContractMode,
    pub fields: Vec<String>,
    pub repair: bool,
    pub max_repair_attempts: u32,
}

impl RuntimeIdentityContract {
    pub fn function_default() -> Self {
        Self {
            mode: IdentityContractMode::ValidateClaims,
            fields: default_identity_fields(),
            repair: true,
            max_repair_attempts: 1,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.mode != IdentityContractMode::Off
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionIdentityContract {
    #[serde(default)]
    pub mode: Option<IdentityContractMode>,
    #[serde(default)]
    pub fields: Vec<String>,
    #[serde(default)]
    pub repair: Option<bool>,
    #[serde(default)]
    pub max_repair_attempts: Option<u32>,
    #[serde(flatten, default)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

impl FunctionIdentityContract {
    fn validate(&self) -> Result<()> {
        validate_extensions("contracts.identity", &self.extensions)?;
        validate_unique_non_empty("contracts.identity.fields", &self.fields)?;
        for field in &self.fields {
            ensure!(
                is_valid_identity_field(field),
                "contracts.identity.fields contains unsupported field `{field}`"
            );
        }
        Ok(())
    }

    fn to_runtime(&self) -> RuntimeIdentityContract {
        RuntimeIdentityContract {
            mode: self.mode.unwrap_or(IdentityContractMode::ValidateClaims),
            fields: if self.fields.is_empty() {
                default_identity_fields()
            } else {
                self.fields.clone()
            },
            repair: self.repair.unwrap_or(true),
            max_repair_attempts: self.max_repair_attempts.unwrap_or(1),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubagentFrontMatter {
    pub schema: String,
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub model: Option<SubagentModel>,
    #[serde(default)]
    pub tools: Option<SubagentTools>,
    #[serde(default)]
    pub skills: Option<SelectionBlock>,
    #[serde(default)]
    pub memory: Option<FunctionMemory>,
    #[serde(default)]
    pub limits: Option<SubagentLimits>,
    #[serde(flatten, default)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

impl SubagentFrontMatter {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == SUBAGENT_SCHEMA,
            "unsupported subagent schema `{}`; expected `{SUBAGENT_SCHEMA}`",
            self.schema
        );
        validate_function_id("subagent id", &self.id)?;
        ensure!(
            !self.title.trim().is_empty(),
            "subagent title cannot be empty"
        );
        validate_extensions("subagent", &self.extensions)?;
        if let Some(model) = &self.model {
            model.validate()?;
        }
        if let Some(tools) = &self.tools {
            tools.validate()?;
        }
        if let Some(skills) = &self.skills {
            skills.validate("subagent.skills.use")?;
        }
        if let Some(memory) = &self.memory {
            memory.validate()?;
        }
        if let Some(limits) = &self.limits {
            limits.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubagentModel {
    #[serde(default)]
    pub inherit: Option<bool>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(flatten, default)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

impl SubagentModel {
    fn validate(&self) -> Result<()> {
        validate_extensions("subagent.model", &self.extensions)?;
        if let Some(profile) = &self.profile {
            validate_function_id("subagent.model.profile", profile)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SubagentTools {
    #[serde(default)]
    pub mode: Option<FunctionToolMode>,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(flatten, default)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

impl SubagentTools {
    fn validate(&self) -> Result<()> {
        validate_extensions("subagent.tools", &self.extensions)?;
        validate_unique_non_empty("subagent.tools.allow", &self.allow)?;
        validate_unique_non_empty("subagent.tools.deny", &self.deny)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SubagentLimits {
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(flatten, default)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

impl SubagentLimits {
    fn validate(&self) -> Result<()> {
        validate_extensions("subagent.limits", &self.extensions)?;
        if let Some(max_turns) = self.max_turns {
            ensure!(
                max_turns > 0,
                "subagent.limits.max_turns must be greater than zero"
            );
        }
        if let Some(max_output_tokens) = self.max_output_tokens {
            ensure!(
                max_output_tokens > 0,
                "subagent.limits.max_output_tokens must be greater than zero"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MarkdownSection {
    pub title: String,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LoadedFunction {
    pub locator: FunctionLocator,
    pub front_matter: AgentFunctionFrontMatter,
    pub system_prompt_path: PathBuf,
    pub system_prompt: String,
    pub system_prompt_sections: Vec<MarkdownSection>,
    pub inference_config_path: Option<PathBuf>,
    pub inference_config_toml: Option<String>,
    pub subagents: Vec<LoadedSubagent>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LoadedSubagent {
    pub path: PathBuf,
    pub front_matter: SubagentFrontMatter,
    pub body: String,
    pub sections: Vec<MarkdownSection>,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RuntimeSubagent {
    pub id: String,
    pub title: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RuntimeFunction {
    pub reference: String,
    pub source: FunctionSource,
    pub path: PathBuf,
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub model_profile: Option<String>,
    pub profile_path: Option<PathBuf>,
    pub inference_config_path: Option<PathBuf>,
    pub inference_config_toml: Option<String>,
    pub tool_mode: Option<FunctionToolMode>,
    pub tool_policy: Option<FunctionToolPolicy>,
    pub max_output_tokens: Option<u32>,
    pub skills: Vec<String>,
    pub memory_enabled: bool,
    pub subagents: Vec<RuntimeSubagent>,
    pub system_prompt_path: PathBuf,
    pub identity_contract: Option<RuntimeIdentityContract>,
    pub context: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FunctionStatusReport {
    pub reference: String,
    pub state: String,
    pub source: Option<String>,
    pub path: Option<PathBuf>,
    pub system_prompt_path: Option<PathBuf>,
    pub id: Option<String>,
    pub title: Option<String>,
    pub profile: Option<String>,
    pub profile_path: Option<PathBuf>,
    pub inference_config_path: Option<PathBuf>,
    pub inference_config_embedded: bool,
    pub inference_model_path: Option<PathBuf>,
    pub inference_model_exists: Option<bool>,
    pub tool_policy: Option<FunctionToolPolicy>,
    pub skills: Vec<String>,
    pub subagents: Vec<RuntimeSubagent>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
    pub next_steps: Vec<String>,
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

pub fn load_function(locator: FunctionLocator) -> Result<LoadedFunction> {
    let builtin = if locator.source == FunctionSource::Builtin {
        Some(resolve_builtin_function(&locator.reference)?)
    } else {
        None
    };
    let content = if let Some(function) = builtin {
        function
            .function_md
            .text()
            .with_context(|| format!("builtin function `{}` is not UTF-8", function.id))?
            .to_string()
    } else {
        std::fs::read_to_string(&locator.path)
            .with_context(|| format!("failed to read function {}", locator.path.display()))?
    };
    let (front_matter, body) = parse_function_document(&content)
        .with_context(|| format!("failed to parse function {}", locator.path.display()))?;
    front_matter.validate()?;
    ensure!(
        body.trim().is_empty(),
        "FUNCTION.md body is not supported; put system instructions in SYSTEM.md"
    );
    if !matches!(
        locator.source,
        FunctionSource::Explicit | FunctionSource::Builtin
    ) {
        let directory_id = locator
            .root_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        ensure!(
            directory_id == front_matter.id,
            "function id `{}` does not match directory `{directory_id}`",
            front_matter.id
        );
    }
    let subagents = load_declared_subagents(&locator.root_dir, &front_matter)?;
    let (system_prompt_path, system_prompt) =
        load_function_system_prompt(&locator.root_dir, builtin)?;
    let system_prompt_sections = markdown_sections(&system_prompt);
    let (inference_config_path, inference_config_toml) =
        load_function_inference_config(&locator.root_dir, &front_matter, builtin)?;
    Ok(LoadedFunction {
        locator,
        front_matter,
        system_prompt_path,
        system_prompt,
        system_prompt_sections,
        inference_config_path,
        inference_config_toml,
        subagents,
    })
}

pub fn resolve_runtime_function(
    reference: &str,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
) -> Result<RuntimeFunction> {
    resolve_runtime_function_with_profile_policy(reference, workspace_root, config_dir, true)
}

pub fn resolve_runtime_function_allow_missing_profile(
    reference: &str,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
) -> Result<RuntimeFunction> {
    resolve_runtime_function_with_profile_policy(reference, workspace_root, config_dir, false)
}

fn resolve_runtime_function_with_profile_policy(
    reference: &str,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
    require_profile: bool,
) -> Result<RuntimeFunction> {
    let locator = resolve_function_reference(reference, &workspace_root, &config_dir)?;
    let loaded = load_function(locator)?;
    let profile_path = if let Some(profile) = loaded.front_matter.model_profile() {
        let resolution = resolve_profile(profile, &workspace_root, &config_dir)?;
        match resolution.selected_path {
            Some(path) => Some(path),
            None if require_profile => {
                bail!(
                    "inference profile `{profile}` not found; checked {}",
                    join_paths(&resolution.candidates)
                );
            }
            None => None,
        }
    } else {
        None
    };
    Ok(runtime_function_from_loaded(loaded, profile_path))
}

pub fn function_status(
    reference: &str,
    workspace_root: impl AsRef<Path>,
    config_dir: impl AsRef<Path>,
) -> FunctionStatusReport {
    let workspace_root = workspace_root.as_ref();
    let config_dir = config_dir.as_ref();
    let mut report = FunctionStatusReport {
        reference: reference.to_string(),
        state: "invalid".to_string(),
        source: None,
        path: None,
        system_prompt_path: None,
        id: None,
        title: None,
        profile: None,
        profile_path: None,
        inference_config_path: None,
        inference_config_embedded: false,
        inference_model_path: None,
        inference_model_exists: None,
        tool_policy: None,
        skills: Vec::new(),
        subagents: Vec::new(),
        warnings: Vec::new(),
        errors: Vec::new(),
        next_steps: Vec::new(),
    };

    let locator = match resolve_function_reference(reference, workspace_root, config_dir) {
        Ok(locator) => locator,
        Err(err) => {
            report.errors.push(format!("{err:#}"));
            if !looks_like_path(reference) && is_valid_function_id(reference) {
                report
                    .next_steps
                    .push(format!("agl function init {reference} --workspace"));
            }
            return report;
        }
    };
    report.source = Some(locator.source.as_str().to_string());
    report.path = Some(locator.path.clone());

    let loaded = match load_function(locator) {
        Ok(loaded) => loaded,
        Err(err) => {
            report.errors.push(format!("{err:#}"));
            return report;
        }
    };
    report.id = Some(loaded.front_matter.id.clone());
    report.title = Some(loaded.front_matter.title.clone());
    report.system_prompt_path = Some(loaded.system_prompt_path.clone());
    report.inference_config_path = loaded.inference_config_path.clone();
    report.inference_config_embedded =
        loaded.locator.source == FunctionSource::Builtin && loaded.inference_config_toml.is_some();
    if let Some(config_toml) = loaded.inference_config_toml.as_deref() {
        let source_name = loaded
            .inference_config_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<function inference config>".to_string());
        match agl_config::load_local_inference_config_from_str(&source_name, config_toml) {
            Ok(config) => {
                report.inference_model_path = Some(config.backend.model.clone());
                let model_exists = config.backend.model.exists();
                report.inference_model_exists = Some(model_exists);
                if !model_exists {
                    report.warnings.push(format!(
                        "function model file not found: {}",
                        config.backend.model.display()
                    ));
                    report.next_steps.push(format!(
                        "install GGUF model or edit function inference config: {}",
                        source_name
                    ));
                }
            }
            Err(err) => report.errors.push(format!("{err:#}")),
        }
    }
    report.skills = loaded.front_matter.selected_skills().to_vec();
    report.tool_policy = loaded.front_matter.tool_policy();
    report.subagents = loaded
        .subagents
        .iter()
        .map(|subagent| RuntimeSubagent {
            id: subagent.front_matter.id.clone(),
            title: subagent.front_matter.title.clone(),
            path: subagent.path.clone(),
        })
        .collect();

    if let Some(profile) = loaded.front_matter.model_profile() {
        report.profile = Some(profile.to_string());
        match resolve_profile(profile, workspace_root, config_dir) {
            Ok(resolution) => {
                report.profile_path = resolution.selected_path.clone();
                match resolution.selected_path {
                    Some(path) if path.is_file() => {}
                    Some(path) => report.errors.push(format!(
                        "inference profile `{profile}` not found: {}",
                        path.display()
                    )),
                    None => report.errors.push(format!(
                        "inference profile `{profile}` not found; checked {}",
                        join_paths(&resolution.candidates)
                    )),
                }
            }
            Err(err) => report.errors.push(format!("{err:#}")),
        }
    }

    if report.errors.is_empty() {
        report.state = if report.warnings.is_empty() {
            "ok".to_string()
        } else {
            "warning".to_string()
        };
    }
    report
}

pub fn render_function_context(function: &LoadedFunction) -> String {
    let mut content = String::new();
    content.push_str("<agentlibre_function_context>\n");
    content.push_str("schema: ");
    content.push_str(FUNCTION_SCHEMA);
    content.push('\n');
    content.push_str("id: ");
    content.push_str(&function.front_matter.id);
    content.push('\n');
    content.push_str("title: ");
    content.push_str(&function.front_matter.title);
    content.push('\n');
    if let Some(description) = &function.front_matter.description {
        content.push_str("description: ");
        content.push_str(description.trim());
        content.push('\n');
    }
    if let Some(profile) = function.front_matter.model_profile() {
        content.push_str("model_profile: ");
        content.push_str(profile);
        content.push('\n');
    }
    let skills = function.front_matter.selected_skills();
    if !skills.is_empty() {
        content.push_str("skills: ");
        content.push_str(&skills.join(", "));
        content.push('\n');
    }
    if !function.subagents.is_empty() {
        content.push_str("\nAvailable subagents:\n");
        for subagent in &function.subagents {
            content.push_str("- ");
            content.push_str(&subagent.front_matter.id);
            content.push_str(": ");
            content.push_str(&subagent.front_matter.title);
            content.push('\n');
        }
    }
    content.push_str("\nFunction system prompt:\n");
    content.push_str(function.system_prompt.trim());
    content.push('\n');
    for subagent in &function.subagents {
        content.push_str("\n<agentlibre_subagent_context id=\"");
        content.push_str(&subagent.front_matter.id);
        content.push_str("\" title=\"");
        content.push_str(&subagent.front_matter.title);
        content.push_str("\">\n");
        content.push_str(subagent.body.trim());
        content.push_str("\n</agentlibre_subagent_context>\n");
    }
    content.push_str("</agentlibre_function_context>\n");
    content
}

pub fn validate_function_id(label: &str, value: &str) -> Result<()> {
    ensure!(
        is_valid_function_id(value),
        "{label} must use lowercase ASCII letters, digits, hyphens, underscores, or dots: {value}"
    );
    Ok(())
}

fn validate_relative_function_file_path(label: &str, value: &str) -> Result<()> {
    ensure!(!value.trim().is_empty(), "{label} cannot be empty");
    ensure!(!value.contains('\0'), "{label} cannot contain NUL");
    let path = Path::new(value);
    ensure!(!path.is_absolute(), "{label} cannot be absolute: {value}");
    for component in path.components() {
        match component {
            std::path::Component::Normal(segment) => {
                ensure!(segment != ".git", "{label} cannot enter .git: {value}");
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                bail!("{label} cannot contain parent traversal: {value}");
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                bail!("{label} cannot be absolute: {value}");
            }
        }
    }
    Ok(())
}

pub fn is_valid_function_id(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
}

fn default_identity_fields() -> Vec<String> {
    ["function", "skills", "subagents"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn is_valid_identity_field(field: &str) -> bool {
    matches!(field, "function" | "skills" | "subagents" | "model_profile")
}

fn runtime_function_from_loaded(
    loaded: LoadedFunction,
    profile_path: Option<PathBuf>,
) -> RuntimeFunction {
    RuntimeFunction {
        reference: loaded.locator.reference.clone(),
        source: loaded.locator.source,
        path: loaded.locator.path.clone(),
        id: loaded.front_matter.id.clone(),
        title: loaded.front_matter.title.clone(),
        description: loaded.front_matter.description.clone(),
        model_profile: loaded.front_matter.model_profile().map(str::to_string),
        profile_path: profile_path.clone(),
        inference_config_path: loaded
            .inference_config_path
            .clone()
            .or_else(|| profile_path.clone()),
        inference_config_toml: loaded.inference_config_toml.clone(),
        tool_mode: loaded.front_matter.runtime_tool_mode(),
        tool_policy: loaded.front_matter.tool_policy(),
        max_output_tokens: loaded.front_matter.runtime_max_output_tokens(),
        skills: loaded.front_matter.selected_skills().to_vec(),
        memory_enabled: loaded.front_matter.enables_memory_context(),
        system_prompt_path: loaded.system_prompt_path.clone(),
        identity_contract: loaded.front_matter.identity_contract(),
        subagents: loaded
            .subagents
            .iter()
            .map(|subagent| RuntimeSubagent {
                id: subagent.front_matter.id.clone(),
                title: subagent.front_matter.title.clone(),
                path: subagent.path.clone(),
            })
            .collect(),
        context: render_function_context(&loaded),
    }
}

fn collect_function_entries(
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

fn collect_builtin_function_entries(entries: &mut Vec<FunctionListEntry>) {
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

fn load_declared_subagents(
    function_root: &Path,
    front_matter: &AgentFunctionFrontMatter,
) -> Result<Vec<LoadedSubagent>> {
    let mut subagents = Vec::new();
    for subagent_id in front_matter.selected_subagents() {
        validate_function_id("subagent id", subagent_id)?;
        let path = function_root
            .join("subagents")
            .join(format!("{subagent_id}.md"));
        ensure!(
            path.starts_with(function_root),
            "subagent path escapes function root: {}",
            path.display()
        );
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read subagent {}", path.display()))?;
        let (front_matter, body) = parse_subagent_document(&content)
            .with_context(|| format!("failed to parse subagent {}", path.display()))?;
        front_matter.validate()?;
        ensure!(
            front_matter.id == *subagent_id,
            "subagent id `{}` does not match declared id `{subagent_id}`",
            front_matter.id
        );
        let file_stem = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        ensure!(
            file_stem == front_matter.id,
            "subagent id `{}` does not match file `{file_stem}`",
            front_matter.id
        );
        let sections = markdown_sections(&body);
        subagents.push(LoadedSubagent {
            path,
            front_matter,
            body,
            sections,
        });
    }
    Ok(subagents)
}

fn load_function_system_prompt(
    function_root: &Path,
    builtin: Option<&'static agl_assets::BuiltinFunction>,
) -> Result<(PathBuf, String)> {
    if let Some(function) = builtin {
        let content = function.system_prompt.text().with_context(|| {
            format!(
                "builtin function `{}` system prompt is not UTF-8",
                function.id
            )
        })?;
        ensure!(
            !content.trim().is_empty(),
            "function system prompt cannot be empty: {}",
            function.system_prompt.source_path
        );
        return Ok((
            PathBuf::from(function.system_prompt.source_path),
            content.to_string(),
        ));
    }
    let path = resolve_function_relative_path(function_root, FUNCTION_SYSTEM_PROMPT_FILE_NAME)?;
    ensure!(
        path.is_file(),
        "function system prompt file not found: {}",
        path.display()
    );
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read function system prompt {}", path.display()))?;
    ensure!(
        !content.trim().is_empty(),
        "function system prompt cannot be empty: {}",
        path.display()
    );

    Ok((path, content))
}

fn load_function_inference_config(
    function_root: &Path,
    front_matter: &AgentFunctionFrontMatter,
    builtin: Option<&'static agl_assets::BuiltinFunction>,
) -> Result<(Option<PathBuf>, Option<String>)> {
    let Some(relative) = front_matter.model_config_path() else {
        return Ok((None, None));
    };
    if let Some(function) = builtin {
        ensure!(
            relative == "inference.toml",
            "builtin function `{}` can only load model.config: inference.toml",
            function.id
        );
        let content = function.inference_config.text().with_context(|| {
            format!(
                "builtin function `{}` inference config is not UTF-8",
                function.id
            )
        })?;
        ensure!(
            !content.trim().is_empty(),
            "function inference config cannot be empty: {}",
            function.inference_config.source_path
        );
        return Ok((
            Some(PathBuf::from(function.inference_config.source_path)),
            Some(content.to_string()),
        ));
    }

    let path = resolve_function_relative_path(function_root, relative)?;
    ensure!(
        path.is_file(),
        "function inference config file not found: {}",
        path.display()
    );
    let content = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "failed to read function inference config {}",
            path.display()
        )
    })?;
    ensure!(
        !content.trim().is_empty(),
        "function inference config cannot be empty: {}",
        path.display()
    );
    Ok((Some(path), Some(content)))
}

fn resolve_builtin_function(reference: &str) -> Result<&'static agl_assets::BuiltinFunction> {
    agl_assets::builtin_function(reference)
        .with_context(|| format!("builtin function `{reference}` is not embedded"))
}

fn resolve_function_relative_path(function_root: &Path, relative: &str) -> Result<PathBuf> {
    validate_relative_function_file_path("function file path", relative)?;
    let path = function_root.join(relative);
    ensure!(
        path.starts_with(function_root),
        "function file path escapes function root: {}",
        path.display()
    );
    Ok(path)
}

fn parse_function_document(content: &str) -> Result<(AgentFunctionFrontMatter, String)> {
    let (yaml, body) = split_front_matter(content)?;
    let front_matter = serde_yaml::from_str::<AgentFunctionFrontMatter>(&yaml)
        .context("failed to parse function YAML front matter")?;
    Ok((front_matter, body))
}

fn parse_subagent_document(content: &str) -> Result<(SubagentFrontMatter, String)> {
    let (yaml, body) = split_front_matter(content)?;
    let front_matter = serde_yaml::from_str::<SubagentFrontMatter>(&yaml)
        .context("failed to parse subagent YAML front matter")?;
    Ok((front_matter, body))
}

fn split_front_matter(content: &str) -> Result<(String, String)> {
    let mut lines = content.lines();
    let Some(first) = lines.next() else {
        bail!("document is empty");
    };
    ensure!(
        first.trim_end_matches('\r') == "---",
        "document must start with YAML front matter"
    );

    let mut yaml = String::new();
    let mut closed = false;
    for line in &mut lines {
        if line.trim_end_matches('\r') == "---" {
            closed = true;
            break;
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    ensure!(closed, "YAML front matter is not closed");
    let body = lines.collect::<Vec<_>>().join("\n");
    Ok((yaml, body))
}

fn markdown_sections(body: &str) -> Vec<MarkdownSection> {
    let mut sections = Vec::new();
    let mut current_title: Option<String> = None;
    let mut current_content = String::new();
    for line in body.lines() {
        if let Some(title) = line.strip_prefix("# ") {
            if let Some(title) = current_title.take() {
                sections.push(MarkdownSection {
                    title,
                    content: current_content.trim().to_string(),
                });
                current_content.clear();
            }
            current_title = Some(title.trim().to_string());
        } else if current_title.is_some() {
            current_content.push_str(line);
            current_content.push('\n');
        }
    }
    if let Some(title) = current_title {
        sections.push(MarkdownSection {
            title,
            content: current_content.trim().to_string(),
        });
    }
    sections
}

fn validate_extensions(
    label: &str,
    extensions: &BTreeMap<String, serde_yaml::Value>,
) -> Result<()> {
    for key in extensions.keys() {
        ensure!(
            key.starts_with("x-"),
            "unknown {label} front matter field `{key}`"
        );
    }
    Ok(())
}

fn validate_unique_non_empty(field: &str, values: &[String]) -> Result<()> {
    let mut seen = BTreeSet::new();
    for value in values {
        ensure!(
            !value.trim().is_empty(),
            "{field} cannot contain empty values"
        );
        ensure!(
            seen.insert(value),
            "{field} contains duplicate value `{value}`"
        );
    }
    Ok(())
}

fn looks_like_path(reference: &str) -> bool {
    reference.contains('/')
        || reference.contains('\\')
        || reference.ends_with(".md")
        || reference.starts_with('.')
}

fn normalize_function_file_path(path: PathBuf) -> PathBuf {
    if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
        path
    } else {
        path.join(FUNCTION_FILE_NAME)
    }
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_function_document() {
        let content = r#"---
schema: agentfunction/v1
id: coding
title: Coding
model:
  config: inference.toml
runtime:
  tool_mode: write
contracts:
  identity:
    mode: require
    fields:
      - function
      - skills
    repair: true
    max_repair_attempts: 2
skills:
  use:
    - repo-status
---
"#;

        let (front_matter, body) = parse_function_document(content).unwrap();

        assert_eq!(front_matter.id, "coding");
        assert_eq!(front_matter.model_profile(), None);
        assert_eq!(front_matter.model_config_path(), Some("inference.toml"));
        assert_eq!(
            front_matter.runtime_tool_mode(),
            Some(FunctionToolMode::Write)
        );
        assert_eq!(
            front_matter.identity_contract(),
            Some(RuntimeIdentityContract {
                mode: IdentityContractMode::Require,
                fields: vec!["function".to_string(), "skills".to_string()],
                repair: true,
                max_repair_attempts: 2,
            })
        );
        assert_eq!(front_matter.selected_skills(), ["repo-status"]);
        assert!(body.trim().is_empty());
    }

    #[test]
    fn runtime_function_preserves_function_tool_policy_states() {
        fn policy(allow: &[&str], deny: &[&str]) -> FunctionToolPolicy {
            FunctionToolPolicy::new(
                allow
                    .iter()
                    .map(|id| CapabilityId::new(*id).expect("test capability ID is valid")),
                deny.iter()
                    .map(|id| CapabilityId::new(*id).expect("test capability ID is valid")),
            )
        }

        struct Case {
            name: &'static str,
            tools_yaml: &'static str,
            expected: Option<FunctionToolPolicy>,
        }

        let cases = [
            Case {
                name: "absent",
                tools_yaml: "",
                expected: None,
            },
            Case {
                name: "present-empty",
                tools_yaml: "tools: {}\n",
                expected: Some(FunctionToolPolicy::default()),
            },
            Case {
                name: "allow-and-deny",
                tools_yaml: "tools:\n  allow:\n    - fs.read\n    - repo.status\n  deny:\n    - repo.status\n",
                expected: Some(policy(&["fs.read", "repo.status"], &["repo.status"])),
            },
            Case {
                name: "deny-only",
                tools_yaml: "tools:\n  deny:\n    - fs.edit\n",
                expected: Some(policy(&[], &["fs.edit"])),
            },
        ];

        let root =
            std::env::temp_dir().join(format!("agl-functions-tool-policy-{}", std::process::id()));
        let workspace = root.join("workspace");
        let config = root.join("config");
        let _ = std::fs::remove_dir_all(&root);

        for (index, case) in cases.iter().enumerate() {
            let id = format!("policy-{index}");
            let function_root = workspace.join(".agl/functions").join(&id);
            std::fs::create_dir_all(&function_root).unwrap();
            std::fs::write(
                function_root.join(FUNCTION_FILE_NAME),
                format!(
                    "---\nschema: agentfunction/v1\nid: {id}\ntitle: Policy {index}\n{}---\n",
                    case.tools_yaml
                ),
            )
            .unwrap();
            std::fs::write(
                function_root.join(FUNCTION_SYSTEM_PROMPT_FILE_NAME),
                "Test policy.\n",
            )
            .unwrap();

            let runtime =
                resolve_runtime_function_allow_missing_profile(&id, &workspace, &config).unwrap();
            assert_eq!(runtime.tool_policy, case.expected, "{}", case.name);

            let report = function_status(&id, &workspace, &config);
            assert_eq!(report.tool_policy, case.expected, "{} status", case.name);

            let serialized = serde_yaml::to_value(&runtime).unwrap();
            let serialized_policy = serialized
                .get("tool_policy")
                .unwrap_or_else(|| panic!("{} evidence omitted tool_policy", case.name))
                .clone();
            let round_trip: Option<FunctionToolPolicy> =
                serde_yaml::from_value(serialized_policy).unwrap();
            assert_eq!(round_trip, case.expected, "{} evidence", case.name);
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_model_profile_and_config_together() {
        let content = r#"---
schema: agentfunction/v1
id: coding
title: Coding
model:
  profile: local
  config: inference.toml
---
"#;

        let (front_matter, _) = parse_function_document(content).unwrap();
        let err = front_matter.validate().unwrap_err();

        assert!(
            err.to_string()
                .contains("model.profile and model.config cannot both be set")
        );
    }

    #[test]
    fn rejects_unknown_fields_without_extension_prefix() {
        let content = r#"---
schema: agentfunction/v1
id: coding
title: Coding
unknown: true
---
"#;

        let (front_matter, _) = parse_function_document(content).unwrap();
        let err = front_matter.validate().unwrap_err();

        assert!(
            err.to_string()
                .contains("unknown function front matter field")
        );
    }

    #[test]
    fn renders_subagent_context() {
        let root =
            std::env::temp_dir().join(format!("agl-functions-render-{}", std::process::id()));
        let function_root = root.join("coding");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(function_root.join("subagents")).unwrap();
        std::fs::write(
            function_root.join(FUNCTION_FILE_NAME),
            r#"---
schema: agentfunction/v1
id: coding
title: Coding
subagents:
  use:
    - reviewer
---
"#,
        )
        .unwrap();
        std::fs::write(
            function_root.join(FUNCTION_SYSTEM_PROMPT_FILE_NAME),
            "Code.\n",
        )
        .unwrap();
        std::fs::write(
            function_root.join("subagents").join("reviewer.md"),
            r#"---
schema: agentlibre/subagent/v1
id: reviewer
title: Reviewer
---

# Mission

Review.
"#,
        )
        .unwrap();
        let locator = FunctionLocator {
            reference: "coding".to_string(),
            source: FunctionSource::Workspace,
            path: function_root.join(FUNCTION_FILE_NAME),
            root_dir: function_root,
        };

        let loaded = load_function(locator).unwrap();
        let context = render_function_context(&loaded);

        assert!(context.contains("id: coding"));
        assert!(context.contains("Function system prompt"));
        assert!(context.contains("Code."));
        assert!(context.contains("Available subagents"));
        assert!(context.contains("Review."));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn runtime_function_can_allow_missing_profile_when_config_overrides() {
        let root = std::env::temp_dir().join(format!(
            "agl-functions-missing-profile-{}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        let config = root.join("config");
        let function_root = workspace.join(".agl/functions/coding");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&function_root).unwrap();
        std::fs::write(
            function_root.join(FUNCTION_FILE_NAME),
            r#"---
schema: agentfunction/v1
id: coding
title: Coding
model:
  profile: missing-profile
---
"#,
        )
        .unwrap();
        std::fs::write(
            function_root.join(FUNCTION_SYSTEM_PROMPT_FILE_NAME),
            "Code.\n",
        )
        .unwrap();

        let missing = resolve_runtime_function("coding", &workspace, &config).unwrap_err();
        assert!(missing.to_string().contains("missing-profile"));

        let allowed =
            resolve_runtime_function_allow_missing_profile("coding", &workspace, &config).unwrap();
        assert_eq!(allowed.model_profile.as_deref(), Some("missing-profile"));
        assert_eq!(allowed.profile_path, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolves_builtin_gemma4_function_with_embedded_config() {
        let root = std::env::temp_dir().join(format!(
            "agl-functions-builtin-gemma4-{}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        let config = root.join("config");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&workspace).unwrap();

        let locator = resolve_function_reference("gemma4-12b", &workspace, &config).unwrap();
        assert_eq!(locator.source, FunctionSource::Builtin);

        let loaded = load_function(locator).unwrap();
        assert_eq!(loaded.front_matter.id, "gemma4-12b");
        assert_eq!(
            loaded.inference_config_path.as_deref(),
            Some(Path::new("assets/functions/gemma4-12b/inference.toml"))
        );
        assert!(
            loaded
                .inference_config_toml
                .as_deref()
                .unwrap()
                .contains("tool_call_format = \"gemma_function_call\"")
        );

        let runtime = resolve_runtime_function("gemma4-12b", &workspace, &config).unwrap();
        assert_eq!(runtime.source, FunctionSource::Builtin);
        assert_eq!(runtime.model_profile, None);
        assert_eq!(runtime.profile_path, None);
        assert!(runtime.inference_config_toml.is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn lists_builtin_functions() {
        let root =
            std::env::temp_dir().join(format!("agl-functions-list-builtin-{}", std::process::id()));
        let workspace = root.join("workspace");
        let config = root.join("config");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&workspace).unwrap();

        let functions = list_functions(&workspace, &config).unwrap();

        assert!(functions.iter().any(|function| {
            function.id == "gemma4-12b"
                && function.source == FunctionSource::Builtin
                && function.valid
        }));
        assert!(functions.iter().any(|function| {
            function.id == "gemma4-26b"
                && function.source == FunctionSource::Builtin
                && function.valid
        }));
        assert!(functions.iter().any(|function| {
            function.id == "gemma4-31b"
                && function.source == FunctionSource::Builtin
                && function.valid
        }));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_function_body_in_manifest() {
        let root = std::env::temp_dir().join(format!("agl-functions-body-{}", std::process::id()));
        let function_root = root.join("coding");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&function_root).unwrap();
        std::fs::write(
            function_root.join(FUNCTION_SYSTEM_PROMPT_FILE_NAME),
            "Code.\n",
        )
        .unwrap();
        std::fs::write(
            function_root.join(FUNCTION_FILE_NAME),
            r#"---
schema: agentfunction/v1
id: coding
title: Coding
---

# Mission

Code.
"#,
        )
        .unwrap();
        let locator = FunctionLocator {
            reference: "coding".to_string(),
            source: FunctionSource::Workspace,
            path: function_root.join(FUNCTION_FILE_NAME),
            root_dir: function_root,
        };

        let err = load_function(locator).unwrap_err();

        assert!(
            err.to_string()
                .contains("FUNCTION.md body is not supported")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_prompt_field_in_manifest() {
        let content = r#"---
schema: agentfunction/v1
id: coding
title: Coding
prompt:
  system: SYSTEM.md
---
"#;

        let (front_matter, _) = parse_function_document(content).unwrap();
        let err = front_matter.validate().unwrap_err();

        assert!(
            err.to_string()
                .contains("unknown function front matter field `prompt`")
        );
    }

    #[test]
    fn rejects_missing_system_prompt_file() {
        let root =
            std::env::temp_dir().join(format!("agl-functions-system-{}", std::process::id()));
        let function_root = root.join("coding");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&function_root).unwrap();
        std::fs::write(
            function_root.join(FUNCTION_FILE_NAME),
            r#"---
schema: agentfunction/v1
id: coding
title: Coding
---
"#,
        )
        .unwrap();
        let locator = FunctionLocator {
            reference: "coding".to_string(),
            source: FunctionSource::Workspace,
            path: function_root.join(FUNCTION_FILE_NAME),
            root_dir: function_root,
        };

        let err = load_function(locator).unwrap_err();

        assert!(
            err.to_string()
                .contains("function system prompt file not found")
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
