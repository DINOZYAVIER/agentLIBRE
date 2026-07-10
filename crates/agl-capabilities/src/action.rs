use std::error::Error;
use std::fmt::{self, Display, Formatter};

use agl_ids::{ExecutionScope, RequestId};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{CapabilityId, DeclarationDigest, PolicyHash, ProviderId};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionInvocation {
    pub scope: ExecutionScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    pub capability_id: CapabilityId,
    pub provider_id: ProviderId,
    pub declaration_digest: DeclarationDigest,
    pub policy_hash: PolicyHash,
    pub arguments: Value,
}

impl ActionInvocation {
    pub fn new(
        scope: ExecutionScope,
        capability_id: CapabilityId,
        provider_id: ProviderId,
        declaration_digest: DeclarationDigest,
        policy_hash: PolicyHash,
        arguments: Value,
    ) -> Self {
        Self {
            scope,
            request_id: None,
            capability_id,
            provider_id,
            declaration_digest,
            policy_hash,
            arguments,
        }
    }

    pub fn with_request_id(mut self, request_id: RequestId) -> Self {
        self.request_id = Some(request_id);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionResult {
    pub data: Value,
}

impl ActionResult {
    pub fn new(data: Value) -> Self {
        Self { data }
    }

    pub fn render_observation(&self) -> String {
        render_canonical_json(&self.data)
    }
}

pub type ActionHandlerError = Box<dyn Error + Send + Sync + 'static>;

pub trait ActionHandler: Send + Sync {
    fn dispatch(&self, invocation: ActionInvocation) -> Result<ActionResult, ActionHandlerError>;
}

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

impl Display for ActionResult {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.render_observation())
    }
}
