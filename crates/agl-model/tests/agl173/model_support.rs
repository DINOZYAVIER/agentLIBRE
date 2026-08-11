use agl_config::{KvCacheType, ModelId};
use agl_model::{
    CatalogCapability, CatalogRuntimeProfile, GenerationPolicy, HostCapabilities,
    HostCapabilityDevice, HostCapabilityDeviceKind, ModelArtifact, ModelArtifactFile,
    ModelArtifactRole, ModelPackage, ModelPackageId, PackagePlanIdentity, ProfileDevice,
    ResolvedFunctionPlanInput, ResolvedModelPlanInput, StructuredGenerationMode,
};

#[derive(Clone)]
pub struct ArtifactFileFixture {
    pub filename: String,
    pub byte_size: u64,
    pub sha256: String,
}

impl ArtifactFileFixture {
    pub fn new(filename: &str, byte_size: u64, sha256: &str) -> Self {
        Self {
            filename: filename.to_owned(),
            byte_size,
            sha256: sha256.to_owned(),
        }
    }
}

pub struct RoleFixture {
    role: ModelArtifactRole,
    model_id: ModelId,
    files: Vec<ArtifactFileFixture>,
}

impl RoleFixture {
    pub fn new(role: ModelArtifactRole, model_id: &str) -> Self {
        Self {
            role,
            model_id: ModelId::new(model_id).unwrap(),
            files: Vec::new(),
        }
    }

    pub fn file(mut self, file: ArtifactFileFixture) -> Self {
        self.files.push(file);
        self
    }
}

#[derive(Clone)]
pub struct PlanFixture {
    function: ResolvedFunctionPlanInput,
    model: ResolvedModelPlanInput,
    capabilities: HostCapabilities,
    label: String,
}

impl PlanFixture {
    pub fn canonical_v3() -> Self {
        let function = ResolvedFunctionPlanInput {
            package: package_identity("function:fixture@=1.0.0", 'f'),
            selected_profile_id: "vulkan0-32k".to_owned(),
            generation_policy: policy(512, &["<end>", "<stop>"]),
            prompt_template_digest: digest('b'),
            visible_tools_digest: digest('c'),
        };
        let model = ModelPackage {
            id: ModelPackageId::new("fixture-model").unwrap(),
            provenance: None,
            display_name: "Fixture model".to_owned(),
            capabilities: vec![CatalogCapability::Text, CatalogCapability::Tools],
            license: "apache-2.0".to_owned(),
            license_url: "https://example.invalid/license".to_owned(),
            repository: "example/fixture".to_owned(),
            revision: "a".repeat(40),
            artifacts: vec![ModelArtifact {
                role: ModelArtifactRole::Main,
                model_id: ModelId::new("fixture-model").unwrap(),
                files: vec![ModelArtifactFile {
                    filename: "fixture.gguf".to_owned(),
                    byte_size: 1024,
                    sha256: "1".repeat(64),
                }],
                required: true,
            }],
            profiles: vec![CatalogRuntimeProfile {
                id: "vulkan0-32k".to_owned(),
                device: ProfileDevice::Gpu,
                pci_device_id: Some("1002:744c".to_owned()),
                pci_subsystem_id: Some("1da2:471e".to_owned()),
                benchmark_evidence: "evidence.md".to_owned(),
                required_total_ram_bytes: 32 << 30,
                host_private_bytes: 64 << 20,
                device_private_bytes: 24 << 30,
                shared_bytes: 512 << 20,
                decoder_scratch_bytes: 32 << 20,
                gpu_layers: 61,
                context_tokens: 32_768,
                batch_size: 2_048,
                ubatch_size: 512,
                threads: 16,
                flash_attention: true,
                cache_type_k: KvCacheType::Q8_0,
                cache_type_v: KvCacheType::Q8_0,
                mmap: true,
                unified_kv: false,
                slot_count: 1,
                smoke_timeout_seconds: 120,
                expected_speed: "fast".to_owned(),
            }],
        };
        Self {
            function,
            model: ResolvedModelPlanInput {
                package: package_identity("model:fixture-model@=1.0.0", 'a'),
                payload_schema: "agentlibre.model/v3".to_owned(),
                model,
            },
            capabilities: HostCapabilities {
                physical_host_bytes: 64 << 30,
                physical_cpu_cores: 32,
                logical_cpu_cores: 64,
                devices: vec![HostCapabilityDevice {
                    identity: "Vulkan0".to_owned(),
                    kind: HostCapabilityDeviceKind::DiscreteGpu,
                    pci_device_id: Some("1002:744c".to_owned()),
                    pci_subsystem_id: Some("1da2:471e".to_owned()),
                    physical_pool_bytes: 32 << 30,
                    usable: true,
                    supports_gpu_offload: true,
                }],
            },
            label: "canonical".to_owned(),
        }
    }

    pub fn resolved_function(&self) -> &ResolvedFunctionPlanInput {
        &self.function
    }

