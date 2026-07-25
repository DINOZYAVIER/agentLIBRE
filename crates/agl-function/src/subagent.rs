use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use agl_capabilities::CapabilityId;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::loader::{LoadedFunction, LoadedSubagent, markdown_sections, parse_subagent_document};
use crate::locator::{FunctionSource, resolve_profile};
use crate::manifest::{
    AgentFunctionFrontMatter, FunctionDelegationBudget, FunctionMemory, FunctionToolMode,
    FunctionToolPolicy, SUBAGENT_SCHEMA, SelectionBlock,
};
use crate::validation::{
    join_paths, sha256_bytes, sha256_text, validate_extensions, validate_function_id,
    validate_unique_non_empty,
};
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubagentFrontMatter {
    pub schema: String,
    pub id: String,
    pub title: String,
    pub description: String,
    pub model: SubagentModel,
    pub tools: SubagentTools,
    #[serde(default)]
    pub skills: Option<SelectionBlock>,
    #[serde(default)]
    pub memory: Option<FunctionMemory>,
    pub subagents: SelectionBlock,
    pub limits: SubagentLimits,
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
        ensure!(
            !self.description.trim().is_empty(),
            "subagent description cannot be empty"
        );
        validate_extensions("subagent", &self.extensions)?;
        self.model.validate()?;
        self.tools.validate()?;
        if let Some(skills) = &self.skills {
            skills.validate("subagent.skills.use")?;
        }
        if let Some(memory) = &self.memory {
            memory.validate()?;
            ensure!(
                memory
                    .read
                    .iter()
                    .chain(&memory.write)
                    .all(|scope| scope == "user"),
                "subagent memory scopes currently support only `user`"
            );
        }
        self.subagents.validate("subagent.subagents.use")?;
        for subagent in &self.subagents.use_ {
            validate_function_id("child subagent id", subagent)?;
        }
        self.limits.validate()?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubagentModel {
    #[serde(default)]
    pub inherit: bool,
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
        ensure!(
            self.inherit ^ self.profile.is_some(),
            "subagent.model requires exactly one of inherit=true or profile"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SubagentTools {
    pub mode: FunctionToolMode,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    #[serde(flatten, default)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

impl SubagentTools {
    fn validate(&self) -> Result<()> {
        validate_extensions("subagent.tools", &self.extensions)?;
        validate_unique_non_empty("subagent.tools.allow", &self.allow)?;
        validate_unique_non_empty("subagent.tools.deny", &self.deny)?;
        for id in self.allow.iter().chain(&self.deny) {
            CapabilityId::new(id.clone())
                .with_context(|| format!("invalid subagent tool capability ID `{id}`"))?;
        }
        Ok(())
    }

    pub(crate) fn to_runtime_policy(&self) -> FunctionToolPolicy {
        FunctionToolPolicy::new(
            self.allow.iter().map(|id| {
                CapabilityId::new(id.clone())
                    .expect("validated subagent allow capability ID must remain valid")
            }),
            self.deny.iter().map(|id| {
                CapabilityId::new(id.clone())
                    .expect("validated subagent deny capability ID must remain valid")
            }),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentLimits {
    pub max_model_attempts: u32,
    pub max_output_tokens: u64,
    pub max_capability_calls: u32,
    pub timeout_seconds: u64,
}

impl SubagentLimits {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.max_model_attempts > 0,
            "subagent.limits.max_model_attempts must be greater than zero"
        );
        ensure!(
            self.max_output_tokens > 0,
            "subagent.limits.max_output_tokens must be greater than zero"
        );
        ensure!(
            self.max_output_tokens <= u64::from(u32::MAX),
            "subagent.limits.max_output_tokens exceeds the inference limit"
        );
        ensure!(
            self.max_capability_calls > 0,
            "subagent.limits.max_capability_calls must be greater than zero"
        );
        ensure!(
            self.timeout_seconds > 0,
            "subagent.limits.timeout_seconds must be greater than zero"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RuntimeSubagent {
    pub id: String,
    pub title: String,
    pub description: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSubagentModel {
    pub inherit: bool,
    pub profile: Option<String>,
    pub profile_path: Option<PathBuf>,
    pub profile_digest: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSubagentSpec {
    pub id: String,
    pub title: String,
    pub description: String,
    pub model: RuntimeSubagentModel,
    pub tool_mode: FunctionToolMode,
    pub tool_policy: FunctionToolPolicy,
    pub skills: Vec<String>,
    pub memory: Option<RuntimeSubagentMemory>,
    pub children: Vec<String>,
    pub limits: SubagentLimits,
    pub system_body: String,
    pub source: FunctionSource,
    pub source_path: PathBuf,
    pub source_digest: String,
    pub spec_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSubagentMemory {
    pub read: Vec<String>,
    pub write: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeDelegationPlan {
    pub budget: FunctionDelegationBudget,
    pub root_subagents: Vec<String>,
    pub subagent_specs: BTreeMap<String, RuntimeSubagentSpec>,
}

pub(crate) fn resolve_runtime_subagent_specs(
    loaded: &LoadedFunction,
    workspace_root: &Path,
    config_dir: &Path,
    require_profile: bool,
) -> Result<BTreeMap<String, RuntimeSubagentSpec>> {
    loaded
        .subagents
        .iter()
        .map(|subagent| {
            let (profile_path, profile_digest) =
                if let Some(profile) = &subagent.front_matter.model.profile {
                    let resolution = resolve_profile(profile, workspace_root, config_dir)?;
                    let selected = match resolution.selected_path {
                        Some(path) => Some(path),
                        None if require_profile => {
                            bail!(
                                "subagent `{}` inference profile `{profile}` not found; checked {}",
                                subagent.front_matter.id,
                                join_paths(&resolution.candidates)
                            );
                        }
                        None => None,
                    };
                    let digest = selected
                        .as_ref()
                        .filter(|path| path.is_file())
                        .map(std::fs::read)
                        .transpose()
                        .with_context(|| {
                            format!(
                                "failed to read subagent profile for `{}`",
                                subagent.front_matter.id
                            )
                        })?
                        .map(|bytes| sha256_bytes(&bytes));
                    (selected, digest)
                } else {
                    (None, None)
                };
            let normalized = serde_yaml::to_string(&subagent.front_matter)
                .context("failed to normalize subagent specification")?;
            let spec_digest = sha256_text(&format!(
                "{}\0{}\0{}",
                subagent.source_digest,
                normalized,
                profile_digest.as_deref().unwrap_or("inherit")
            ));
            let memory =
                subagent
                    .front_matter
                    .memory
                    .as_ref()
                    .map(|memory| RuntimeSubagentMemory {
                        read: memory.read.clone(),
                        write: memory.write.clone(),
                    });
            Ok((
                subagent.front_matter.id.clone(),
                RuntimeSubagentSpec {
                    id: subagent.front_matter.id.clone(),
                    title: subagent.front_matter.title.clone(),
                    description: subagent.front_matter.description.clone(),
                    model: RuntimeSubagentModel {
                        inherit: subagent.front_matter.model.inherit,
                        profile: subagent.front_matter.model.profile.clone(),
                        profile_path,
                        profile_digest,
                    },
                    tool_mode: subagent.front_matter.tools.mode,
                    tool_policy: subagent.front_matter.tools.to_runtime_policy(),
                    skills: subagent
                        .front_matter
                        .skills
                        .as_ref()
                        .map(|skills| skills.use_.clone())
                        .unwrap_or_default(),
                    memory,
                    children: subagent.front_matter.subagents.use_.clone(),
                    limits: subagent.front_matter.limits.clone(),
                    system_body: subagent.body.trim().to_string(),
                    source: loaded.locator.source,
                    source_path: subagent.path.clone(),
                    source_digest: subagent.source_digest.clone(),
                    spec_digest,
                },
            ))
        })
        .collect()
}

pub(crate) fn load_declared_subagents(
    function_root: &Path,
    front_matter: &AgentFunctionFrontMatter,
) -> Result<Vec<LoadedSubagent>> {
    let mut subagents = BTreeMap::new();
    let mut visiting = Vec::new();
    for subagent_id in front_matter.selected_subagents() {
        load_subagent_graph_node(function_root, subagent_id, &mut visiting, &mut subagents)?;
    }
    Ok(subagents.into_values().collect())
}

pub(crate) fn load_subagent_graph_node(
    function_root: &Path,
    subagent_id: &str,
    visiting: &mut Vec<String>,
    loaded: &mut BTreeMap<String, LoadedSubagent>,
) -> Result<()> {
    validate_function_id("subagent id", subagent_id)?;
    if loaded.contains_key(subagent_id) {
        return Ok(());
    }
    if let Some(index) = visiting
        .iter()
        .position(|candidate| candidate == subagent_id)
    {
        let mut cycle = visiting[index..].to_vec();
        cycle.push(subagent_id.to_string());
        bail!("subagent graph contains a cycle: {}", cycle.join(" -> "));
    }
    visiting.push(subagent_id.to_string());
    let path = function_root
        .join("subagents")
        .join(format!("{subagent_id}.md"));
    ensure!(
        path.starts_with(function_root),
        "subagent path escapes function root: {}",
        path.display()
    );
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read declared subagent `{subagent_id}`"))?;
    let source_digest = sha256_text(&content);
    let (front_matter, body) = parse_subagent_document(&content)
        .with_context(|| format!("failed to parse subagent {}", path.display()))?;
    front_matter.validate()?;
    ensure!(
        front_matter.id == subagent_id,
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
    ensure!(
        !body.trim().is_empty(),
        "subagent `{subagent_id}` system body cannot be empty"
    );
    for child in &front_matter.subagents.use_ {
        ensure!(
            child != subagent_id,
            "subagent `{subagent_id}` cannot delegate to itself"
        );
        load_subagent_graph_node(function_root, child, visiting, loaded)?;
    }
    visiting.pop();
    let sections = markdown_sections(&body);
    loaded.insert(
        subagent_id.to_string(),
        LoadedSubagent {
            path,
            front_matter,
            body,
            sections,
            source_digest,
        },
    );
    Ok(())
}
