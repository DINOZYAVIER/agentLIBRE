use std::collections::BTreeMap;

use agl_capabilities::CapabilityId;
pub use agl_capabilities::FunctionToolPolicy;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::validation::{
    default_identity_fields, is_valid_identity_field, validate_extensions, validate_function_id,
    validate_relative_function_file_path, validate_unique_non_empty,
};
pub const FUNCTION_SCHEMA: &str = "agentfunction/v1";
pub const SUBAGENT_SCHEMA: &str = "agentlibre/subagent/v1";
pub const FUNCTION_FILE_NAME: &str = "FUNCTION.md";
pub const FUNCTION_SYSTEM_PROMPT_FILE_NAME: &str = "SYSTEM.md";
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
    pub delegation: Option<FunctionDelegationBudget>,
    #[serde(default)]
    pub memory: Option<FunctionMemory>,
    #[serde(default)]
    pub artifacts: Option<FunctionArtifacts>,
    #[serde(default)]
    pub doctor: Option<FunctionDoctor>,
    #[serde(default)]
    pub validation: Option<FunctionValidation>,
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
        if let Some(delegation) = &self.delegation {
            delegation.validate()?;
        }
        ensure!(
            self.selected_subagents().is_empty() || self.delegation.is_some(),
            "functions with subagents must declare a finite delegation budget"
        );
        if let Some(memory) = &self.memory {
            memory.validate()?;
        }
        if let Some(artifacts) = &self.artifacts {
            artifacts.validate()?;
        }
        if let Some(doctor) = &self.doctor {
            doctor.validate()?;
        }
        if let Some(validation) = &self.validation {
            validation.validate()?;
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

    pub fn runtime_identity_validation(&self) -> Option<RuntimeIdentityValidation> {
        self.validation
            .as_ref()
            .and_then(|validation| validation.runtime_identity.as_ref())
            .map(FunctionRuntimeIdentityValidation::to_runtime)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionDelegationBudget {
    pub max_depth: u32,
    pub max_children_per_run: u32,
    pub max_descendants: u32,
    pub max_total_output_tokens: u64,
    pub timeout_seconds: u64,
}

impl FunctionDelegationBudget {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.max_depth > 0,
            "delegation.max_depth must be greater than zero"
        );
        ensure!(
            self.max_children_per_run > 0,
            "delegation.max_children_per_run must be greater than zero"
        );
        ensure!(
            self.max_descendants > 0,
            "delegation.max_descendants must be greater than zero"
        );
        ensure!(
            self.max_total_output_tokens > 0,
            "delegation.max_total_output_tokens must be greater than zero"
        );
        ensure!(
            self.timeout_seconds > 0,
            "delegation.timeout_seconds must be greater than zero"
        );
        ensure!(self.max_depth <= 16, "delegation.max_depth exceeds 16");
        ensure!(
            self.max_children_per_run <= 64,
            "delegation.max_children_per_run exceeds 64"
        );
        ensure!(
            self.max_descendants <= 1_024,
            "delegation.max_descendants exceeds 1024"
        );
        Ok(())
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
    pub(crate) fn validate(&self, field: &str) -> Result<()> {
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
    pub(crate) fn validate(&self) -> Result<()> {
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
    pub(crate) fn validate(&self) -> Result<()> {
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
pub struct FunctionValidation {
    #[serde(default)]
    pub runtime_identity: Option<FunctionRuntimeIdentityValidation>,
    #[serde(flatten, default)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

impl FunctionValidation {
    fn validate(&self) -> Result<()> {
        validate_extensions("validation", &self.extensions)?;
        if let Some(runtime_identity) = &self.runtime_identity {
            runtime_identity.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeIdentityValidation {
    pub required: bool,
    pub fields: Vec<String>,
    pub repair_attempts: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FunctionRuntimeIdentityValidation {
    #[serde(default)]
    pub required: bool,
    #[serde(default = "default_identity_fields")]
    pub fields: Vec<String>,
    #[serde(default)]
    pub repair_attempts: u32,
    #[serde(flatten, default)]
    pub extensions: BTreeMap<String, serde_yaml::Value>,
}

impl FunctionRuntimeIdentityValidation {
    fn validate(&self) -> Result<()> {
        validate_extensions("validation.runtime_identity", &self.extensions)?;
        validate_unique_non_empty("validation.runtime_identity.fields", &self.fields)?;
        for field in &self.fields {
            ensure!(
                is_valid_identity_field(field),
                "validation.runtime_identity.fields contains unsupported field `{field}`"
            );
        }
        Ok(())
    }

    fn to_runtime(&self) -> RuntimeIdentityValidation {
        RuntimeIdentityValidation {
            required: self.required,
            fields: self.fields.clone(),
            repair_attempts: self.repair_attempts,
        }
    }
}
