use std::collections::BTreeSet;
use std::fmt::{self, Display, Formatter};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    DeclarationDigest, EffectId, ExtensionId, HookEvent, HookId, SchemaValidationError, ToolId,
    ToolSchema, WorkflowEventId, draft202012_schema_for,
};

pub const EXTENSION_WORKFLOW_SCHEMA: &str = "agentlibre.extension-workflow.v1alpha";

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Read,
    Request,
    Write,
    Execute,
    Approve,
    Admin,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDelivery {
    ReplaySafe,
    IdempotentRunStep,
    AtMostOnce,
}

impl ToolDelivery {
    fn for_operation(operation_kind: OperationKind) -> Self {
        if operation_kind == OperationKind::Read {
            Self::ReplaySafe
        } else {
            Self::AtMostOnce
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReplaySafe => "replay_safe",
            Self::IdempotentRunStep => "idempotent_run_step",
            Self::AtMostOnce => "at_most_once",
        }
    }
}

impl OperationKind {
    pub fn rank(self) -> u8 {
        match self {
            Self::Read => 0,
            Self::Request => 1,
            Self::Write => 2,
            Self::Execute => 3,
            Self::Approve => 4,
            Self::Admin => 5,
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
            Self::Request => "request",
            Self::Write => "write",
            Self::Execute => "execute",
            Self::Approve => "approve",
            Self::Admin => "admin",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectDeclaration {
    pub id: EffectId,
    pub authority_class: String,
}

impl EffectDeclaration {
    pub fn new(id: EffectId, authority_class: impl Into<String>) -> Self {
        Self {
            id,
            authority_class: authority_class.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SensitiveInput {
    ScreenCapture,
}

impl SensitiveInput {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScreenCapture => "screen_capture",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDeclaration {
    pub id: ToolId,
    pub description: String,
    pub input_schema: Value,
    pub output_schema: Value,
    pub outcomes: Vec<ToolOutcomeDeclaration>,
    pub errors: Vec<ToolErrorDeclaration>,
    pub operation_kind: OperationKind,
    pub delivery: ToolDelivery,
    pub state_effects: BTreeSet<EffectId>,
    pub conditional_state_effects: BTreeSet<EffectId>,
    pub sensitive_inputs: BTreeSet<SensitiveInput>,
}

impl ToolDeclaration {
    pub fn new(
        id: ToolId,
        description: impl Into<String>,
        input_schema: Value,
        operation_kind: OperationKind,
    ) -> Result<Self, DeclarationError> {
        let declaration = Self {
            id,
            description: description.into(),
            input_schema,
            output_schema: generic_object_schema(),
            outcomes: vec![ToolOutcomeDeclaration::new("success")],
            errors: vec![ToolErrorDeclaration::terminal("execution_failed")],
            operation_kind,
            delivery: ToolDelivery::for_operation(operation_kind),
            state_effects: BTreeSet::new(),
            conditional_state_effects: BTreeSet::new(),
            sensitive_inputs: BTreeSet::new(),
        };
        declaration.validate_shape()?;
        Ok(declaration)
    }

    pub fn from_schema<T: JsonSchema>(
        id: ToolId,
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

    pub fn with_output_schema<T: JsonSchema>(mut self) -> Result<Self, DeclarationError> {
        self.output_schema = draft202012_schema_for::<T>();
        self.validate_shape()?;
        Ok(self)
    }

    pub fn with_errors(
        mut self,
        errors: impl IntoIterator<Item = ToolErrorDeclaration>,
    ) -> Result<Self, DeclarationError> {
        self.errors = errors.into_iter().collect();
        self.validate_shape()?;
        Ok(self)
    }

    pub fn with_outcomes(
        mut self,
        outcomes: impl IntoIterator<Item = ToolOutcomeDeclaration>,
    ) -> Result<Self, DeclarationError> {
        self.outcomes = outcomes.into_iter().collect();
        self.validate_shape()?;
        Ok(self)
    }

    pub fn with_state_effects(mut self, effects: impl IntoIterator<Item = EffectId>) -> Self {
        self.state_effects = effects.into_iter().collect();
        self
    }

    pub fn with_conditional_state_effects(
        mut self,
        effects: impl IntoIterator<Item = EffectId>,
    ) -> Self {
        self.conditional_state_effects = effects.into_iter().collect();
        self
    }

    pub fn with_sensitive_inputs(
        mut self,
        inputs: impl IntoIterator<Item = SensitiveInput>,
    ) -> Self {
        self.sensitive_inputs = inputs.into_iter().collect();
        self
    }

    pub fn with_run_step_idempotency(mut self) -> Self {
        self.delivery = ToolDelivery::IdempotentRunStep;
        self
    }

    pub fn validate(&self) -> Result<(), DeclarationError> {
        self.validate_shape()?;
        if self
            .state_effects
            .iter()
            .any(|effect| self.conditional_state_effects.contains(effect))
        {
            return Err(DeclarationError::InvalidOperation {
                id: self.id.clone(),
                message: "mandatory and conditional state effects must be disjoint",
            });
        }
        if self.operation_kind.is_state_mutating()
            && self.operation_kind != OperationKind::Request
            && self.state_effects.is_empty()
        {
            return Err(DeclarationError::InvalidOperation {
                id: self.id.clone(),
                message: "state-mutating operations must declare state effects",
            });
        }
        if self.operation_kind == OperationKind::Request
            && self
                .state_effects
                .iter()
                .chain(&self.conditional_state_effects)
                .any(|effect| effect != &EffectId::store_permission_requests())
        {
            return Err(DeclarationError::InvalidOperation {
                id: self.id.clone(),
                message: "request operations may only store pending permission requests",
            });
        }
        if !self.operation_kind.is_state_mutating()
            && self
                .state_effects
                .iter()
                .any(|effect| effect != &EffectId::host_screen_capture())
        {
            return Err(DeclarationError::InvalidOperation {
                id: self.id.clone(),
                message: "read operations may only declare host input effects",
            });
        }
        let screen_effect = self
            .state_effects
            .contains(&EffectId::host_screen_capture());
        let screen_input = self
            .sensitive_inputs
            .contains(&SensitiveInput::ScreenCapture);
        if screen_effect != screen_input {
            return Err(DeclarationError::InvalidOperation {
                id: self.id.clone(),
                message: "screen capture effect and sensitive input must be declared together",
            });
        }
        match (self.operation_kind.is_state_mutating(), self.delivery) {
            (false, ToolDelivery::ReplaySafe)
            | (true, ToolDelivery::IdempotentRunStep | ToolDelivery::AtMostOnce) => {}
            (false, _) => {
                return Err(DeclarationError::InvalidOperation {
                    id: self.id.clone(),
                    message: "read operations must use replay-safe delivery",
                });
            }
            (true, ToolDelivery::ReplaySafe) => {
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
            ToolSchema::compile(&self.input_schema).map_err(DeclarationError::InvalidSchema)?;
        validate_input_schema_shape(&self.input_schema, &compiled)?;
        ToolSchema::compile(&self.output_schema).map_err(DeclarationError::InvalidSchema)?;
        if self.output_schema.get("$schema").and_then(Value::as_str) != Some(DRAFT_2020_12_SCHEMA) {
            return Err(DeclarationError::IncompleteSchema(
                "tool output schema must declare JSON Schema Draft 2020-12",
            ));
        }
        if self.errors.is_empty() {
            return Err(DeclarationError::IncompleteSchema(
                "tool must declare at least one bounded error",
            ));
        }
        if self.outcomes.is_empty() {
            return Err(DeclarationError::IncompleteSchema(
                "tool must declare at least one semantic outcome",
            ));
        }
        reject_duplicates(
            self.outcomes.iter().map(|outcome| outcome.code.as_str()),
            "tool outcome",
        )?;
        for outcome in &self.outcomes {
            outcome.validate()?;
        }
        reject_duplicates(
            self.errors.iter().map(|error| error.code.as_str()),
            "tool error",
        )?;
        for error in &self.errors {
            error.validate()?;
        }
        Ok(())
    }

    pub fn compile_schema(&self) -> Result<ToolSchema, SchemaValidationError> {
        ToolSchema::compile(&self.input_schema)
    }

    pub fn compile_output_schema(&self) -> Result<ToolSchema, SchemaValidationError> {
        ToolSchema::compile(&self.output_schema)
    }

    pub fn declared_error(&self, code: &str) -> Option<&ToolErrorDeclaration> {
        self.errors.iter().find(|error| error.code == code)
    }

    pub fn declared_outcome(&self, code: &str) -> Option<&ToolOutcomeDeclaration> {
        self.outcomes.iter().find(|outcome| outcome.code == code)
    }

    pub fn digest(&self) -> DeclarationDigest {
        let value = serde_json::to_value(self).expect("action declarations are serializable");
        DeclarationDigest::from_json(&value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolOutcomeDeclaration {
    pub code: String,
}

impl ToolOutcomeDeclaration {
    pub fn new(code: impl Into<String>) -> Self {
        Self { code: code.into() }
    }

    fn validate(&self) -> Result<(), DeclarationError> {
        validate_outcome_code(&self.code)
    }
}

fn generic_object_schema() -> Value {
    serde_json::json!({
        "$schema": DRAFT_2020_12_SCHEMA,
        "type": "object",
        "additionalProperties": true
    })
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorClass {
    Recoverable,
    Terminal,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolErrorDeclaration {
    pub code: String,
    pub class: ToolErrorClass,
    pub data_schema: Value,
}

impl ToolErrorDeclaration {
    pub fn recoverable(code: impl Into<String>) -> Self {
        Self::new(code, ToolErrorClass::Recoverable)
    }

    pub fn terminal(code: impl Into<String>) -> Self {
        Self::new(code, ToolErrorClass::Terminal)
    }

    fn new(code: impl Into<String>, class: ToolErrorClass) -> Self {
        Self {
            code: code.into(),
            class,
            data_schema: generic_object_schema(),
        }
    }

    pub fn with_data_schema<T: JsonSchema>(mut self) -> Self {
        self.data_schema = draft202012_schema_for::<T>();
        self
    }

    fn validate(&self) -> Result<(), DeclarationError> {
        validate_outcome_code(&self.code)?;
        ToolSchema::compile(&self.data_schema).map_err(DeclarationError::InvalidSchema)?;
        Ok(())
    }
}

fn validate_outcome_code(code: &str) -> Result<(), DeclarationError> {
    if code.is_empty()
        || !code.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'.')
        })
    {
        return Err(DeclarationError::InvalidOutcomeCode {
            code: code.to_owned(),
        });
    }
    Ok(())
}

const DRAFT_2020_12_SCHEMA: &str = "https://json-schema.org/draft/2020-12/schema";

fn validate_input_schema_shape(
    schema: &Value,
    compiled: &ToolSchema,
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
pub enum ExtensionSource {
    Builtin,
    ThirdPartyRegistered,
    TestFixture,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionTrust {
    TrustedByBinary,
    TrustedRegistered,
    Unsupported,
    Unknown,
    Changed,
    Revoked,
}

impl ExtensionTrust {
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
pub struct ToolWorkflowMapping {
    pub tool_id: ToolId,
    pub outcome_code: String,
    pub event_id: WorkflowEventId,
}

impl ToolWorkflowMapping {
    pub fn new(
        tool_id: ToolId,
        outcome_code: impl Into<String>,
        event_id: WorkflowEventId,
    ) -> Self {
        Self {
            tool_id,
            outcome_code: outcome_code.into(),
            event_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionWorkflowFragment {
    pub schema: String,
    pub mappings: Vec<ToolWorkflowMapping>,
}

impl ExtensionWorkflowFragment {
    pub fn new(mappings: impl IntoIterator<Item = ToolWorkflowMapping>) -> Self {
        Self {
            schema: EXTENSION_WORKFLOW_SCHEMA.to_string(),
            mappings: mappings.into_iter().collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionDescriptor {
    pub id: ExtensionId,
    pub name: String,
    pub version: String,
    pub source: ExtensionSource,
    pub trust: ExtensionTrust,
    pub hooks: Vec<HookDeclaration>,
    pub effects: Vec<EffectDeclaration>,
    pub tools: Vec<ToolDeclaration>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<ExtensionWorkflowFragment>,
}

impl ExtensionDescriptor {
    pub fn builtin(
        id: ExtensionId,
        name: impl Into<String>,
        version: impl Into<String>,
    ) -> Result<Self, DeclarationError> {
        Self::new(
            id,
            name,
            version,
            ExtensionSource::Builtin,
            ExtensionTrust::TrustedByBinary,
        )
    }

    pub fn new(
        id: ExtensionId,
        name: impl Into<String>,
        version: impl Into<String>,
        source: ExtensionSource,
        trust: ExtensionTrust,
    ) -> Result<Self, DeclarationError> {
        let declaration = Self {
            id,
            name: name.into(),
            version: version.into(),
            source,
            trust,
            hooks: Vec::new(),
            effects: Vec::new(),
            tools: Vec::new(),
            workflow: None,
        };
        declaration.validate()?;
        Ok(declaration)
    }

    pub fn with_hook(mut self, hook: HookDeclaration) -> Self {
        self.hooks.push(hook);
        self
    }

    pub fn with_tool(mut self, tool: ToolDeclaration) -> Self {
        for effect_id in tool
            .state_effects
            .iter()
            .chain(&tool.conditional_state_effects)
        {
            if self.effects.iter().any(|effect| &effect.id == effect_id) {
                continue;
            }
            if let Some(authority_class) = standard_authority_class(effect_id) {
                self.effects
                    .push(EffectDeclaration::new(effect_id.clone(), authority_class));
            }
        }
        self.tools.push(tool);
        self
    }

    pub fn with_effect(mut self, effect: EffectDeclaration) -> Self {
        self.effects.push(effect);
        self
    }

    pub fn with_workflow(mut self, workflow: ExtensionWorkflowFragment) -> Self {
        self.workflow = Some(workflow);
        self
    }

    pub fn with_trust(mut self, trust: ExtensionTrust) -> Self {
        self.trust = trust;
        self
    }

    pub fn permits_execution(&self) -> bool {
        self.trust.permits_execution()
    }

    pub fn digest(&self) -> DeclarationDigest {
        #[derive(Serialize)]
        struct DigestMaterial<'a> {
            id: &'a ExtensionId,
            name: &'a str,
            version: &'a str,
            source: ExtensionSource,
            trust: ExtensionTrust,
            hooks: std::collections::BTreeMap<&'a HookId, &'a HookDeclaration>,
            effects: std::collections::BTreeMap<&'a EffectId, &'a EffectDeclaration>,
            tools: std::collections::BTreeMap<&'a ToolId, &'a ToolDeclaration>,
            workflow: &'a Option<ExtensionWorkflowFragment>,
        }

        let value = serde_json::to_value(DigestMaterial {
            id: &self.id,
            name: &self.name,
            version: &self.version,
            source: self.source,
            trust: self.trust,
            hooks: self.hooks.iter().map(|hook| (&hook.id, hook)).collect(),
            effects: self
                .effects
                .iter()
                .map(|effect| (&effect.id, effect))
                .collect(),
            tools: self.tools.iter().map(|tool| (&tool.id, tool)).collect(),
            workflow: &self.workflow,
        })
        .expect("extension descriptors are serializable");
        DeclarationDigest::from_json(&value)
    }

    pub fn tool(&self, id: &ToolId) -> Option<&ToolDeclaration> {
        self.tools.iter().find(|tool| &tool.id == id)
    }

    pub fn validate(&self) -> Result<(), DeclarationError> {
        if self.name.trim().is_empty() {
            return Err(DeclarationError::BlankField {
                field: "extension name",
            });
        }
        if self.version.trim().is_empty() {
            return Err(DeclarationError::BlankField {
                field: "extension version",
            });
        }
        if self.id.as_str() == "core" && self.source != ExtensionSource::Builtin {
            return Err(DeclarationError::ReservedProviderNamespace {
                provider_id: self.id.clone(),
            });
        }
        reject_duplicates(self.hooks.iter().map(|hook| hook.id.as_str()), "hook")?;
        reject_duplicates(
            self.effects.iter().map(|effect| effect.id.as_str()),
            "effect",
        )?;
        reject_duplicates(self.tools.iter().map(|tool| tool.id.as_str()), "tool")?;
        for hook in &self.hooks {
            if hook.id.extension_namespace() != self.id.as_str() {
                return Err(DeclarationError::HookProviderMismatch {
                    hook_id: hook.id.clone(),
                    provider_id: self.id.clone(),
                });
            }
        }
        let declared_effects = self
            .effects
            .iter()
            .map(|effect| &effect.id)
            .collect::<BTreeSet<_>>();
        for effect in &self.effects {
            if effect.authority_class.trim().is_empty() {
                return Err(DeclarationError::BlankField {
                    field: "effect authority class",
                });
            }
        }
        for tool in &self.tools {
            tool.validate()?;
            if let Some(effect_id) = tool
                .state_effects
                .iter()
                .chain(&tool.conditional_state_effects)
                .find(|effect_id| !declared_effects.contains(effect_id))
            {
                return Err(DeclarationError::UndeclaredEffect {
                    tool_id: tool.id.clone(),
                    effect_id: effect_id.clone(),
                });
            }
        }
        if let Some(workflow) = &self.workflow {
            if workflow.schema != EXTENSION_WORKFLOW_SCHEMA {
                return Err(DeclarationError::UnsupportedWorkflowSchema {
                    schema: workflow.schema.clone(),
                });
            }
            let mut mappings = BTreeSet::new();
            for mapping in &workflow.mappings {
                validate_outcome_code(&mapping.outcome_code)?;
                if !mappings.insert((mapping.tool_id.clone(), mapping.outcome_code.clone())) {
                    return Err(DeclarationError::DuplicateWorkflowMapping {
                        tool_id: mapping.tool_id.clone(),
                        outcome_code: mapping.outcome_code.clone(),
                    });
                }
                let Some(tool) = self.tool(&mapping.tool_id) else {
                    return Err(DeclarationError::WorkflowToolUndeclared {
                        tool_id: mapping.tool_id.clone(),
                    });
                };
                if tool.declared_outcome(&mapping.outcome_code).is_none()
                    && tool.declared_error(&mapping.outcome_code).is_none()
                {
                    return Err(DeclarationError::WorkflowOutcomeUndeclared {
                        tool_id: mapping.tool_id.clone(),
                        outcome_code: mapping.outcome_code.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}

fn standard_authority_class(effect_id: &EffectId) -> Option<&'static str> {
    match effect_id.as_str() {
        "agl:host.screen_capture" => Some("host_observation"),
        "agl:agent.spawn" => Some("agent_delegation"),
        "agl:session.working_directory" => Some("session_mutation"),
        "agl:process.spawn" => Some("process_spawn"),
        "agl:process.control" => Some("process_control"),
        "agl:process.host_execution" => Some("host_execution"),
        "agl:process.shell_login_startup" => Some("shell_startup"),
        "agl:repo.files" | "agl:repo.workspace" => Some("repository_mutation"),
        "agl:repo.hooks" => Some("repository_hooks"),
        "agl:store.memory_entries"
        | "agl:store.memory_suggestions"
        | "agl:store.notes"
        | "agl:store.note_links"
        | "agl:store.cron"
        | "agl:store.schema"
        | "agl:store.idempotency" => Some("durable_store_mutation"),
        "agl:matrix.outbox" => Some("external_delivery"),
        "agl:store.permission_requests" | "agl:store.permission_grants" => {
            Some("permission_mutation")
        }
        "agl:skill.trust" => Some("trust_mutation"),
        _ => None,
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
        id: ToolId,
        message: &'static str,
    },
    InvalidOutcomeCode {
        code: String,
    },
    UnsupportedWorkflowSchema {
        schema: String,
    },
    DuplicateWorkflowMapping {
        tool_id: ToolId,
        outcome_code: String,
    },
    WorkflowToolUndeclared {
        tool_id: ToolId,
    },
    WorkflowOutcomeUndeclared {
        tool_id: ToolId,
        outcome_code: String,
    },
    HookProviderMismatch {
        hook_id: HookId,
        provider_id: ExtensionId,
    },
    UndeclaredEffect {
        tool_id: ToolId,
        effect_id: EffectId,
    },
    ReservedProviderNamespace {
        provider_id: ExtensionId,
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
            Self::InvalidOutcomeCode { code } => {
                write!(formatter, "invalid bounded tool outcome code `{code}`")
            }
            Self::UnsupportedWorkflowSchema { schema } => {
                write!(
                    formatter,
                    "unsupported Extension workflow schema `{schema}`"
                )
            }
            Self::DuplicateWorkflowMapping {
                tool_id,
                outcome_code,
            } => write!(
                formatter,
                "duplicate workflow mapping for Tool `{tool_id}` outcome `{outcome_code}`"
            ),
            Self::WorkflowToolUndeclared { tool_id } => {
                write!(
                    formatter,
                    "workflow mapping references undeclared Tool `{tool_id}`"
                )
            }
            Self::WorkflowOutcomeUndeclared {
                tool_id,
                outcome_code,
            } => write!(
                formatter,
                "workflow mapping references undeclared outcome `{outcome_code}` for Tool `{tool_id}`"
            ),
            Self::HookProviderMismatch {
                hook_id,
                provider_id,
            } => write!(
                formatter,
                "hook `{hook_id}` namespace must match provider `{provider_id}`"
            ),
            Self::UndeclaredEffect { tool_id, effect_id } => write!(
                formatter,
                "tool `{tool_id}` references undeclared effect `{effect_id}`"
            ),
            Self::ReservedProviderNamespace { provider_id } => write!(
                formatter,
                "provider namespace `{provider_id}` is reserved for builtin AgentLIBRE hooks"
            ),
        }
    }
}

impl std::error::Error for DeclarationError {}
