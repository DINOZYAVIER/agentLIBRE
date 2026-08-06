use agl_kernel::{
    DeclarationError, HookBatchRequest, OperationKind, ToolDeclaration, ToolId, ToolSchema,
};
use schemars::JsonSchema;
use serde_json::{Value, json};

#[derive(JsonSchema)]
#[allow(dead_code)]
struct ReadArgs {
    path: String,
    options: ReadOptions,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
struct ReadOptions {
    limit: Option<u32>,
}

fn tool_id() -> ToolId {
    ToolId::new("example.schema:read").unwrap()
}

// Retained AGL-154 invariant. Mutation: emit an open nested object schema.
#[test]
fn generated_input_schema_is_draft_2020_12_and_closes_every_object() {
    let declaration =
        ToolDeclaration::from_schema::<ReadArgs>(tool_id(), "Read a file", OperationKind::Read)
            .unwrap();

    assert_eq!(
        declaration.input_schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(
        declaration.input_schema["additionalProperties"],
        Value::Bool(false)
    );
    let encoded = serde_json::to_string(&declaration.input_schema).unwrap();
    assert!(
        encoded.matches("\"additionalProperties\":false").count() >= 2,
        "nested object was not closed: {encoded}"
    );

    let schema = declaration.compile_schema().unwrap();
    schema
        .validate(&json!({"path": "README.md", "options": {"limit": 3}}))
        .unwrap();
    for invalid in [
        json!({}),
        json!({"path": 7, "options": {"limit": 3}}),
        json!({"path": "README.md", "options": {"extra": true}}),
        json!({"path": "README.md", "options": {}, "extra": true}),
    ] {
        assert!(schema.validate(&invalid).is_err(), "accepted {invalid}");
    }
}

// Retained AGL-154 invariant. Mutation: accept an open, incomplete or wrong-draft schema.
#[test]
fn declaration_rejects_invalid_or_incomplete_input_schemas() {
    let invalid_schema = ToolDeclaration::new(
        tool_id(),
        "Broken",
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": 42
        }),
        OperationKind::Read,
    )
    .unwrap_err();
    assert!(matches!(invalid_schema, DeclarationError::InvalidSchema(_)));

    for schema in [
        json!({}),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"value": {"type": "string"}}
        }),
        json!({
            "$schema": "https://json-schema.org/draft/2019-09/schema",
            "type": "object",
            "additionalProperties": false
        }),
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "nested": {
                    "type": "object",
                    "properties": {"value": {"type": "string"}}
                }
            },
            "additionalProperties": false
        }),
    ] {
        assert!(matches!(
            ToolDeclaration::new(tool_id(), "Incomplete", schema, OperationKind::Read),
            Err(DeclarationError::IncompleteSchema(_))
        ));
    }
}

// Retained schema diagnostic invariant. Mutation: report every oneOf branch instead of the closest.
#[test]
fn one_of_failure_reports_only_the_closest_actionable_branch() {
    let schema = ToolSchema::compile(&json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "operation": {
                "oneOf": [
                    {
                        "type": "object",
                        "properties": {
                            "op": {"const": "create"},
                            "path": {"type": "string"},
                            "expected_absent": {"type": "boolean"}
                        },
                        "required": ["op", "path", "expected_absent"],
                        "additionalProperties": false
                    },
                    {
                        "type": "object",
                        "properties": {
                            "op": {"const": "delete"},
                            "path": {"type": "string"},
                            "expected_digest": {"type": "string"}
                        },
                        "required": ["op", "path", "expected_digest"],
                        "additionalProperties": false
                    }
                ]
            }
        },
        "required": ["operation"],
        "additionalProperties": false
    }))
    .unwrap();

    let message = schema
        .validate(&json!({"operation": {"action": "create", "path": "x"}}))
        .unwrap_err()
        .to_string();
    assert!(message.contains("'action' was unexpected"));
    assert!(message.contains("\"expected_absent\" is a required property"));
    assert!(message.contains("\"op\" is a required property"));
    assert!(!message.contains("not valid under any of the schemas"));
}

// KCT-CHK-001. Mutation: accept unknown fields in a kernel request DTO.
#[test]
fn kernel_declarations_and_request_dtos_reject_unknown_fields() {
    let mut declaration = serde_json::to_value(
        ToolDeclaration::from_schema::<ReadArgs>(tool_id(), "Read a file", OperationKind::Read)
            .unwrap(),
    )
    .unwrap();
    declaration["legacy_visibility"] = json!(true);
    assert!(serde_json::from_value::<ToolDeclaration>(declaration).is_err());

    assert!(
        serde_json::from_value::<HookBatchRequest>(json!({
            "event": "turn_finish",
            "hooks": [],
            "payload": {},
            "legacy": true
        }))
        .is_err()
    );
}
