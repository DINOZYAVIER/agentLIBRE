use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agl_content::Content;
use agl_ids::{ExecutionScope, RequestId};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{DeclarationDigest, EffectId, ExtensionId, PolicyHash, ToolId};

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolGrantProvenance {
    pub grant_id: String,
    pub duration: String,
    pub admitted_scope: String,
    pub scope_digest: String,
}

impl ToolGrantProvenance {
    pub fn new(
        grant_id: impl Into<String>,
        duration: impl Into<String>,
        admitted_scope: impl Into<String>,
        scope_digest: impl Into<String>,
    ) -> Self {
        Self {
            grant_id: grant_id.into(),
            duration: duration.into(),
            admitted_scope: admitted_scope.into(),
            scope_digest: scope_digest.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolInvocation {
    pub scope: ExecutionScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub tool_id: ToolId,
    pub extension_id: ExtensionId,
    pub declaration_digest: DeclarationDigest,
    pub policy_hash: PolicyHash,
    pub arguments: Value,
}

impl ToolInvocation {
    pub fn new(
        scope: ExecutionScope,
        tool_id: ToolId,
        extension_id: ExtensionId,
        declaration_digest: DeclarationDigest,
        policy_hash: PolicyHash,
        arguments: Value,
    ) -> Self {
        Self {
            scope,
            request_id: None,
            tool_id,
            extension_id,
            declaration_digest,
            policy_hash,
            arguments,
        }
    }

    pub fn with_request_id(mut self, request_id: RequestId) -> Self {
        self.request_id = Some(request_id);
        self
    }

    pub fn run_step_idempotency_key(&self) -> Option<String> {
        self.scope
            .step_id()
            .map(|step_id| format!("{}:{step_id}", self.scope.run_id()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    pub outcome_code: String,
    pub data: Value,
    pub observed_effects: Vec<ObservedEffect>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<Content>,
}

impl ToolResult {
    pub fn new(data: Value) -> Self {
        Self {
            outcome_code: "success".to_string(),
            data,
            observed_effects: Vec::new(),
            content: None,
        }
    }

    pub fn with_content(mut self, content: Content) -> Self {
        self.content = Some(content);
        self
    }

    pub fn with_outcome_code(mut self, outcome_code: impl Into<String>) -> Self {
        self.outcome_code = outcome_code.into();
        self
    }

    pub fn with_observed_effects(
        mut self,
        effects: impl IntoIterator<Item = ObservedEffect>,
    ) -> Self {
        self.observed_effects = effects.into_iter().collect();
        self
    }

    pub fn render_observation(&self) -> String {
        render_canonical_json(&self.data)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedEffect {
    pub effect_id: EffectId,
    pub scope: BTreeMap<String, String>,
}

impl ObservedEffect {
    pub fn new(effect_id: EffectId, scope: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            effect_id,
            scope: scope.into_iter().collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolHandlerError {
    pub code: String,
    pub message: String,
    pub data: Value,
}

impl ToolHandlerError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, data: Value) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            data,
        }
    }

    pub fn execution_failed(message: impl Into<String>) -> Self {
        Self::new("execution_failed", message, Value::Object(Map::new()))
    }
}

impl From<anyhow::Error> for ToolHandlerError {
    fn from(error: anyhow::Error) -> Self {
        Self::execution_failed(format!("{error:#}"))
    }
}

impl Display for ToolHandlerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ToolHandlerError {}

pub trait CancellationSignal: Send + Sync {
    fn is_cancelled(&self) -> bool;
}

#[derive(Debug)]
struct NeverCancelled;

impl CancellationSignal for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

#[derive(Clone)]
pub struct ToolDispatchControl {
    cancellation: Arc<dyn CancellationSignal>,
    deadline: Option<Instant>,
}

impl ToolDispatchControl {
    pub fn new(cancellation: Arc<dyn CancellationSignal>, deadline: Option<Instant>) -> Self {
        Self {
            cancellation,
            deadline,
        }
    }

    pub fn uncancellable() -> Self {
        Self::new(Arc::new(NeverCancelled), None)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    pub fn is_expired(&self) -> bool {
        self.deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    }
}

impl fmt::Debug for ToolDispatchControl {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolDispatchControl")
            .field("cancelled", &self.is_cancelled())
            .field("deadline", &self.deadline)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct ToolDispatchContext {
    invocation: ToolInvocation,
    control: ToolDispatchControl,
    authorized_conditional_effects: std::collections::BTreeSet<EffectId>,
    grant_provenance: Option<ToolGrantProvenance>,
    effect_correlation: Option<crate::ToolEffectCorrelation>,
}

impl ToolDispatchContext {
    pub fn new(
        invocation: ToolInvocation,
        control: ToolDispatchControl,
        authorized_conditional_effects: impl IntoIterator<Item = EffectId>,
        grant_provenance: Option<ToolGrantProvenance>,
        effect_correlation: Option<crate::ToolEffectCorrelation>,
    ) -> Self {
        Self {
            invocation,
            control,
            authorized_conditional_effects: authorized_conditional_effects.into_iter().collect(),
            grant_provenance,
            effect_correlation,
        }
    }

    pub fn invocation(&self) -> &ToolInvocation {
        &self.invocation
    }

    pub fn control(&self) -> &ToolDispatchControl {
        &self.control
    }

    pub fn authorized_conditional_effects(&self) -> &std::collections::BTreeSet<EffectId> {
        &self.authorized_conditional_effects
    }

    pub fn grant_provenance(&self) -> Option<&ToolGrantProvenance> {
        self.grant_provenance.as_ref()
    }

    pub fn effect_correlation(&self) -> Option<&crate::ToolEffectCorrelation> {
        self.effect_correlation.as_ref()
    }

    pub fn into_invocation(self) -> ToolInvocation {
        self.invocation
    }
}

pub trait ToolHandler: Send + Sync {
    fn preflight(
        &self,
        _invocation: &ToolInvocation,
    ) -> Result<std::collections::BTreeSet<EffectId>, ToolHandlerError> {
        Ok(std::collections::BTreeSet::new())
    }

    fn dispatch(&self, context: ToolDispatchContext) -> ToolHandlerFuture<'_>;
}

pub type ToolHandlerFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolResult, ToolHandlerError>> + Send + 'a>>;

pub fn render_canonical_json(value: &Value) -> String {
    serde_json::to_string(&canonicalize(value)).expect("serializing a JSON value cannot fail")
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonicalize).collect()),
        Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            let sorted = keys
                .into_iter()
                .map(|key| (key.clone(), canonicalize(&values[key])))
                .collect::<Map<_, _>>();
            Value::Object(sorted)
        }
        _ => value.clone(),
    }
}

impl Display for ToolResult {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.render_observation())
    }
}
