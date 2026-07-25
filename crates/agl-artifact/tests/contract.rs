use agl_artifact::{
    AglCompatibility, ArtifactAdapterDescriptor, ArtifactAdapterRegistry, ArtifactEnvelope,
    ArtifactError, ArtifactPackageRef, ArtifactTypeId, ArtifactVersion, ArtifactVersionReq,
    EXTENSION_ROOT, FUNCTION_ROOT, SKILL_ROOT,
};

fn version(value: &str) -> ArtifactVersion {
    value.parse().unwrap()
}

fn envelope_json() -> serde_json::Value {
    serde_json::json!({
        "schema": "agentlibre.artifact/v1",
        "type": "function",
        "id": "example",
        "version": "1.0.0",
        "payload_schema": "agentlibre.function/v2",
        "agl": {
            "compatible": ">=1.0.0, <2.0.0",
            "tested": ["1.0.0"]
        },
        "requires": ["skill:vendor/workflow@^1.0"]
    })
}

#[test]
fn envelope_wire_round_trip_is_canonical() {
    let envelope: ArtifactEnvelope = serde_json::from_value(envelope_json()).unwrap();
    assert_eq!(serde_json::to_value(&envelope).unwrap(), envelope_json());
}

#[test]
fn envelope_wire_rejects_unknown_fields_and_invalid_evidence() {
    let mut unknown = envelope_json();
    unknown["unexpected"] = serde_json::json!(true);
    let error = serde_json::from_value::<ArtifactEnvelope>(unknown).unwrap_err();
    assert!(error.to_string().contains("unknown field"));

    let mut empty_tested = envelope_json();
    empty_tested["agl"]["tested"] = serde_json::json!([]);
    assert!(serde_json::from_value::<ArtifactEnvelope>(empty_tested).is_err());
}

#[test]
fn reference_and_semver_serde_use_strings() {
    let reference: ArtifactPackageRef = "model:vendor/base@>=1.0.0, <2.0.0".parse().unwrap();
    let encoded = serde_json::to_string(&reference).unwrap();
    assert_eq!(encoded, r#""model:vendor/base@>=1.0.0, <2.0.0""#);
    assert_eq!(
        serde_json::from_str::<ArtifactPackageRef>(&encoded).unwrap(),
        reference
    );

    let requirement: ArtifactVersionReq = "^1.2.0".parse().unwrap();
    assert_eq!(serde_json::to_string(&requirement).unwrap(), r#""^1.2.0""#);
    assert!(requirement.matches(&version("1.5.0")));
    assert!(!requirement.matches(&version("2.0.0")));

    let prerelease: ArtifactVersionReq = ">=1.0.0-alpha.1, <2.0.0".parse().unwrap();
    assert!(prerelease.matches(&version("1.0.0-alpha.2")));
    assert!(prerelease.matches(&version("1.0.0")));
}

#[test]
fn compatibility_serde_validates_tested_versions() {
    let empty = serde_json::json!({
        "compatible": ">=1.0.0",
        "tested": []
    });
    assert!(serde_json::from_value::<AglCompatibility>(empty).is_err());
}

#[test]
fn registry_rejects_duplicate_and_reserved_roots() {
    let function =
        ArtifactAdapterDescriptor::new(ArtifactTypeId::function(), FUNCTION_ROOT, "FUNCTION.md")
            .unwrap();
    let duplicate_type =
        ArtifactAdapterDescriptor::new(ArtifactTypeId::function(), FUNCTION_ROOT, "other.md")
            .unwrap();
    assert!(matches!(
        ArtifactAdapterRegistry::new([function.clone(), duplicate_type]),
        Err(ArtifactError::DuplicateAdapterType { .. })
    ));

    let skill =
        ArtifactAdapterDescriptor::new(ArtifactTypeId::skill(), SKILL_ROOT, "SKILL.md").unwrap();
    let duplicate_root =
        ArtifactAdapterDescriptor::new("vendor.workflow".parse().unwrap(), SKILL_ROOT, "entry.md")
            .unwrap();
    assert!(matches!(
        ArtifactAdapterRegistry::new([skill, duplicate_root]),
        Err(ArtifactError::DuplicateAdapterRoot { .. })
    ));

    let reserved = ArtifactAdapterDescriptor::new(
        "vendor.workflow".parse().unwrap(),
        EXTENSION_ROOT,
        "entry.md",
    )
    .unwrap();
    assert!(matches!(
        ArtifactAdapterRegistry::new([reserved]),
        Err(ArtifactError::ReservedRootCollision { .. })
    ));
}

#[test]
fn adapter_descriptor_rejects_path_like_values() {
    let type_id = ArtifactTypeId::function();
    assert!(matches!(
        ArtifactAdapterDescriptor::new(type_id.clone(), "", "FUNCTION.md"),
        Err(ArtifactError::InvalidAdapterRoot { .. })
    ));
    assert!(matches!(
        ArtifactAdapterDescriptor::new(type_id, FUNCTION_ROOT, "nested/FUNCTION.md"),
        Err(ArtifactError::InvalidAdapterEntrypoint { .. })
    ));
}
