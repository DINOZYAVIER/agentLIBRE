use agl_extension::{
    DeclarationDigest, ExtensionId, ToolErrorDeclaration, ToolHandlerError, ToolId, ToolResult,
    WorkflowEventId, render_canonical_json,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const TOOL_OUTCOME_SCHEMA: &str = "agentlibre.tool-outcome.v1alpha";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcomeStatus {
    Succeeded,
    RecoverableError,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolOutcomeError {
    pub code: String,
    pub message: String,
    pub data: Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolOutcome {
    pub schema: String,
    pub call_id: String,
    pub tool_id: ToolId,
    pub extension_id: ExtensionId,
    pub schema_digest: DeclarationDigest,
    pub status: ToolOutcomeStatus,
    pub outcome_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow_event: Option<WorkflowEventId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolOutcomeError>,
    pub admitted_effect_receipt_refs: Vec<String>,
    pub observed_effect_receipt_refs: Vec<String>,
    pub observation: String,
}

impl ToolOutcome {
    pub fn succeeded(
        call_id: String,
        tool_id: ToolId,
        extension_id: ExtensionId,
        schema_digest: DeclarationDigest,
        result: ToolResult,
    ) -> Self {
        let observation = render_canonical_json(&json!({
            "status": "succeeded",
            "outcome_code": result.outcome_code,
            "data": result.data,
        }));
        Self {
            schema: TOOL_OUTCOME_SCHEMA.to_string(),
            call_id,
            tool_id,
            extension_id,
            schema_digest,
            status: ToolOutcomeStatus::Succeeded,
            outcome_code: result.outcome_code,
            workflow_event: None,
            data: Some(result.data),
            error: None,
            admitted_effect_receipt_refs: Vec::new(),
            observed_effect_receipt_refs: Vec::new(),
            observation,
        }
    }

    pub(crate) fn recoverable(
        call_id: String,
        tool_id: ToolId,
        extension_id: ExtensionId,
        schema_digest: DeclarationDigest,
        error_declaration: &ToolErrorDeclaration,
        error: ToolHandlerError,
    ) -> Self {
        let observation = render_canonical_json(&json!({
            "status": "recoverable_error",
            "outcome_code": error.code,
            "error": {
                "code": error.code,
                "message": error.message,
                "data": error.data,
            }
        }));
        Self {
            schema: TOOL_OUTCOME_SCHEMA.to_string(),
            call_id,
            tool_id,
            extension_id,
            schema_digest,
            status: ToolOutcomeStatus::RecoverableError,
            outcome_code: error_declaration.code.clone(),
            workflow_event: None,
            data: None,
            error: Some(ToolOutcomeError {
                code: error.code,
                message: error.message,
                data: error.data,
            }),
            admitted_effect_receipt_refs: Vec::new(),
            observed_effect_receipt_refs: Vec::new(),
            observation,
        }
    }

    pub fn render_observation(&self) -> &str {
        &self.observation
    }

    pub(crate) fn with_effect_receipts(
        mut self,
        admitted: impl IntoIterator<Item = String>,
        observed: impl IntoIterator<Item = String>,
    ) -> Self {
        self.admitted_effect_receipt_refs = admitted.into_iter().collect();
        self.observed_effect_receipt_refs = observed.into_iter().collect();
        self
    }

    pub(crate) fn with_workflow_event(mut self, event: Option<WorkflowEventId>) -> Self {
        self.workflow_event = event;
        self
    }

    pub fn observation_result(&self) -> ToolResult {
        ToolResult::new(serde_json::to_value(self).expect("ToolOutcome is always serializable"))
            .with_outcome_code(self.outcome_code.clone())
    }
}
