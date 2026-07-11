use std::collections::BTreeSet;
use std::fmt::{self, Display, Formatter};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ActionSchema, CapabilityId, DeclarationDigest, HookEvent, HookId, ProviderId,
    SchemaValidationError, draft202012_schema_for,
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Read,
    Write,
    Execute,
    Approve,
    Admin,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionDelivery {
    ReplaySafe,
    IdempotentRunStep,
    AtMostOnce,
}

impl ActionDelivery {
    fn for_operation(operation_kind: OperationKind) -> Self {
        if operation_kind == OperationKind::Read {
            Self::ReplaySafe
        } else {
            Self::AtMostOnce
        }
    }
}

impl OperationKind {
    pub fn rank(self) -> u8 {
        match self {
            Self::Read => 0,
            Self::Write => 1,
            Self::Execute => 2,
            Self::Approve => 3,
            Self::Admin => 4,
        }
    }

    pub fn permits(self, requested: Self) -> bool {
        self.rank() >= requested.rank()
    }

    pub fn is_state_mutating(self) -> bool {
        self != Self::Read
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Execute => "execute",
            Self::Approve => "approve",
            Self::Admin => "admin",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateEffect {
    RepoFiles,
    RepoWorkspace,
    RepoHooks,
    StoreMemoryEntries,
    StoreMemorySuggestions,
    StoreNotes,
    StoreNoteLinks,
    StoreCron,
    StoreSchema,
    MatrixOutbox,
    StoreIdempotency,
    StorePermissionRequests,
    StorePermissionGrants,
    SkillTrust,
}

impl StateEffect {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RepoFiles => "repo_files",
            Self::RepoWorkspace => "repo_workspace",
            Self::RepoHooks => "repo_hooks",
            Self::StoreMemoryEntries => "store_memory_entries",
            Self::StoreMemorySuggestions => "store_memory_suggestions",
            Self::StoreNotes => "store_notes",
            Self::StoreNoteLinks => "store_note_links",
            Self::StoreCron => "store_cron",
            Self::StoreSchema => "store_schema",
            Self::MatrixOutbox => "matrix_outbox",
            Self::StoreIdempotency => "store_idempotency",
            Self::StorePermissionRequests => "store_permission_requests",
            Self::StorePermissionGrants => "store_permission_grants",
            Self::SkillTrust => "skill_trust",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionVisibility {
    pub visible_in_read_only: bool,
}

impl ActionVisibility {
    pub fn for_operation(operation_kind: OperationKind) -> Self {
        Self {
            visible_in_read_only: operation_kind == OperationKind::Read,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionDeclaration {
    pub id: CapabilityId,
    pub description: String,
    pub input_schema: Value,
    pub operation_kind: OperationKind,
    pub delivery: ActionDelivery,
    pub state_effects: BTreeSet<StateEffect>,
    pub visibility: ActionVisibility,
}

impl ActionDeclaration {
    pub fn new(
        id: CapabilityId,
        description: impl Into<String>,
        input_schema: Value,
        operation_kind: OperationKind,
    ) -> Result<Self, DeclarationError> {
        let declaration = Self {
            id,
            description: description.into(),
            input_schema,
            operation_kind,
            delivery: ActionDelivery::for_operation(operation_kind),
            state_effects: BTreeSet::new(),
            visibility: ActionVisibility::for_operation(operation_kind),
        };
        declaration.validate_shape()?;
        Ok(declaration)
    }

    pub fn from_schema<T: JsonSchema>(
        id: CapabilityId,
        description: impl Into<String>,
        operation_kind: OperationKind,
    ) -> Result<Self, DeclarationError> {
        Self::new(
            id,
            description,
            draft202012_schema_for::<T>(),
            operation_kind,
        )
    }

    pub fn with_state_effects(mut self, effects: impl IntoIterator<Item = StateEffect>) -> Self {
        self.state_effects = effects.into_iter().collect();
        self
    }

    pub fn with_run_step_idempotency(mut self) -> Self {
        self.delivery = ActionDelivery::IdempotentRunStep;
        self
    }

    pub fn with_visibility(mut self, visibility: ActionVisibility) -> Self {
        self.visibility = visibility;
        self
    }

    pub fn validate(&self) -> Result<(), DeclarationError> {
        self.validate_shape()?;
        if self.operation_kind.is_state_mutating() && self.state_effects.is_empty() {
            return Err(DeclarationError::InvalidOperation {
                id: self.id.clone(),
                message: "state-mutating operations must declare state effects",
            });
        }
        if !self.operation_kind.is_state_mutating() && !self.state_effects.is_empty() {
            return Err(DeclarationError::InvalidOperation {
                id: self.id.clone(),
                message: "read operations must not declare state effects",
            });
        }
        match (self.operation_kind.is_state_mutating(), self.delivery) {
            (false, ActionDelivery::ReplaySafe)
            | (true, ActionDelivery::IdempotentRunStep | ActionDelivery::AtMostOnce) => {}
            (false, _) => {
                return Err(DeclarationError::InvalidOperation {
                    id: self.id.clone(),
                    message: "read operations must use replay-safe delivery",
                });
            }
            (true, ActionDelivery::ReplaySafe) => {
                return Err(DeclarationError::InvalidOperation {
                    id: self.id.clone(),
                    message: "state-mutating operations cannot use replay-safe delivery",
                });
            }
        }
        Ok(())
    }

    fn validate_shape(&self) -> Result<(), DeclarationError> {
        if self.description.trim().is_empty() {
            return Err(DeclarationError::BlankField {
                field: "action description",
            });
        }
        let compiled =
            ActionSchema::compile(&self.input_schema).map_err(DeclarationError::InvalidSchema)?;
        validate_input_schema_contract(&self.input_schema, &compiled)?;
        Ok(())
    }

    pub fn compile_schema(&self) -> Result<ActionSchema, SchemaValidationError> {
        ActionSchema::compile(&self.input_schema)
    }

    pub fn digest(&self) -> DeclarationDigest {
        let value = serde_json::to_value(self).expect("action declarations are serializable");
        DeclarationDigest::from_json(&value)
    }
}

const DRAFT_2020_12_SCHEMA: &str = "https://json-schema.org/draft/2020-12/schema";

fn validate_input_schema_contract(
    schema: &Value,
    compiled: &ActionSchema,
) -> Result<(), DeclarationError> {
    if schema.get("$schema").and_then(Value::as_str) != Some(DRAFT_2020_12_SCHEMA) {
        return Err(DeclarationError::IncompleteSchema(
            "action input schema must declare JSON Schema Draft 2020-12",
        ));
    }
    if !root_schema_restricts_to_object(schema, schema, &mut BTreeSet::new()) {
        return Err(DeclarationError::IncompleteSchema(
            "action input schema must restrict the root value to an object",
        ));
    }
    ensure_object_schemas_are_explicitly_closed(schema)?;

    for non_object in [
        Value::Null,
        Value::Bool(false),
        Value::Number(0.into()),
        Value::String(String::new()),
        Value::Array(Vec::new()),
    ] {
        if compiled.validate(&non_object).is_ok() {
            return Err(DeclarationError::IncompleteSchema(
                "action input schema must accept only JSON objects",
            ));
        }
    }
    Ok(())
}

fn root_schema_restricts_to_object(
    schema: &Value,
    root: &Value,
    seen_refs: &mut BTreeSet<String>,
) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    if object.get("type").and_then(Value::as_str) == Some("object") {
        return true;
    }
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        let Some(pointer) = reference.strip_prefix('#') else {
            return false;
        };
        if !seen_refs.insert(reference.to_owned()) {
            return false;
        }
        let result = root
            .pointer(pointer)
            .is_some_and(|target| root_schema_restricts_to_object(target, root, seen_refs));
        seen_refs.remove(reference);
        return result;
    }
    for keyword in ["oneOf", "anyOf"] {
        if let Some(branches) = object.get(keyword).and_then(Value::as_array) {
            return !branches.is_empty()
                && branches
                    .iter()
                    .all(|branch| root_schema_restricts_to_object(branch, root, seen_refs));
        }
    }
    object
        .get("allOf")
        .and_then(Value::as_array)
        .is_some_and(|branches| {
            branches
                .iter()
                .any(|branch| root_schema_restricts_to_object(branch, root, seen_refs))
        })
}

fn ensure_object_schemas_are_explicitly_closed(value: &Value) -> Result<(), DeclarationError> {
    match value {
        Value::Array(values) => {
            for value in values {
                ensure_object_schemas_are_explicitly_closed(value)?;
            }
        }
        Value::Object(object) => {
            let describes_object = object.get("type").and_then(Value::as_str) == Some("object")
                || object.contains_key("properties");
            if describes_object
                && !object.contains_key("additionalProperties")
                && !object.contains_key("unevaluatedProperties")
            {
                return Err(DeclarationError::IncompleteSchema(
                    "every object schema must declare additionalProperties or unevaluatedProperties",
                ));
            }
            for child in object.values() {
                ensure_object_schemas_are_explicitly_closed(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSource {
    Builtin,
    ThirdPartyRegistered,
    TestFixture,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderTrust {
    TrustedByBinary,
    TrustedRegistered,
    Unsupported,
    Unknown,
    Changed,
    Revoked,
}

impl ProviderTrust {
    pub fn permits_execution(self) -> bool {
        matches!(self, Self::TrustedByBinary | Self::TrustedRegistered)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::TrustedByBinary => "trusted_by_binary",
            Self::TrustedRegistered => "trusted_registered",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
            Self::Changed => "changed",
            Self::Revoked => "revoked",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookDeclaration {
    pub id: HookId,
    pub event: HookEvent,
    pub required: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDeclaration {
    pub id: ProviderId,
    pub name: String,
    pub version: String,
    pub source: ProviderSource,
    pub trust: ProviderTrust,
    pub hooks: Vec<HookDeclaration>,
    pub actions: Vec<ActionDeclaration>,
}

impl ProviderDeclaration {
    pub fn builtin(
        id: ProviderId,
        name: impl Into<String>,
        version: impl Into<String>,
    ) -> Result<Self, DeclarationError> {
        Self::new(
            id,
            name,
            version,
            ProviderSource::Builtin,
            ProviderTrust::TrustedByBinary,
        )
    }

    pub fn new(
        id: ProviderId,
        name: impl Into<String>,
        version: impl Into<String>,
        source: ProviderSource,
        trust: ProviderTrust,
    ) -> Result<Self, DeclarationError> {
        let declaration = Self {
            id,
            name: name.into(),
            version: version.into(),
            source,
            trust,
            hooks: Vec::new(),
            actions: Vec::new(),
        };
        declaration.validate()?;
        Ok(declaration)
    }

    pub fn with_hook(mut self, hook: HookDeclaration) -> Self {
        self.hooks.push(hook);
        self
    }

    pub fn with_action(mut self, action: ActionDeclaration) -> Self {
        self.actions.push(action);
        self
    }

    pub fn with_trust(mut self, trust: ProviderTrust) -> Self {
        self.trust = trust;
        self
    }

    pub fn permits_execution(&self) -> bool {
        self.trust.permits_execution()
    }

    pub fn digest(&self) -> DeclarationDigest {
        #[derive(Serialize)]
        struct DigestMaterial<'a> {
            id: &'a ProviderId,
            name: &'a str,
            version: &'a str,
            source: ProviderSource,
            trust: ProviderTrust,
            hooks: std::collections::BTreeMap<&'a HookId, &'a HookDeclaration>,
            actions: std::collections::BTreeMap<&'a CapabilityId, &'a ActionDeclaration>,
        }

        let value = serde_json::to_value(DigestMaterial {
            id: &self.id,
            name: &self.name,
            version: &self.version,
            source: self.source,
            trust: self.trust,
            hooks: self.hooks.iter().map(|hook| (&hook.id, hook)).collect(),
            actions: self
                .actions
                .iter()
                .map(|action| (&action.id, action))
                .collect(),
        })
        .expect("provider declarations are serializable");
        DeclarationDigest::from_json(&value)
    }

    pub fn action(&self, id: &CapabilityId) -> Option<&ActionDeclaration> {
        self.actions.iter().find(|action| &action.id == id)
    }

    pub fn validate(&self) -> Result<(), DeclarationError> {
        if self.name.trim().is_empty() {
            return Err(DeclarationError::BlankField {
                field: "provider name",
            });
        }
        if self.version.trim().is_empty() {
            return Err(DeclarationError::BlankField {
                field: "provider version",
            });
        }
        reject_duplicates(self.hooks.iter().map(|hook| hook.id.as_str()), "hook")?;
        reject_duplicates(
            self.actions.iter().map(|action| action.id.as_str()),
            "action",
        )?;
        for action in &self.actions {
            action.validate()?;
        }
        Ok(())
    }
}

fn reject_duplicates<'a>(
    values: impl IntoIterator<Item = &'a str>,
    kind: &'static str,
) -> Result<(), DeclarationError> {
    let mut seen = BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(DeclarationError::DuplicateId {
                kind,
                id: value.to_owned(),
            });
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeclarationError {
    BlankField {
        field: &'static str,
    },
    DuplicateId {
        kind: &'static str,
        id: String,
    },
    InvalidSchema(SchemaValidationError),
    IncompleteSchema(&'static str),
    InvalidOperation {
        id: CapabilityId,
        message: &'static str,
    },
}

impl Display for DeclarationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::BlankField { field } => write!(formatter, "{field} cannot be blank"),
            Self::DuplicateId { kind, id } => write!(formatter, "duplicate {kind} ID `{id}`"),
            Self::InvalidSchema(error) => Display::fmt(error, formatter),
            Self::IncompleteSchema(message) => {
                write!(formatter, "incomplete action schema: {message}")
            }
            Self::InvalidOperation { id, message } => {
                write!(
                    formatter,
                    "action `{id}` has invalid operation metadata: {message}"
                )
            }
        }
    }
}

impl std::error::Error for DeclarationError {}
