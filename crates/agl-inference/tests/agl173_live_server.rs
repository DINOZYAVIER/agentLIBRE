use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use agl_config::{KvCacheType, ModelDialect, ModelId, ToolCallFormat};
use agl_content::Content;
use agl_ids::{AttemptId, RunId, TurnId};
use agl_inference::{
    ArtifactFileHandle, EngineExecutable, InferenceHost, InferenceHostConfig, InferenceRequest,
    VolatileHandles,
};
use agl_model::{
    CatalogCapability, CatalogRuntimeProfile, GenerationPolicy, ModelArtifact, ModelArtifactFile,
    ModelArtifactRole, ModelPackage, ModelPackageId, PackagePlanIdentity, ProfileDevice,
    ResolvedFunctionPlanInput, ResolvedModelPlanInput, StructuredGenerationMode,
    resolve_execution_plan,
};
use agl_oven::{RenderedMessage, RenderedMessageRole, RenderedModelRequest};
use sha2::{Digest, Sha256};

// MIW-ADM-009, MIW-ENG-012 and MIW-ENG-014.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires AGL_LLAMA_SERVER and AGL_TEST_MODEL_GGUF"]
async fn private_server_generates_on_the_selected_gpu_from_the_verified_model_descriptor() {
    let executable = required_path("AGL_LLAMA_SERVER");
    let model_path = required_path("AGL_TEST_MODEL_GGUF");
    let executable_sha256 = sha256(&executable);
    let model_sha256 =
        std::env::var("AGL_TEST_MODEL_SHA256").unwrap_or_else(|_| sha256(&model_path));
    let model_size = model_path.metadata().unwrap().len();

    let host = InferenceHost::start(InferenceHostConfig {
        executable: EngineExecutable {
            path: executable,
            sha256: executable_sha256,
        },
        queue_capacity: 4,
        external_host_reserve_bytes: 1 << 30,
        authority_root: std::env::temp_dir()
            .join(format!("agl173-live-authority-{}", std::process::id())),
        context_idle_duration: std::time::Duration::from_secs(900),
        model_idle_duration: std::time::Duration::from_secs(300),
        evidence_root: std::env::temp_dir()
            .join(format!("agl173-live-evidence-{}", std::process::id())),
    })
    .unwrap();
    let function = ResolvedFunctionPlanInput {
        package: package_identity("function:live-smoke@=1.0.0", 'f'),
        selected_profile_id: "vulkan-smoke".to_owned(),
        generation_policy: GenerationPolicy::greedy(
            1,
            Vec::new(),
            StructuredGenerationMode::Disabled,
            false,
        )
        .unwrap(),
        prompt_template_digest: digest('a'),
        visible_tools_digest: digest('b'),
    };
    let model = ResolvedModelPlanInput {
        package: package_identity("model:live-smoke@=1.0.0", 'c'),
        payload_schema: "agentlibre.model/v3".to_owned(),
        model: ModelPackage {
            id: ModelPackageId::new("live-smoke").unwrap(),
            provenance: None,
            display_name: "Live smoke".to_owned(),
            capabilities: vec![CatalogCapability::Text],
            license: "test-only".to_owned(),
            license_url: "https://example.invalid".to_owned(),
            repository: "local/live-smoke".to_owned(),
            revision: "0".repeat(40),
            artifacts: vec![ModelArtifact {
                role: ModelArtifactRole::Main,
                model_id: ModelId::new("live-smoke").unwrap(),
                files: vec![ModelArtifactFile {
                    filename: model_path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    byte_size: model_size,
                    sha256: model_sha256,
                }],
                required: true,
            }],
            profiles: vec![CatalogRuntimeProfile {
                id: "vulkan-smoke".to_owned(),
                device: ProfileDevice::Gpu,
                pci_device_id: Some("1002:744c".to_owned()),
                pci_subsystem_id: Some("1da2:471e".to_owned()),
                benchmark_evidence: "live test".to_owned(),
                required_total_ram_bytes: 16 << 30,
                host_private_bytes: 20 << 30,
                device_private_bytes: 22 << 30,
                shared_bytes: 0,
                decoder_scratch_bytes: 1 << 30,
                gpu_layers: 999,
                context_tokens: 65_536,
                batch_size: 512,
                ubatch_size: 256,
                threads: 8,
                flash_attention: true,
                cache_type_k: KvCacheType::F16,
                cache_type_v: KvCacheType::F16,
                mmap: true,
                unified_kv: false,
                slot_count: 1,
                mtp: None,
                smoke_timeout_seconds: 600,
                expected_speed: "smoke".to_owned(),
            }],
        },
    };
    let plan = resolve_execution_plan(&function, &model, host.static_capabilities()).unwrap();
    let run_id = RunId::generate();
    let turn_id = TurnId::generate();
    let request = InferenceRequest {
        run_id: run_id.clone(),
        turn_id: turn_id.clone(),
        attempt_id: AttemptId::generate(),
        session_id: None,
        request_id: None,
        rendered: RenderedModelRequest {
            run_id,
            turn_id,
            request_index: 0,
            dialect: ModelDialect::Generic,
            tool_call_format: ToolCallFormat::StructuredToolCalls,
            messages: vec![RenderedMessage {
                role: RenderedMessageRole::User,
                content: Some(Content::text("Reply with exactly: AGL_OK").unwrap()),
                name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        },
    };
    let context_key = plan.context_key(request.run_id.as_str());
    let artifacts = vec![ArtifactFileHandle {
        role: ModelArtifactRole::Main,
        basename: model_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        path: model_path,
    }];
    let first_response = host
        .submit(
            plan.clone(),
            request.clone(),
            artifacts.clone(),
            VolatileHandles::default(),
        )
        .await
        .unwrap();
    assert!(!first_response.content.trim().is_empty());
    assert_eq!(
        first_response.metadata.selected_device.as_deref(),
        Some("Vulkan0")
    );
    assert_eq!(host.status().resident_models, 1);
    assert_eq!(host.status().resident_contexts, 1);

    let mut second_request = request;
    second_request.attempt_id = AttemptId::generate();
    second_request.rendered.request_index = 1;
    let second_response = host
        .submit(
            plan.clone(),
            second_request,
            artifacts,
            VolatileHandles::default(),
        )
        .await
        .unwrap();
    assert!(!second_response.content.trim().is_empty());
    assert_eq!(
        second_response.metadata.selected_device.as_deref(),
        Some("Vulkan0")
    );
    assert_eq!(
        first_response.metadata.model_state, second_response.metadata.model_state,
        "same ModelKey must reuse the exact resident server generation"
    );

    assert!(host.clear_context(&context_key).unwrap());
    assert_eq!(host.status().resident_contexts, 0);
    assert!(host.unload_model_digest(plan.model_key().as_str()).unwrap());
    let status = host.status();
    assert_eq!(status.resident_models, 0);
    assert_eq!(status.reserved.host_bytes, 0);
}

fn required_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("{name} is required")))
}

fn sha256(path: &Path) -> String {
    let mut file = File::open(path).unwrap();
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let mut output = String::with_capacity(64);
    for byte in digest.finalize() {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").unwrap();
    }
    output
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
