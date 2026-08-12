use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use agl_config::{ModelDialect, ToolCallFormat};
use agl_ids::{AttemptId, RunId, TurnId};
use agl_inference::{
    AttemptJournal, EngineExecutable, InferenceAttemptMachine, InferenceAttemptPhase,
    InferenceAttemptTransition, InferenceHost, InferenceHostConfig, InferenceHostStartError,
    InferencePlanRejectionEvidence, InferenceRequest,
};
use agl_model::{
    GenerationPolicy, ModelPackage, ModelPackageId, ModelPlanRejection, PackagePlanIdentity,
    ResolvedFunctionPlanInput, ResolvedModelPlanInput, StructuredGenerationMode,
};
use agl_oven::RenderedModelRequest;
use sha2::{Digest, Sha256};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(1);

// MIW-ARCH-004, MIW-TYPE-001 and MIW-ENG-013.
#[test]
fn the_host_supplies_capabilities_from_the_sealed_engine_inventory() {
    let executable = inventory_fixture(false);
    let host = InferenceHost::start(config(&executable)).unwrap();
    assert!(host.static_capabilities().physical_cpu_cores > 0);
    assert_eq!(host.static_capabilities().devices.len(), 1);
    assert_eq!(host.static_capabilities().devices[0].identity, "CPU");
    assert_eq!(host.engine_inventory().llama_cpp_commit, "0123456");
}

// MIW-ENG-001 and MIW-ID-001.
#[test]
fn executable_identity_drift_fails_before_inventory_is_trusted() {
    let executable = inventory_fixture(false);
    let mut bad = config(&executable);
    bad.executable.sha256 = "0".repeat(64);
    assert!(matches!(
        InferenceHost::start(bad),
        Err(InferenceHostStartError::ExecutableIdentityMismatch { .. })
    ));
}

// MIW-TYPE-001 and MIW-ENG-013.
#[test]
fn malformed_native_inventory_is_terminal() {
    let executable = inventory_fixture(true);
    assert!(matches!(
        InferenceHost::start(config(&executable)),
        Err(InferenceHostStartError::InvalidEngineInventory { .. })
    ));
}