    pub fn resolved_model(&self) -> &ResolvedModelPlanInput {
        &self.model
    }

    pub fn host_capabilities(&self) -> &HostCapabilities {
        &self.capabilities
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn with_payload_schema(mut self, schema: &str) -> Self {
        self.model.payload_schema = schema.to_owned();
        self
    }

    pub fn with_device_pci(mut self, device: &str, subsystem: &str) -> Self {
        self.capabilities.devices[0].pci_device_id = Some(device.to_owned());
        self.capabilities.devices[0].pci_subsystem_id = Some(subsystem.to_owned());
        self
    }

    pub fn with_physical_host_bytes(mut self, bytes: u64) -> Self {
        self.capabilities.physical_host_bytes = bytes;
        self
    }

    pub fn with_physical_device_bytes(mut self, bytes: u64) -> Self {
        self.capabilities.devices[0].physical_pool_bytes = bytes;
        self
    }

    pub fn with_live_available_bytes(self, _host: u64, _device: u64) -> Self {
        self
    }

    pub fn every_audit_field_variant(&self) -> Vec<Self> {
        let mut function_package = self.clone();
        function_package.function.package = package_identity("function:other@=1.0.0", 'f');
        function_package.label = "function package".to_owned();
        let mut model_package = self.clone();
        model_package.model.package = package_identity("model:other@=1.0.0", 'a');
        model_package.label = "model package".to_owned();
        let mut profile = self.clone();
        profile.model.model.profiles[0].batch_size += 1;
        profile.label = "profile runtime".to_owned();
        let mut policy = self.clone();
        policy.function.generation_policy = policy_fn(513);
        policy.label = "generation policy".to_owned();
        let mut artifact = self.clone();
        artifact.model.model.artifacts[0].files[0].sha256 = "2".repeat(64);
        artifact.label = "artifact digest".to_owned();
        vec![function_package, model_package, profile, policy, artifact]
    }

    pub fn sampling_only_variants(&self) -> Vec<Self> {
        let mut limit = self.clone();
        limit.function.generation_policy = policy_fn(513);
        limit.label = "output limit".to_owned();
        let mut stop = self.clone();
        stop.function.generation_policy = policy(512, &["different"]);
        stop.label = "stop rules".to_owned();
        vec![limit, stop]
    }

    pub fn native_load_variants(&self) -> Vec<Self> {
        let mut bytes = self.clone();
        bytes.model.model.artifacts[0].files[0].sha256 = "3".repeat(64);
        bytes.label = "weight bytes".to_owned();
        let mut shape = self.clone();
        shape.model.model.profiles[0].context_tokens = 65_536;
        shape.label = "native shape".to_owned();
        vec![bytes, shape]
    }

    pub fn replace_role(mut self, role: RoleFixture) -> Self {
        self.model
            .model
            .artifacts
            .retain(|artifact| artifact.role != role.role);
        self.model.model.artifacts.push(ModelArtifact {
            role: role.role,
            model_id: role.model_id,
            files: role
                .files
                .into_iter()
                .map(|file| ModelArtifactFile {
                    filename: file.filename,
                    byte_size: file.byte_size,
                    sha256: file.sha256,
                })
                .collect(),
            required: true,
        });
        self
    }

    pub fn empty_role(self, role: ModelArtifactRole) -> Self {
        self.replace_role(RoleFixture::new(role, "empty-role"))
    }

    pub fn role_file_basename(mut self, role: ModelArtifactRole, value: &str) -> Self {
        self.role_mut(role).files[0].filename = value.to_owned();
        self
    }

    pub fn role_file_size(mut self, role: ModelArtifactRole, value: u64) -> Self {
        self.role_mut(role).files[0].byte_size = value;
        self
    }

    pub fn role_file_sha256(mut self, role: ModelArtifactRole, value: &str) -> Self {
        self.role_mut(role).files[0].sha256 = value.to_owned();
        self
    }

    fn role_mut(&mut self, role: ModelArtifactRole) -> &mut ModelArtifact {
        self.model
            .model
            .artifacts
            .iter_mut()
            .find(|artifact| artifact.role == role)
            .unwrap()
    }
}

fn policy_fn(max_output_tokens: u32) -> GenerationPolicy {
    policy(max_output_tokens, &["<end>", "<stop>"])
}

fn policy(max_output_tokens: u32, stops: &[&str]) -> GenerationPolicy {
    GenerationPolicy::greedy(
        max_output_tokens,
        stops.iter().map(|value| (*value).to_owned()).collect(),
        StructuredGenerationMode::LazyTool,
        true,
    )
    .unwrap()
}

fn package_identity(reference: &str, fill: char) -> PackagePlanIdentity {
    PackagePlanIdentity {
        reference: reference.parse().unwrap(),
        source_id: "test".parse().unwrap(),
        package_tree_digest: digest(fill).parse().unwrap(),
    }
}

fn digest(fill: char) -> String {
    format!("sha256:{}", fill.to_string().repeat(64))
}
