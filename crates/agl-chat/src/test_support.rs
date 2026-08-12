use std::path::{Path, PathBuf};

use agl_config::ModelId;
use agl_model::{
    InstallRecordState, InstallSource, ModelArtifactRole, ModelInstallRecord, ModelPackageId,
};
use agl_runtime::AgentLibreRuntimeConfig;

pub(crate) fn install_package_bound_test_model(
    workspace_root: &Path,
    runtime: &AgentLibreRuntimeConfig,
) -> PathBuf {
    let model_path = workspace_root.join("test-model.gguf");
    std::fs::write(&model_path, b"GGUF-test-fixture").unwrap();

    let model_root = workspace_root.join(".agl/models/test-model");
    std::fs::create_dir_all(model_root.join("evidence")).unwrap();
    std::fs::write(
        model_root.join("MODEL.toml"),
        r#"package = { schema = "agentlibre.package/v1", type = "model", id = "test-model", version = "1.0.0", payload_schema = "agentlibre.model/v3", agl = { compatible = ">=1.0.0-alpha.12", tested = ["1.0.0-alpha.12"] }, requires = [] }

display_name = "Chat test model"
capabilities = ["text", "tools"]
license = "test-only"
license_url = "https://example.invalid/license"
repository = "agentlibre/test-model"
upstream_revision = "1111111111111111111111111111111111111111"

[[weights]]
role = "main"
model_id = "test-model"
files = [{ filename = "test-model.gguf", byte_size = 17, sha256 = "2222222222222222222222222222222222222222222222222222222222222222" }]
required = true

[[profiles]]
id = "local"
device = "cpu"
benchmark_evidence = "evidence/local.md"
required_total_ram_bytes = 1
host_private_bytes = 1
device_private_bytes = 0
shared_bytes = 0
decoder_scratch_bytes = 0
gpu_layers = 0
context_tokens = 256
batch_size = 32
ubatch_size = 32
threads = 1
flash_attention = false
cache_type_k = "f16"
cache_type_v = "f16"
mmap = true
unified_kv = false
slot_count = 1
smoke_timeout_seconds = 30
expected_speed = "fixture"

[[profiles]]
id = "reviewer"
device = "cpu"
benchmark_evidence = "evidence/reviewer.md"
required_total_ram_bytes = 1
host_private_bytes = 1
device_private_bytes = 0
shared_bytes = 0
decoder_scratch_bytes = 0
gpu_layers = 0
context_tokens = 256
batch_size = 32
ubatch_size = 32
threads = 1
flash_attention = false
cache_type_k = "f16"
cache_type_v = "f16"
mmap = true
unified_kv = false
slot_count = 1
smoke_timeout_seconds = 30
expected_speed = "fixture"
"#,
    )
    .unwrap();
    std::fs::write(model_root.join("evidence/local.md"), "Test evidence.\n").unwrap();
    std::fs::write(model_root.join("evidence/reviewer.md"), "Test evidence.\n").unwrap();

    let record = ModelInstallRecord {
        version: 1,
        model_id: ModelId::new("test-model").unwrap(),
        package_id: Some(ModelPackageId::new("test-model").unwrap()),
        role: ModelArtifactRole::Main,
        source: InstallSource::HuggingFace {
            repository: "agentlibre/test-model".to_owned(),
            revision: "1".repeat(40),
            filename: "test-model.gguf".to_owned(),
        },
        path: model_path.clone(),
        byte_size: 17,
        sha256: "2".repeat(64),
        additional_files: Vec::new(),
        installed_at_unix_ms: 1,
        state: InstallRecordState::Active,
    };
    let store = agl_model::ModelInstallStore::new(runtime.paths.model_install_root());
    std::fs::create_dir_all(store.root()).unwrap();
    std::fs::write(
        store.record_path(&record.model_id),
        serde_json::to_vec_pretty(&record).unwrap(),
    )
    .unwrap();
    model_path
}
