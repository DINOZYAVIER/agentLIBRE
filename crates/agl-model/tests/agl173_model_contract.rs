#[path = "agl173/model_support.rs"]
mod model_support;

use agl_model::{
    MODEL_PAYLOAD_SCHEMA, ModelArtifactRole, ModelPlanRejection, ProfileMismatchPredicate,
    resolve_execution_plan,
};
use model_support::{ArtifactFileFixture, PlanFixture, RoleFixture};

fn plan(fixture: &PlanFixture) -> agl_model::ModelExecutionPlan {
    resolve_execution_plan(
        fixture.resolved_function(),
        fixture.resolved_model(),
        fixture.host_capabilities(),
    )
    .expect("canonical fixture must resolve")
}

// MIW-MODEL-001. v3 is the only accepted schema and its canonical fixture
// projects every runtime/resource field into the opaque plan.
#[test]
fn model_v3_is_complete_and_v2_is_rejected() {
    assert_eq!(MODEL_PAYLOAD_SCHEMA, "agentlibre.model/v3");

    let fixture = PlanFixture::canonical_v3();
    let selected = plan(&fixture);
    assert_eq!(selected.profile_id(), "vulkan0-32k");
    assert_eq!(selected.runtime().context_tokens(), 32_768);
    assert_eq!(selected.runtime().batch_size(), 2_048);
    assert_eq!(selected.runtime().ubatch_size(), 512);
    assert_eq!(selected.runtime().threads(), 16);
    assert_eq!(selected.runtime().gpu_layers(), 61);
    assert!(selected.runtime().flash_attention());
    assert_eq!(selected.runtime().key_cache_type(), "q8_0");
    assert_eq!(selected.runtime().value_cache_type(), "q8_0");
    assert!(selected.runtime().mmap());
    assert!(!selected.runtime().unified_kv());
    assert_eq!(selected.runtime().slot_count(), 1);
    assert_eq!(selected.resources().host_private_bytes(), 64 << 20);
    assert_eq!(selected.resources().device_private_bytes(), 24 << 30);
    assert_eq!(selected.resources().shared_bytes(), 512 << 20);

    let legacy = PlanFixture::canonical_v3().with_payload_schema("agentlibre.model/v2");
    assert!(matches!(
        resolve_execution_plan(
            legacy.resolved_function(),
            legacy.resolved_model(),
            legacy.host_capabilities()
        ),
        Err(ModelPlanRejection::UnsupportedModelSchema { schema, .. })
            if schema == "agentlibre.model/v2"
    ));
}

// MIW-MODEL-002. Exact profile matching rejects drift; it never clamps or
// substitutes the package-owned load/process shape.
#[test]
fn exact_profile_values_are_not_clamped_or_reselected() {
    let fixture = PlanFixture::canonical_v3();
    let selected = plan(&fixture);
    let declared = &fixture.resolved_model().model.profiles[0];
    assert_eq!(selected.runtime().threads(), declared.threads);
    assert_eq!(selected.runtime().batch_size(), declared.batch_size);
    assert_eq!(selected.runtime().ubatch_size(), declared.ubatch_size);
    assert_eq!(selected.runtime().gpu_layers(), declared.gpu_layers);
    assert_eq!(
        selected.runtime().flash_attention(),
        declared.flash_attention
    );
    assert_eq!(selected.runtime().mmap(), declared.mmap);
    assert_eq!(selected.runtime().unified_kv(), declared.unified_kv);
}

// MIW-MODEL-004. Audit, resident-model and context identities have separate
// domains and absolute filesystem placement is never identity material.
#[test]
fn plan_model_and_context_identities_change_only_for_their_owned_fields() {
    let base = plan(&PlanFixture::canonical_v3());

    for changed in PlanFixture::canonical_v3().every_audit_field_variant() {
        assert_ne!(
            plan(&changed).digest(),
            base.digest(),
            "{}",
            changed.label()
        );
    }

    for changed in PlanFixture::canonical_v3().sampling_only_variants() {
        let changed = plan(&changed);
        assert_ne!(changed.digest(), base.digest());
        assert_eq!(changed.model_key(), base.model_key());
    }

    for changed in PlanFixture::canonical_v3().native_load_variants() {
        let changed = plan(&changed);
        assert_ne!(changed.model_key(), base.model_key());
        assert_ne!(
            changed.context_key("conversation-a"),
            base.context_key("conversation-a")
        );
    }

    let other_conversation = base.context_key("conversation-b");
    assert_ne!(other_conversation, base.context_key("conversation-a"));

    let moved = plan(&PlanFixture::canonical_v3());
    assert_eq!(moved.digest(), base.digest());
    assert_eq!(moved.model_key(), base.model_key());
    assert_eq!(
        moved.context_key("conversation-a"),
        base.context_key("conversation-a")
    );
}

