use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter, Write as _};
use std::io::BufRead;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::SafeRuntimeEventEnvelope;

pub const SEMANTIC_TRACE_SCHEMA: &str = "agentlibre.semantic-trace.v1alpha";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticTraceIdentity {
    pub agl_version: String,
    pub extension_catalog_digest: String,
    pub function_digest: String,
    pub tool_set_digest: String,
    pub policy_digest: String,
    pub workflow_digest: String,
    pub inference_digest: String,
}

pub fn export_semantic_trace<R: BufRead>(
    reader: R,
    identity: SemanticTraceIdentity,
    content_refs: Vec<SemanticContentRef>,
) -> Result<SemanticTrace, SemanticTraceError> {
    let mut events = Vec::new();
    for (index, line) in reader.lines().enumerate() {
        let line_number = index + 1;
        let line = line.map_err(SemanticTraceError::Read)?;
        if line.trim().is_empty() {
            continue;
        }
        let event = serde_json::from_str::<SafeRuntimeEventEnvelope>(&line).map_err(|source| {
            SemanticTraceError::InvalidCanonicalLogLine {
                line: line_number,
                source,
            }
        })?;
        events.push(event);
    }
    if content_refs.is_empty() {
        SemanticTrace::redacted(identity, events)
    } else {
        SemanticTrace::with_content_refs(identity, events, content_refs)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticContentRef {
    pub digest: String,
    pub local_artifact_path: String,
    pub media_type: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticTrace {
    pub schema: String,
    pub identity: SemanticTraceIdentity,
    pub payload_policy: String,
    pub content_refs: Vec<SemanticContentRef>,
    pub events: Vec<SafeRuntimeEventEnvelope>,
    pub trace_digest: String,
}

impl SemanticTrace {
    pub fn redacted(
        identity: SemanticTraceIdentity,
        events: Vec<SafeRuntimeEventEnvelope>,
    ) -> Result<Self, SemanticTraceError> {
        Self::new(identity, events, Vec::new())
    }

    pub fn with_content_refs(
        identity: SemanticTraceIdentity,
        events: Vec<SafeRuntimeEventEnvelope>,
        content_refs: Vec<SemanticContentRef>,
    ) -> Result<Self, SemanticTraceError> {
        Self::new(identity, events, content_refs)
    }

    fn new(
        identity: SemanticTraceIdentity,
        events: Vec<SafeRuntimeEventEnvelope>,
        mut content_refs: Vec<SemanticContentRef>,
    ) -> Result<Self, SemanticTraceError> {
        validate_identity(&identity)?;
        validate_events(&events)?;
        for reference in &content_refs {
            validate_content_ref(reference)?;
        }
        content_refs.sort_by(|left, right| {
            (&left.digest, &left.local_artifact_path)
                .cmp(&(&right.digest, &right.local_artifact_path))
        });
        content_refs.dedup();
        let mut trace = Self {
            schema: SEMANTIC_TRACE_SCHEMA.to_owned(),
            identity,
            payload_policy: if content_refs.is_empty() {
                "redacted_metadata".to_owned()
            } else {
                "local_content_addressed_refs".to_owned()
            },
            content_refs,
            events,
            trace_digest: String::new(),
        };
        trace.trace_digest = trace.compute_digest()?;
        Ok(trace)
    }

    pub fn render(&self) -> Result<String, SemanticTraceError> {
        verify_trace_digest(self)?;
        serde_json::to_string(self).map_err(SemanticTraceError::Serialize)
    }

    fn compute_digest(&self) -> Result<String, SemanticTraceError> {
        let mut unsigned = self.clone();
        unsigned.trace_digest.clear();
        let value = serde_json::to_value(unsigned).map_err(SemanticTraceError::Serialize)?;
        let canonical = canonical_json(&value);
        Ok(sha256(canonical.as_bytes()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SemanticDrift {
    pub field: String,
    pub expected: String,
    pub actual: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SemanticReplayReport {
    pub event_count: usize,
    pub drifts: Vec<SemanticDrift>,
}

impl SemanticReplayReport {
    pub fn matches(&self) -> bool {
        self.drifts.is_empty()
    }
}

pub fn replay_semantic_trace(
    encoded: &str,
    expected: &SemanticTraceIdentity,
) -> Result<SemanticReplayReport, SemanticTraceError> {
    let trace: SemanticTrace =
        serde_json::from_str(encoded).map_err(SemanticTraceError::Deserialize)?;
    if trace.schema != SEMANTIC_TRACE_SCHEMA {
        return Err(SemanticTraceError::UnsupportedSchema(trace.schema));
    }
    validate_identity(expected)?;
    validate_identity(&trace.identity)?;
    verify_trace_digest(&trace)?;
    validate_events(&trace.events)?;
    for reference in &trace.content_refs {
        validate_content_ref(reference)?;
    }
    let mut drifts = Vec::new();
    compare_identity_field(
        &mut drifts,
        "agl_version",
        &expected.agl_version,
        &trace.identity.agl_version,
    );
    compare_identity_field(
        &mut drifts,
        "extension_catalog_digest",
        &expected.extension_catalog_digest,
        &trace.identity.extension_catalog_digest,
    );
    compare_identity_field(
        &mut drifts,
        "function_digest",
        &expected.function_digest,
        &trace.identity.function_digest,
    );
    compare_identity_field(
        &mut drifts,
        "tool_set_digest",
        &expected.tool_set_digest,
        &trace.identity.tool_set_digest,
    );
    compare_identity_field(
        &mut drifts,
        "policy_digest",
        &expected.policy_digest,
        &trace.identity.policy_digest,
    );
    compare_identity_field(
        &mut drifts,
        "workflow_digest",
        &expected.workflow_digest,
        &trace.identity.workflow_digest,
    );
    compare_identity_field(
        &mut drifts,
        "inference_digest",
        &expected.inference_digest,
        &trace.identity.inference_digest,
    );
    Ok(SemanticReplayReport {
        event_count: trace.events.len(),
        drifts,
    })
}

fn compare_identity_field(
    drifts: &mut Vec<SemanticDrift>,
    field: &str,
    expected: &str,
    actual: &str,
) {
    if expected != actual {
        drifts.push(SemanticDrift {
            field: field.to_owned(),
            expected: expected.to_owned(),
            actual: actual.to_owned(),
        });
    }
}

fn validate_events(events: &[SafeRuntimeEventEnvelope]) -> Result<(), SemanticTraceError> {
    let mut sequences = BTreeMap::new();
    for event in events {
        event
            .validate()
            .map_err(|error| SemanticTraceError::InvalidEvent(error.to_string()))?;
        let previous = sequences
            .insert(event.scope.run_id().clone(), event.sequence)
            .unwrap_or(0);
        if event.sequence != previous + 1 {
            return Err(SemanticTraceError::InvalidEvent(format!(
                "run {} event sequence advanced from {previous} to {}",
                event.scope.run_id(),
                event.sequence
            )));
        }
    }
    Ok(())
}

fn validate_identity(identity: &SemanticTraceIdentity) -> Result<(), SemanticTraceError> {
    if identity.agl_version.trim().is_empty() {
        return Err(SemanticTraceError::InvalidIdentity(
            "agl_version cannot be blank".to_owned(),
        ));
    }
    for (field, digest) in [
        (
            "extension_catalog_digest",
            &identity.extension_catalog_digest,
        ),
        ("function_digest", &identity.function_digest),
        ("tool_set_digest", &identity.tool_set_digest),
        ("policy_digest", &identity.policy_digest),
        ("workflow_digest", &identity.workflow_digest),
        ("inference_digest", &identity.inference_digest),
    ] {
        if !is_sha256(digest) {
            return Err(SemanticTraceError::InvalidIdentity(format!(
                "{field} must be a sha256 digest"
            )));
        }
    }
    Ok(())
}

fn validate_content_ref(reference: &SemanticContentRef) -> Result<(), SemanticTraceError> {
    if !is_sha256(&reference.digest)
        || reference.local_artifact_path.is_empty()
        || reference.local_artifact_path.starts_with('/')
        || reference.local_artifact_path.contains("..")
        || reference.media_type.is_empty()
    {
        return Err(SemanticTraceError::InvalidContentRef(
            reference.local_artifact_path.clone(),
        ));
    }
    Ok(())
}

fn verify_trace_digest(trace: &SemanticTrace) -> Result<(), SemanticTraceError> {
    let actual = trace.compute_digest()?;
    if actual != trace.trace_digest {
        return Err(SemanticTraceError::DigestMismatch {
            expected: trace.trace_digest.clone(),
            actual,
        });
    }
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_owned(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::String(value) => {
            serde_json::to_string(value).expect("serializing a string cannot fail")
        }
        serde_json::Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        serde_json::Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| format!(
                        "{}:{}",
                        serde_json::to_string(key).expect("serializing an object key cannot fail"),
                        canonical_json(&values[key])
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    }
}

fn sha256(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(71);
    value.push_str("sha256:");
    for byte in Sha256::digest(bytes) {
        write!(&mut value, "{byte:02x}").expect("writing to a String cannot fail");
    }
    value
}

#[derive(Debug)]
pub enum SemanticTraceError {
    Serialize(serde_json::Error),
    Deserialize(serde_json::Error),
    Read(std::io::Error),
    InvalidCanonicalLogLine {
        line: usize,
        source: serde_json::Error,
    },
    UnsupportedSchema(String),
    DigestMismatch {
        expected: String,
        actual: String,
    },
    InvalidEvent(String),
    InvalidIdentity(String),
    InvalidContentRef(String),
}

impl Display for SemanticTraceError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serialize(error) => write!(formatter, "failed to serialize trace: {error}"),
            Self::Deserialize(error) => write!(formatter, "failed to decode trace: {error}"),
            Self::Read(error) => write!(formatter, "failed to read canonical event log: {error}"),
            Self::InvalidCanonicalLogLine { line, source } => {
                write!(
                    formatter,
                    "invalid canonical event log line {line}: {source}"
                )
            }
            Self::UnsupportedSchema(schema) => {
                write!(formatter, "unsupported semantic trace schema `{schema}`")
            }
            Self::DigestMismatch { expected, actual } => {
                write!(
                    formatter,
                    "trace digest mismatch: expected {expected}, found {actual}"
                )
            }
            Self::InvalidEvent(message) => write!(formatter, "invalid trace event: {message}"),
            Self::InvalidIdentity(message) => {
                write!(formatter, "invalid trace identity: {message}")
            }
            Self::InvalidContentRef(path) => {
                write!(formatter, "invalid local content reference `{path}`")
            }
        }
    }
}

impl std::error::Error for SemanticTraceError {}

#[cfg(test)]
mod tests {
    use agl_ids::{EventId, RunId};

    use super::*;
    use crate::{EVENT_SCHEMA, EventScope, SafeRuntimeEvent};

    fn identity() -> SemanticTraceIdentity {
        SemanticTraceIdentity {
            agl_version: "1.0.0-alpha.12".to_owned(),
            extension_catalog_digest: sha256(b"extensions"),
            function_digest: sha256(b"function"),
            tool_set_digest: sha256(b"tools"),
            policy_digest: sha256(b"policy"),
            workflow_digest: sha256(b"workflow"),
            inference_digest: sha256(b"inference"),
        }
    }

    fn event() -> SafeRuntimeEventEnvelope {
        let run_id = RunId::generate();
        SafeRuntimeEventEnvelope {
            schema: EVENT_SCHEMA.to_owned(),
            event_id: EventId::generate(),
            sequence: 1,
            occurred_at_unix_ms: 1,
            scope: EventScope::builder(run_id).build().unwrap(),
            request_id: None,
            caused_by: None,
            payload: SafeRuntimeEvent::ToolEffectLifecycle {
                call_id: "call-1".to_owned(),
                tool_id: "core.workspace:fs.read".to_owned(),
                extension_id: "core.workspace".to_owned(),
                schema_digest: sha256(b"schema"),
                delivery: "replay_safe".to_owned(),
                state: "committed".to_owned(),
                admitted_effects: Vec::new(),
                observed_effects: Vec::new(),
                outcome_code: Some("success".to_owned()),
            },
        }
    }

    #[test]
    fn export_is_deterministic_redacted_and_replays_without_effects() {
        let trace = SemanticTrace::redacted(identity(), vec![event()]).unwrap();
        let first = trace.render().unwrap();
        let second = trace.render().unwrap();
        assert_eq!(first, second);
        assert_eq!(trace.payload_policy, "redacted_metadata");
        assert!(!first.contains("arguments"));
        assert!(!first.contains("workspace content"));
        let report = replay_semantic_trace(&first, &identity()).unwrap();
        assert!(report.matches());
        assert_eq!(report.event_count, 1);
    }

    #[test]
    fn exporter_reads_only_canonical_events_and_rejects_transcript_rows() {
        let source = format!("{}\n", serde_json::to_string(&event()).unwrap());
        let trace = export_semantic_trace(source.as_bytes(), identity(), Vec::new()).unwrap();
        assert_eq!(trace.events.len(), 1);

        let error = export_semantic_trace(
            br#"{"role":"assistant","content":"synthetic training row"}"#.as_slice(),
            identity(),
            Vec::new(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            SemanticTraceError::InvalidCanonicalLogLine { line: 1, .. }
        ));
    }

    #[test]
    fn fixed_agl157_corpus_exports_and_replays_without_external_effects() {
        let identity: SemanticTraceIdentity =
            serde_json::from_str(include_str!("../tests/fixtures/agl157/identity.json")).unwrap();
        let canonical_events = include_str!("../tests/fixtures/agl157/canonical-events.jsonl");
        let trace =
            export_semantic_trace(canonical_events.as_bytes(), identity.clone(), Vec::new())
                .unwrap();
        let encoded = trace.render().unwrap();
        let report = replay_semantic_trace(&encoded, &identity).unwrap();
        assert!(report.matches());
        assert_eq!(report.event_count, 1);
        assert_eq!(trace.payload_policy, "redacted_metadata");
        assert_eq!(
            trace.trace_digest,
            "sha256:b7b0e7e2943f96b2817d2a8c7831a00de0879308620f2b3df4f720db374c0bf4"
        );
    }

    #[test]
    fn replay_detects_tool_extension_workflow_and_inference_drift() {
        let trace = SemanticTrace::redacted(identity(), vec![event()]).unwrap();
        let encoded = trace.render().unwrap();
        let mut expected = identity();
        expected.extension_catalog_digest = sha256(b"changed extensions");
        expected.tool_set_digest = sha256(b"changed tools");
        expected.workflow_digest = sha256(b"changed workflow");
        expected.inference_digest = sha256(b"changed inference");
        let report = replay_semantic_trace(&encoded, &expected).unwrap();
        assert_eq!(
            report
                .drifts
                .iter()
                .map(|drift| drift.field.as_str())
                .collect::<Vec<_>>(),
            vec![
                "extension_catalog_digest",
                "tool_set_digest",
                "workflow_digest",
                "inference_digest"
            ]
        );
    }

    #[test]
    fn content_capture_requires_explicit_local_content_addressed_refs() {
        let reference = SemanticContentRef {
            digest: sha256(b"payload"),
            local_artifact_path: "content/sha256/payload".to_owned(),
            media_type: "application/json".to_owned(),
            bytes: 7,
        };
        let trace =
            SemanticTrace::with_content_refs(identity(), vec![event()], vec![reference]).unwrap();
        assert_eq!(trace.payload_policy, "local_content_addressed_refs");
        assert_eq!(trace.content_refs.len(), 1);
    }
}
