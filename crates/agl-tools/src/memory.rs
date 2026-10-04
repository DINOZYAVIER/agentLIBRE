//! Native memory request boundary. The daemon host owns the derived index
//! and current Run tokenizer; registration does not admit this Tool to a Run.
use std::sync::Arc;

use agl_core::agent::{ToolFailure, ToolFailureKind};
use agl_core::package::{InMemoryPackageView, PackageRelativePath};
use agl_tool_api::{ExtensionBindings, ToolBinding, ToolContext, ToolFuture, ToolHandler};
use serde::Deserialize;
use serde_json::{Value, json};

pub const SEARCH: &str = "agentlibre.memory:search";

pub fn declaration() -> InMemoryPackageView {
    InMemoryPackageView::new([
        (
            PackageRelativePath::new("EXTENSION.toml").unwrap(),
            include_bytes!("../../../extensions/agentlibre-memory/EXTENSION.toml").to_vec(),
        ),
        (
            PackageRelativePath::new("schemas/search.json").unwrap(),
            include_bytes!("../../../extensions/agentlibre-memory/schemas/search.json").to_vec(),
        ),
    ])
    .expect("native memory package paths are unique")
}

pub fn bindings(
    version: agl_core::package::PackageVersion,
    content_digest: agl_core::package::PackageTreeDigest,
    definition: agl_core::ExtensionDefinition,
    host: Arc<dyn ToolHandler>,
) -> anyhow::Result<ExtensionBindings> {
    crate::validate_declaration(
        &declaration(),
        &content_digest,
        &definition,
        "agentlibre.memory",
        &[SEARCH],
    )?;
    let tool = &definition.tools[0];
    Ok(ExtensionBindings {
        version,
        content_digest,
        tools: vec![ToolBinding {
            tool_id: tool.id.clone(),
            definition_digest: crate::definition_digest(tool),
            handler: Arc::new(Handler { host }),
        }],
        definition,
        allows_authority: Arc::new(|_| false),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    query: String,
    #[serde(default = "default_limit", deserialize_with = "deserialize_limit")]
    limit: i64,
}
fn default_limit() -> i64 {
    5
}
fn deserialize_limit<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
    let value = serde_json::Number::deserialize(deserializer)?;
    if let Some(value) = value.as_i64() {
        return Ok(value.clamp(1, 10));
    }
    if value.as_u64().is_some() {
        return Ok(10);
    }
    Err(serde::de::Error::custom("limit must be an integer"))
}
struct Handler {
    host: Arc<dyn ToolHandler>,
}
impl ToolHandler for Handler {
    fn call(&self, context: ToolContext, input: Value) -> ToolFuture {
        let host = self.host.clone();
        Box::pin(async move {
            let input: Input = serde_json::from_value(input).map_err(|_| invalid_input())?;
            if input.query.trim().is_empty() {
                return Err(invalid_input());
            }
            if context.is_cancelled() {
                return Err(ToolFailure::no_effect(
                    ToolFailureKind::Cancelled,
                    None,
                    &[],
                ));
            }
            host.call(context, json!({"query":input.query,"limit":input.limit}))
                .await
        })
    }
}
fn invalid_input() -> ToolFailure {
    ToolFailure::no_effect(
        ToolFailureKind::InvalidInput,
        Some("query"),
        &["Provide a nonempty English query using topic names, identifiers or file paths."],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn limits_cover_json_integers_without_overflow() {
        for (input, expected) in [
            (json!({"query":"test"}), 5),
            (json!({"query":"test","limit":0}), 1),
            (json!({"query":"test","limit":i64::MIN}), 1),
            (json!({"query":"test","limit":u64::MAX}), 10),
            (json!({"query":"test","limit":7}), 7),
        ] {
            assert_eq!(
                serde_json::from_value::<Input>(input).unwrap().limit,
                expected
            );
        }
        for input in [
            json!({"query":"test","limit":null}),
            json!({"query":"test","limit":1.5}),
            json!({"query":"test","unknown":1}),
            json!({"query":5}),
            json!({"limit":5}),
        ] {
            assert!(serde_json::from_value::<Input>(input).is_err());
        }
    }
    struct Host(std::sync::Mutex<Vec<Value>>);
    impl ToolHandler for Host {
        fn call(&self, _: ToolContext, input: Value) -> ToolFuture {
            self.0.lock().unwrap().push(input);
            Box::pin(async {
                Err(ToolFailure::no_effect(
                    ToolFailureKind::Unavailable,
                    None,
                    &[],
                ))
            })
        }
    }
    fn context() -> ToolContext {
        ToolContext::new(
            agl_core::agent::AgentOperationKey {
                run_id: agl_core::AgentRunId::generate(),
                ordinal: std::num::NonZeroU32::MIN,
            },
            None,
            agl_core::agent::WorkspaceScope {
                root: agl_core::agent::AbsolutePath::try_from("/tmp".to_owned()).unwrap(),
                working_directory: agl_core::agent::RelativePath::try_from(".".to_owned()).unwrap(),
            },
            agl_core::agent::AuthorityGrantSet(vec![]),
            i64::MAX,
            65536,
            agl_tool_api::ToolCancellation::new(),
        )
    }
    #[tokio::test]
    async fn native_boundary_normalizes_before_host_and_refuses_cancelled_calls() {
        let host = Arc::new(Host(std::sync::Mutex::new(vec![])));
        let handler = Handler { host: host.clone() };
        let failure = handler
            .call(context(), json!({"query":"HTTPServer", "limit":u64::MAX}))
            .await
            .unwrap_err();
        assert_eq!(failure.kind, ToolFailureKind::Unavailable);
        assert_eq!(
            *host.0.lock().unwrap(),
            [json!({"query":"HTTPServer", "limit":10})]
        );
        let cancelled = context();
        cancelled.cancellation().cancel();
        assert_eq!(
            handler
                .call(cancelled, json!({"query":"topic"}))
                .await
                .unwrap_err()
                .kind,
            ToolFailureKind::Cancelled
        );
        assert_eq!(
            handler
                .call(context(), json!({"query":"  "}))
                .await
                .unwrap_err()
                .kind,
            ToolFailureKind::InvalidInput
        );
        assert_eq!(host.0.lock().unwrap().len(), 1);
    }
}