// MIW-JRN-004 and MIW-FSM-004. A durable nonterminal attempt is never
// re-executed after host replacement; recovery records one typed failure.
#[test]
fn host_recovery_finishes_a_nonterminal_attempt_without_retry() {
    let executable = inventory_fixture(false);
    let root = std::env::temp_dir().join(format!(
        "agl173-host-recovery-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ));
    let attempt_id = AttemptId::generate();
    let path = root.join(attempt_id.as_str()).join("transitions.jsonl");
    let mut journal = AttemptJournal::create(&path).unwrap();
    let mut machine =
        InferenceAttemptMachine::new(RunId::generate(), TurnId::generate(), attempt_id);
    journal
        .append(
            &mut machine,
            InferenceAttemptTransition::StartAttempt {
                backend: "llama_cpp".to_owned(),
                request_path: PathBuf::from("request.json"),
                projection_root: None,
            },
        )
        .unwrap();
    drop(journal);

    let _host = InferenceHost::start_with_journal_root(config(&executable), &root).unwrap();
    let bytes = fs::read(path).unwrap();
    let replay = AttemptJournal::replay(&bytes).unwrap();
    assert_eq!(replay.machine().phase(), InferenceAttemptPhase::Failed);
    let encoded = String::from_utf8(bytes).unwrap();
    assert_eq!(encoded.matches("host_restarted").count(), 1);
}

// MIW-FSM-004 and MIW-JRN-005. Static planning failure happens after command
// identity allocation and is terminal in the same durable attempt journal.
#[test]
fn plan_rejection_has_exact_identity_and_durable_typed_evidence() {
    let executable = inventory_fixture(false);
    let config = config(&executable);
    let journal_root = executable.parent().unwrap().join("journals");
    let host = InferenceHost::start_with_journal_root(config.clone(), &journal_root).unwrap();
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
            messages: Vec::new(),
            tools: Vec::new(),
        },
    };
    let function = ResolvedFunctionPlanInput {
        package: package_identity("function:rejected@=1.0.0", 'a'),
        selected_profile_id: "gpu-required".to_owned(),
        generation_policy: GenerationPolicy::greedy(
            32,
            Vec::new(),
            StructuredGenerationMode::Disabled,
            false,
        )
        .unwrap(),
        prompt_template_digest: digest('b'),
        visible_tools_digest: digest('c'),
    };
    let model = ResolvedModelPlanInput {
        package: package_identity("model:rejected@=1.0.0", 'd'),
        payload_schema: "agentlibre.model/v3".to_owned(),
        model: ModelPackage {
            id: ModelPackageId::new("rejected").unwrap(),
            provenance: None,
            display_name: "Rejected fixture".to_owned(),
            capabilities: Vec::new(),
            license: "test".to_owned(),
            license_url: "https://example.invalid".to_owned(),
            repository: "test/rejected".to_owned(),
            revision: "0".repeat(40),
            artifacts: Vec::new(),
            profiles: Vec::new(),
        },
    };
    let rejection = ModelPlanRejection::UnknownProfile {
        profile_id: function.selected_profile_id.clone(),
    };
    let projection_root = config
        .evidence_root
        .join(request.run_id.as_str())
        .join("attempts")
        .join(request.attempt_id.as_str());
    fs::create_dir_all(&projection_root).unwrap();

    host.record_plan_rejection(
        &request,
        InferencePlanRejectionEvidence::new(
            &function,
            &model,
            rejection,
            Some(serde_json::json!({"source": "product"})),
        ),
        Some(&projection_root),
    )
    .unwrap();

    let path = journal_root
        .join(request.attempt_id.as_str())
        .join("transitions.jsonl");
    let bytes = fs::read(path).unwrap();
    let replay = AttemptJournal::replay(&bytes).unwrap();
    assert_eq!(replay.machine().phase(), InferenceAttemptPhase::Failed);
    let encoded = String::from_utf8(bytes).unwrap();
    assert!(encoded.contains(request.run_id.as_str()));
    assert!(encoded.contains(request.attempt_id.as_str()));
    assert!(encoded.contains("unknown_profile"));
    assert!(encoded.contains("function:rejected@=1.0.0"));
    assert!(encoded.contains("model:rejected@=1.0.0"));
}

// MIW-ADM-004 and MIW-SUP-002. The host and selected-device leases are held
// for the complete host lifetime and become available only after handoff.
#[test]
fn a_second_host_cannot_oversubscribe_the_same_authority() {
    let executable = inventory_fixture(false);
    let config = config(&executable);
    let first = InferenceHost::start(config.clone()).unwrap();
    assert!(matches!(
        InferenceHost::start(config.clone()),
        Err(InferenceHostStartError::LeaseUnavailable { .. })
    ));
    assert_eq!(first.status().authority_leases, 1);
    drop(first);
    InferenceHost::start(config).unwrap();
}

fn config(path: &Path) -> InferenceHostConfig {
    InferenceHostConfig {
        executable: EngineExecutable {
            path: path.to_path_buf(),
            sha256: sha256(path),
        },
        queue_capacity: 2,
        external_host_reserve_bytes: 0,
        authority_root: path.parent().unwrap().join("authority"),
        context_idle_duration: std::time::Duration::from_secs(900),
        model_idle_duration: std::time::Duration::from_secs(300),
        evidence_root: path.parent().unwrap().join("evidence"),
    }
}

fn inventory_fixture(malformed: bool) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "agl173-inventory-{}-{}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
        if malformed { "bad" } else { "good" }
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let path = root.join("llama-server");
    let payload = if malformed {
        r#"{"schema":"wrong","llama_cpp_commit":"0123456","devices":[]}"#
    } else {
        r#"{"schema":"agentlibre.llama-inventory/v1","llama_cpp_commit":"0123456","devices":[{"identity":"CPU","description":"fixture","native_device_id":"","kind":"cpu","available_pool_bytes":1073741824,"physical_pool_bytes":1073741824}]}"#
    };
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' '{}' >&$AGL_LLAMA_SERVER_INVENTORY_FD\n",
            payload
        ),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn sha256(path: &Path) -> String {
    let mut file = fs::File::open(path).unwrap();
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
