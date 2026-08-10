use agl_protocol::{ExtensionCatalogDto, ExtensionStateDto};

// AGL171-016. CLI, TUI and GUI consume this one wire DTO rather than parsing
// Extension packages independently.
#[test]
fn extension_query_dto_round_trips_every_runtime_state() {
    let value = serde_json::json!({
        "schema": "agentlibre.extension-catalog/v1",
        "catalog_digest": format!("sha256:{}", "a".repeat(64)),
        "extensions": [
            {
                "id": "example.compiled",
                "declaration_digest": format!("sha256:{}", "1".repeat(64)),
                "compiled": true,
                "selected": false,
                "admitted": false,
                "unavailable_reason": null
            },
            {
                "id": "example.unavailable",
                "declaration_digest": format!("sha256:{}", "2".repeat(64)),
                "compiled": true,
                "selected": false,
                "admitted": false,
                "unavailable_reason": "missing host.clock"
            }
        ]
    });
    let dto: ExtensionCatalogDto = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(&dto).unwrap(), value);
    assert_eq!(dto.extensions.len(), 2);

    fn shared_consumer_type(_: &ExtensionStateDto) {}
    for state in &dto.extensions {
        shared_consumer_type(state);
    }
}
