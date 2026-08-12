use std::collections::BTreeSet;

use agl_package::{PackageRef, PackageSourceId, PackageTreeDigest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    CatalogCapability, CatalogRuntimeProfile, ModelArtifactRole, ModelPackage, ProfileDevice,
};

pub const MODEL_EXECUTION_PLAN_IDENTITY_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackagePlanIdentity {
    pub reference: PackageRef,
    pub source_id: PackageSourceId,
    pub package_tree_digest: PackageTreeDigest,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredGenerationMode {
    Disabled,
    #[default]
    LazyTool,
    RequiredTool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationPolicy {
    max_output_tokens: u32,
    stop_rules: Vec<String>,
    structured_mode: StructuredGenerationMode,
    repair_malformed_tool_calls: bool,
}

impl GenerationPolicy {
    pub fn greedy(
        max_output_tokens: u32,
        stop_rules: Vec<String>,
        structured_mode: StructuredGenerationMode,
        repair_malformed_tool_calls: bool,
    ) -> Result<Self, ModelPlanRejection> {
        if max_output_tokens == 0 {
            return Err(ModelPlanRejection::InvalidGenerationPolicy {
                reason: "max_output_tokens must be greater than zero".to_owned(),
            });
        }
        if stop_rules.iter().any(|rule| rule.is_empty()) {
            return Err(ModelPlanRejection::InvalidGenerationPolicy {
                reason: "stop rules must not be empty".to_owned(),
            });
        }
        let policy = Self {
            max_output_tokens,
            stop_rules,
            structured_mode,
            repair_malformed_tool_calls,
        };
        Ok(policy)
    }

    pub fn is_greedy(&self) -> bool {
        true
    }

    pub fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
    }

    pub fn stop_rules(&self) -> &[String] {
        &self.stop_rules
    }

    pub fn structured_mode(&self) -> StructuredGenerationMode {
        self.structured_mode
    }

    pub fn repair_malformed_tool_calls(&self) -> bool {
        self.repair_malformed_tool_calls
    }
}

impl StructuredGenerationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::LazyTool => "lazy_tool",
            Self::RequiredTool => "required_tool",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedFunctionPlanInput {
    pub package: PackagePlanIdentity,
    pub selected_profile_id: String,
    pub generation_policy: GenerationPolicy,
    pub prompt_template_digest: String,
    pub visible_tools_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedModelPlanInput {
    pub package: PackagePlanIdentity,
    pub payload_schema: String,
    pub model: ModelPackage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostCapabilityDeviceKind {
    Cpu,
    DiscreteGpu,
    IntegratedGpu,
    Accelerator,
    Metadata,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCapabilityDevice {
    pub identity: String,
    pub kind: HostCapabilityDeviceKind,
    pub pci_device_id: Option<String>,
    pub pci_subsystem_id: Option<String>,
    pub physical_pool_bytes: u64,
    pub usable: bool,
    pub supports_gpu_offload: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCapabilities {
    pub physical_host_bytes: u64,
    pub physical_cpu_cores: usize,
    pub logical_cpu_cores: usize,
    pub devices: Vec<HostCapabilityDevice>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileMismatchPredicate {
    ProfileId,
    DeviceKind,
    PciDeviceId,
    PciSubsystemId,
    HostPhysicalBytes,
    DevicePhysicalBytes,
    CpuCoreCount,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Error)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelPlanRejection {
    #[error("unsupported Model payload schema `{schema}`; expected agentlibre.model/v3")]
    UnsupportedModelSchema { schema: String },
    #[error("Model profile `{profile_id}` does not exist")]
    UnknownProfile { profile_id: String },
    #[error("Model profile `{profile_id}` has static capability mismatches: {predicates:?}")]
    StaticMismatch {
        profile_id: String,
        predicates: BTreeSet<ProfileMismatchPredicate>,
    },
    #[error("invalid Model artifact role `{role}`: {reason}")]
    InvalidArtifact { role: String, reason: String },
    #[error("invalid Function generation policy: {reason}")]
    InvalidGenerationPolicy { reason: String },
    #[error("invalid Model runtime shape: {reason}")]
    InvalidRuntimeShape { reason: String },
    #[error("failed to encode canonical execution-plan identity: {reason}")]
    IdentityEncoding { reason: String },
}

impl ModelPlanRejection {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedModelSchema { .. } => "unsupported_model_schema",
            Self::UnknownProfile { .. } => "unknown_profile",
            Self::StaticMismatch { .. } => "static_profile_mismatch",
            Self::InvalidArtifact { .. } => "invalid_model_artifact",
            Self::InvalidGenerationPolicy { .. } => "invalid_generation_policy",
            Self::InvalidRuntimeShape { .. } => "invalid_runtime_shape",
            Self::IdentityEncoding { .. } => "plan_identity_encoding_failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedArtifactFile {
    basename: String,
    byte_size: u64,
    sha256: String,
}

impl PlannedArtifactFile {
    pub fn basename(&self) -> &str {
        &self.basename
    }

    pub fn byte_size(&self) -> u64 {
        self.byte_size
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelArtifactPlan {
    role: ModelArtifactRole,
    model_id: String,
    files: Vec<PlannedArtifactFile>,
}

impl ModelArtifactPlan {
    pub fn role(&self) -> ModelArtifactRole {
        self.role
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    pub fn files(&self) -> &[PlannedArtifactFile] {
        &self.files
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelRuntimeShape {
    context_tokens: u32,
    batch_size: u32,
    ubatch_size: u32,
    threads: u32,
    gpu_layers: u32,
    flash_attention: bool,
    key_cache_type: String,
    value_cache_type: String,
    mmap: bool,
    unified_kv: bool,
    slot_count: u32,
    mtp: Option<ModelMtpShape>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelMtpShape {
    max_draft_tokens: u32,
    min_draft_tokens: u32,
    p_min_millionths: u32,
    gpu_layers: u32,
    key_cache_type: String,
    value_cache_type: String,
}

impl ModelMtpShape {
    pub fn max_draft_tokens(&self) -> u32 {
        self.max_draft_tokens
    }
    pub fn min_draft_tokens(&self) -> u32 {
        self.min_draft_tokens
    }
    pub fn p_min_millionths(&self) -> u32 {
        self.p_min_millionths
    }
    pub fn gpu_layers(&self) -> u32 {
        self.gpu_layers
    }
    pub fn key_cache_type(&self) -> &str {
        &self.key_cache_type
    }
    pub fn value_cache_type(&self) -> &str {
        &self.value_cache_type
    }
}

impl ModelRuntimeShape {
    pub fn context_tokens(&self) -> u32 {
        self.context_tokens
    }
    pub fn batch_size(&self) -> u32 {
        self.batch_size
    }
    pub fn ubatch_size(&self) -> u32 {
        self.ubatch_size
    }
    pub fn threads(&self) -> u32 {
        self.threads
    }
    pub fn gpu_layers(&self) -> u32 {
        self.gpu_layers
    }
    pub fn flash_attention(&self) -> bool {
        self.flash_attention
    }
    pub fn key_cache_type(&self) -> &str {
        &self.key_cache_type
    }
    pub fn value_cache_type(&self) -> &str {
        &self.value_cache_type
    }
    pub fn mmap(&self) -> bool {
        self.mmap
    }
    pub fn unified_kv(&self) -> bool {
        self.unified_kv
    }
    pub fn slot_count(&self) -> u32 {
        self.slot_count
    }
    pub fn mtp(&self) -> Option<&ModelMtpShape> {
        self.mtp.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelResourceEnvelope {
    host_private_bytes: u64,
    device_private_bytes: u64,
    shared_bytes: u64,
    decoder_scratch_bytes: u64,
}

impl ModelResourceEnvelope {
    pub fn host_private_bytes(&self) -> u64 {
        self.host_private_bytes
    }
    pub fn device_private_bytes(&self) -> u64 {
        self.device_private_bytes
    }
    pub fn shared_bytes(&self) -> u64 {
        self.shared_bytes
    }
    pub fn decoder_scratch_bytes(&self) -> u64 {
        self.decoder_scratch_bytes
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelExecutionPlanDigest(String);

impl ModelExecutionPlanDigest {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelLoadKey(String);

impl ModelLoadKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelContextKey(String);

impl ModelContextKey {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelExecutionPlan {
    identity_version: u32,
    function_package: PackagePlanIdentity,
    model_package: PackagePlanIdentity,
    profile_id: String,
    capabilities: Vec<CatalogCapability>,
    selected_device: Option<HostCapabilityDevice>,
    artifacts: Vec<ModelArtifactPlan>,
    runtime: ModelRuntimeShape,
    resources: ModelResourceEnvelope,
    generation_policy: GenerationPolicy,
    prompt_template_digest: String,
    visible_tools_digest: String,
    digest: ModelExecutionPlanDigest,
    model_key: ModelLoadKey,
}

impl ModelExecutionPlan {
    pub fn digest(&self) -> &ModelExecutionPlanDigest {
        &self.digest
    }
    pub fn function_package(&self) -> &PackagePlanIdentity {
        &self.function_package
    }
    pub fn model_package(&self) -> &PackagePlanIdentity {
        &self.model_package
    }
    pub fn profile_id(&self) -> &str {
        &self.profile_id
    }
    pub fn capabilities(&self) -> &[CatalogCapability] {
        &self.capabilities
    }
    pub fn supports(&self, capability: CatalogCapability) -> bool {
        self.capabilities.contains(&capability)
    }
    pub fn selected_device(&self) -> Option<&HostCapabilityDevice> {
        self.selected_device.as_ref()
    }
    pub fn artifact_roles(&self) -> &[ModelArtifactPlan] {
        &self.artifacts
    }
    pub fn artifact_role(&self, role: ModelArtifactRole) -> Option<&ModelArtifactPlan> {
        self.artifacts.iter().find(|artifact| artifact.role == role)
    }
    pub fn runtime(&self) -> &ModelRuntimeShape {
        &self.runtime
    }
    pub fn resources(&self) -> &ModelResourceEnvelope {
        &self.resources
    }
    pub fn generation_policy(&self) -> &GenerationPolicy {
        &self.generation_policy
    }
    pub fn model_key(&self) -> &ModelLoadKey {
        &self.model_key
    }
    pub fn context_key(&self, conversation_identity: &str) -> ModelContextKey {
        let material = serde_json::json!({
            "domain": "agentlibre.model-context-key/v1",
            "model_key": self.model_key.as_str(),
            "conversation_identity": conversation_identity,
            "prompt_template_digest": self.prompt_template_digest,
            "visible_tools_digest": self.visible_tools_digest,
        });
        ModelContextKey(digest_json(&material).expect("serializing JSON value cannot fail"))
    }
}

#[derive(Serialize)]
struct PlanDigestMaterial<'a> {
    domain: &'static str,
    identity_version: u32,
    function_package: &'a PackagePlanIdentity,
    model_package: &'a PackagePlanIdentity,
    profile_id: &'a str,
    capabilities: &'a [CatalogCapability],
    selected_device: &'a Option<HostCapabilityDevice>,
    artifacts: &'a [ModelArtifactPlan],
    runtime: &'a ModelRuntimeShape,
    resources: &'a ModelResourceEnvelope,
    generation_policy: &'a GenerationPolicy,
    prompt_template_digest: &'a str,
    visible_tools_digest: &'a str,
}

#[derive(Serialize)]
struct ModelKeyMaterial<'a> {
    domain: &'static str,
    selected_device: &'a Option<HostCapabilityDevice>,
    artifacts: &'a [ModelArtifactPlan],
    runtime: &'a ModelRuntimeShape,
}

pub fn resolve_execution_plan(
    function: &ResolvedFunctionPlanInput,
    model: &ResolvedModelPlanInput,
    capabilities: &HostCapabilities,
) -> Result<ModelExecutionPlan, ModelPlanRejection> {
    if model.payload_schema != "agentlibre.model/v3" {
        return Err(ModelPlanRejection::UnsupportedModelSchema {
            schema: model.payload_schema.clone(),
        });
    }
    let profile = model
        .model
        .profiles
        .iter()
        .find(|profile| profile.id == function.selected_profile_id)
        .ok_or_else(|| ModelPlanRejection::UnknownProfile {
            profile_id: function.selected_profile_id.clone(),
        })?;
    let selected_device = match_static_profile(profile, capabilities)?;
    let artifacts = planned_artifacts(&model.model)?;
    validate_runtime_feature_matrix(&model.model, profile)?;
    let runtime = runtime_shape(profile);
    let resources = resource_envelope(profile);
    let model_key = ModelLoadKey(digest_json(&ModelKeyMaterial {
        domain: "agentlibre.model-load-key/v1",
        selected_device: &selected_device,
        artifacts: &artifacts,
        runtime: &runtime,
    })?);
    let digest = ModelExecutionPlanDigest(digest_json(&PlanDigestMaterial {
        domain: "agentlibre.model-execution-plan/v1",
        identity_version: MODEL_EXECUTION_PLAN_IDENTITY_VERSION,
        function_package: &function.package,
        model_package: &model.package,
        profile_id: &function.selected_profile_id,
        capabilities: &model.model.capabilities,
        selected_device: &selected_device,
        artifacts: &artifacts,
        runtime: &runtime,
        resources: &resources,
        generation_policy: &function.generation_policy,
        prompt_template_digest: &function.prompt_template_digest,
        visible_tools_digest: &function.visible_tools_digest,
    })?);
    Ok(ModelExecutionPlan {
        identity_version: MODEL_EXECUTION_PLAN_IDENTITY_VERSION,
        function_package: function.package.clone(),
        model_package: model.package.clone(),
        profile_id: function.selected_profile_id.clone(),
        capabilities: model.model.capabilities.clone(),
        selected_device,
        artifacts,
        runtime,
        resources,
        generation_policy: function.generation_policy.clone(),
        prompt_template_digest: function.prompt_template_digest.clone(),
        visible_tools_digest: function.visible_tools_digest.clone(),
        digest,
        model_key,
    })
}

fn validate_runtime_feature_matrix(
    model: &ModelPackage,
    profile: &CatalogRuntimeProfile,
) -> Result<(), ModelPlanRejection> {
    let has_draft = model
        .artifacts
        .iter()
        .any(|artifact| artifact.role == ModelArtifactRole::Draft && artifact.required);
    if has_draft != profile.mtp.is_some() {
        return Err(ModelPlanRejection::InvalidRuntimeShape {
            reason: "Draft artifact and MTP profile must be present together".to_owned(),
        });
    }
    if profile.mtp.is_some() && model.capabilities.contains(&CatalogCapability::Vision) {
        return Err(ModelPlanRejection::InvalidRuntimeShape {
            reason: "vision and speculative MTP cannot share one profile".to_owned(),
        });
    }
    if let Some(mtp) = &profile.mtp
        && (!(1..=64).contains(&mtp.max_draft_tokens)
            || mtp.min_draft_tokens > mtp.max_draft_tokens
            || mtp.p_min_millionths > 1_000_000)
    {
        return Err(ModelPlanRejection::InvalidRuntimeShape {
            reason: "MTP token or probability bounds are invalid".to_owned(),
        });
    }
    Ok(())
}

fn match_static_profile(
    profile: &CatalogRuntimeProfile,
    capabilities: &HostCapabilities,
) -> Result<Option<HostCapabilityDevice>, ModelPlanRejection> {
    let mut mismatches = BTreeSet::new();
    if capabilities.physical_host_bytes < profile.required_total_ram_bytes {
        mismatches.insert(ProfileMismatchPredicate::HostPhysicalBytes);
    }
    if capabilities.physical_cpu_cores < profile.threads as usize {
        mismatches.insert(ProfileMismatchPredicate::CpuCoreCount);
    }
    let selected = match profile.device {
        ProfileDevice::Cpu => None,
        ProfileDevice::Gpu => {
            let candidate = capabilities.devices.iter().find(|device| {
                device.usable
                    && device.supports_gpu_offload
                    && matches!(
                        device.kind,
                        HostCapabilityDeviceKind::DiscreteGpu
                            | HostCapabilityDeviceKind::IntegratedGpu
                            | HostCapabilityDeviceKind::Accelerator
                    )
            });
            let Some(device) = candidate else {
                mismatches.insert(ProfileMismatchPredicate::DeviceKind);
                return Err(ModelPlanRejection::StaticMismatch {
                    profile_id: profile.id.clone(),
                    predicates: mismatches,
                });
            };
            if profile.pci_device_id.as_ref() != device.pci_device_id.as_ref() {
                mismatches.insert(ProfileMismatchPredicate::PciDeviceId);
            }
            if profile.pci_subsystem_id.as_ref() != device.pci_subsystem_id.as_ref() {
                mismatches.insert(ProfileMismatchPredicate::PciSubsystemId);
            }
            if device.physical_pool_bytes < profile.device_private_bytes {
                mismatches.insert(ProfileMismatchPredicate::DevicePhysicalBytes);
            }
            Some(device.clone())
        }
    };
    if mismatches.is_empty() {
        Ok(selected)
    } else {
        Err(ModelPlanRejection::StaticMismatch {
            profile_id: profile.id.clone(),
            predicates: mismatches,
        })
    }
}

fn planned_artifacts(model: &ModelPackage) -> Result<Vec<ModelArtifactPlan>, ModelPlanRejection> {
    let mut roles = BTreeSet::new();
    let mut basenames = BTreeSet::new();
    model
        .required_artifacts()
        .map(|artifact| {
            if !roles.insert(artifact.role) {
                return Err(ModelPlanRejection::InvalidArtifact {
                    role: format!("{:?}", artifact.role),
                    reason: "role appears more than once".to_owned(),
                });
            }
            if artifact.files.is_empty() {
                return Err(ModelPlanRejection::InvalidArtifact {
                    role: format!("{:?}", artifact.role),
                    reason: "role has no declared files".to_owned(),
                });
            }
            for file in &artifact.files {
                let safe_basename = !file.filename.is_empty()
                    && std::path::Path::new(&file.filename).components().count() == 1
                    && file.filename.to_ascii_lowercase().ends_with(".gguf");
                if !safe_basename {
                    return Err(ModelPlanRejection::InvalidArtifact {
                        role: format!("{:?}", artifact.role),
                        reason: format!("`{}` is not a safe GGUF basename", file.filename),
                    });
                }
                if !basenames.insert(file.filename.as_str()) {
                    return Err(ModelPlanRejection::InvalidArtifact {
                        role: format!("{:?}", artifact.role),
                        reason: format!("duplicate basename `{}`", file.filename),
                    });
                }
                if file.byte_size <= 4 {
                    return Err(ModelPlanRejection::InvalidArtifact {
                        role: format!("{:?}", artifact.role),
                        reason: format!("`{}` has invalid byte size", file.filename),
                    });
                }
                if file.sha256.len() != 64
                    || !file
                        .sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                {
                    return Err(ModelPlanRejection::InvalidArtifact {
                        role: format!("{:?}", artifact.role),
                        reason: format!("`{}` has invalid SHA-256", file.filename),
                    });
                }
            }
            Ok(ModelArtifactPlan {
                role: artifact.role,
                model_id: artifact.model_id.to_string(),
                files: artifact
                    .files
                    .iter()
                    .map(|file| PlannedArtifactFile {
                        basename: file.filename.clone(),
                        byte_size: file.byte_size,
                        sha256: file.sha256.clone(),
                    })
                    .collect(),
            })
        })
        .collect()
}

fn runtime_shape(profile: &CatalogRuntimeProfile) -> ModelRuntimeShape {
    ModelRuntimeShape {
        context_tokens: profile.context_tokens,
        batch_size: profile.batch_size,
        ubatch_size: profile.ubatch_size,
        threads: profile.threads,
        gpu_layers: profile.gpu_layers,
        flash_attention: profile.flash_attention,
        key_cache_type: kv_cache_type_name(profile.cache_type_k).to_owned(),
        value_cache_type: kv_cache_type_name(profile.cache_type_v).to_owned(),
        mmap: profile.mmap,
        unified_kv: profile.unified_kv,
        slot_count: profile.slot_count,
        mtp: profile.mtp.as_ref().map(|mtp| ModelMtpShape {
            max_draft_tokens: mtp.max_draft_tokens,
            min_draft_tokens: mtp.min_draft_tokens,
            p_min_millionths: mtp.p_min_millionths,
            gpu_layers: mtp.gpu_layers,
            key_cache_type: kv_cache_type_name(mtp.cache_type_k).to_owned(),
            value_cache_type: kv_cache_type_name(mtp.cache_type_v).to_owned(),
        }),
    }
}

fn resource_envelope(profile: &CatalogRuntimeProfile) -> ModelResourceEnvelope {
    ModelResourceEnvelope {
        host_private_bytes: profile.host_private_bytes,
        device_private_bytes: profile.device_private_bytes,
        shared_bytes: profile.shared_bytes,
        decoder_scratch_bytes: profile.decoder_scratch_bytes,
    }
}

fn kv_cache_type_name(value: agl_config::KvCacheType) -> &'static str {
    use agl_config::KvCacheType;
    match value {
        KvCacheType::F32 => "f32",
        KvCacheType::F16 => "f16",
        KvCacheType::Bf16 => "bf16",
        KvCacheType::Q8_0 => "q8_0",
        KvCacheType::Q4_0 => "q4_0",
        KvCacheType::Q4_1 => "q4_1",
        KvCacheType::Iq4Nl => "iq4_nl",
        KvCacheType::Q5_0 => "q5_0",
        KvCacheType::Q5_1 => "q5_1",
    }
}

fn digest_json(value: &impl Serialize) -> Result<String, ModelPlanRejection> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| ModelPlanRejection::IdentityEncoding {
            reason: error.to_string(),
        })?;
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(71);
    encoded.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(encoded)
}