// MIW-MODEL-005. Static mismatch lists every failed predicate and ignores live
// available-memory pressure, which belongs to agl-inference admission.
#[test]
fn static_mismatch_is_complete_and_free_memory_is_not_a_planning_input() {
    let mismatch = PlanFixture::canonical_v3()
        .with_device_pci("ffff", "eeee")
        .with_physical_host_bytes(2 << 30)
        .with_physical_device_bytes(4 << 30);
    let error = resolve_execution_plan(
        mismatch.resolved_function(),
        mismatch.resolved_model(),
        mismatch.host_capabilities(),
    )
    .unwrap_err();
    let ModelPlanRejection::StaticMismatch { predicates, .. } = error else {
        panic!("expected typed static mismatch")
    };
    assert!(predicates.contains(&ProfileMismatchPredicate::PciDeviceId));
    assert!(predicates.contains(&ProfileMismatchPredicate::PciSubsystemId));
    assert!(predicates.contains(&ProfileMismatchPredicate::HostPhysicalBytes));
    assert!(predicates.contains(&ProfileMismatchPredicate::DevicePhysicalBytes));

    let pressured = PlanFixture::canonical_v3().with_live_available_bytes(1, 1);
    assert_eq!(
        plan(&pressured).digest(),
        plan(&PlanFixture::canonical_v3()).digest()
    );
}

// MIW-MODEL-006. Each role owns a non-empty ordered file set with safe
// basenames, byte sizes and SHA-256; no shard is discovered from a cache.
#[test]
fn ordered_role_files_are_complete_and_path_independent() {
    let split = PlanFixture::canonical_v3().replace_role(
        RoleFixture::new(ModelArtifactRole::Main, "model-main")
            .file(ArtifactFileFixture::new(
                "model-00001-of-00002.gguf",
                10,
                "1111111111111111111111111111111111111111111111111111111111111111",
            ))
            .file(ArtifactFileFixture::new(
                "model-00002-of-00002.gguf",
                20,
                "2222222222222222222222222222222222222222222222222222222222222222",
            )),
    );
    let selected = plan(&split);
    let main = selected.artifact_role(ModelArtifactRole::Main).unwrap();
    assert_eq!(main.files().len(), 2);
    assert_eq!(main.files()[0].basename(), "model-00001-of-00002.gguf");
    assert_eq!(main.files()[1].basename(), "model-00002-of-00002.gguf");
    assert_eq!(main.files()[0].byte_size(), 10);
    assert_eq!(main.files()[1].byte_size(), 20);

    for invalid in [
        PlanFixture::canonical_v3().empty_role(ModelArtifactRole::Main),
        PlanFixture::canonical_v3().role_file_basename(ModelArtifactRole::Main, "../model.gguf"),
        PlanFixture::canonical_v3().role_file_basename(ModelArtifactRole::Main, "/model.gguf"),
        PlanFixture::canonical_v3().role_file_size(ModelArtifactRole::Main, 0),
        PlanFixture::canonical_v3().role_file_sha256(ModelArtifactRole::Main, "not-a-digest"),
    ] {
        assert!(
            resolve_execution_plan(
                invalid.resolved_function(),
                invalid.resolved_model(),
                invalid.host_capabilities()
            )
            .is_err()
        );
    }
}

// MIW-MODEL-007. Model owns immutable load/process shape; Function owns the
// closed per-attempt policy. Both projections are frozen in the plan.
#[test]
fn package_owners_project_typed_runtime_and_generation_policy() {
    let fixture = PlanFixture::canonical_v3();
    let selected = plan(&fixture);
    assert!(selected.generation_policy().is_greedy());
    assert_eq!(selected.generation_policy().max_output_tokens(), 512);
    assert_eq!(selected.generation_policy().stop_rules().len(), 2);
    assert_eq!(
        selected.generation_policy().structured_mode().as_str(),
        "lazy_tool"
    );
    assert!(selected.generation_policy().repair_malformed_tool_calls());
    let runtime = serde_json::to_value(selected.runtime()).unwrap();
    let policy = serde_json::to_value(selected.generation_policy()).unwrap();
    assert!(runtime.get("argv").is_none());
    assert!(policy.get("parameters").is_none());
}
