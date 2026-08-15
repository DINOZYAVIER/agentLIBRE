use std::collections::BTreeSet;
use std::sync::Arc;

use agl_exec::AuthorityFingerprint;
use agl_kernel::{
    EffectDeclaration, EffectId, ExtensionDescriptor, ExtensionId, OperationKind, SensitiveInput,
    ToolDeclaration, ToolDispatchContext, ToolHandler, ToolId, ToolInvocation, ToolResult,
};
use agl_permission::{
    PermissionDuration, PermissionGrantDraft, PermissionOperationId, PermissionRepository,
    PermissionRequestDraft, PermissionRequestRecord, PermissionRequestState,
};
use agl_process::TerminalEndpoint;
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::parse_tool_args as parse_args;

pub const EXTENSION_ID: &str = "core.permission";
pub const PERMISSIONS_STATUS_TOOL_ID: &str = "core.permission:status";
pub const PERMISSIONS_REQUEST_TOOL_ID: &str = "core.permission:request";
pub const PERMISSIONS_GRANT_TOOL_ID: &str = "core.permission:grant";
pub const PERMISSIONS_REVOKE_TOOL_ID: &str = "core.permission:revoke";

#[derive(Clone)]
pub struct PermissionTools {
    repository: Arc<dyn PermissionRepository>,
    runtime_status: PermissionRuntimeStatus,
    terminal_endpoint: Option<Arc<TerminalEndpoint>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionRuntimeStatus {
    pub current_mode: String,
    pub visible_tools: Vec<String>,
    pub dynamic_grants: bool,
    pub granted_visible_tools: Vec<String>,
    pub ignored_grants: Vec<String>,
}

impl Default for PermissionRuntimeStatus {
    fn default() -> Self {
        Self {
            current_mode: "unknown".to_string(),
            visible_tools: Vec::new(),
            dynamic_grants: false,
            granted_visible_tools: Vec::new(),
            ignored_grants: Vec::new(),
        }
    }
}

impl PermissionTools {
    pub fn new(repository: Arc<dyn PermissionRepository>) -> Self {
        Self {
            repository,
            runtime_status: PermissionRuntimeStatus::default(),
            terminal_endpoint: None,
        }
    }

    pub fn with_runtime_status(mut self, runtime_status: PermissionRuntimeStatus) -> Self {
        self.runtime_status = runtime_status;
        self
    }

    pub fn with_terminal_endpoint(mut self, terminal_endpoint: Arc<TerminalEndpoint>) -> Self {
        self.terminal_endpoint = Some(terminal_endpoint);
        self
    }

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<Value> {
        match name {
            PERMISSIONS_STATUS_TOOL_ID => self.status(arguments),
            PERMISSIONS_REQUEST_TOOL_ID => self.request(arguments),
            PERMISSIONS_GRANT_TOOL_ID | PERMISSIONS_REVOKE_TOOL_ID => bail!(
                "{name} requires a request or run-step idempotency identity and must be dispatched through ToolHandler"
            ),
            _ => anyhow::bail!("unknown permission tool `{name}`"),
        }
    }

    fn status(&self, arguments: Value) -> Result<Value> {
        parse_args::<StatusArgs>(PERMISSIONS_STATUS_TOOL_ID, arguments)?;
        let pending = self
            .repository
            .requests_by_state(PermissionRequestState::Pending)?;
        let active = self.repository.active_grants()?;
        let pending_requests = pending
            .into_iter()
            .map(|request| {
                json!({
                    "request_id": request.id,
                    "tools": request.requested_tools,
                    "max_operation_kind": request.max_operation_kind,
                    "state_effects": request.state_effects,
                    "sensitive_inputs": request.sensitive_inputs,
                    "duration": request.duration,
                    "status": request.state.as_str(),
                })
            })
            .collect::<Vec<_>>();
        let active_grants = active
            .into_iter()
            .map(|grant| {
                json!({
                    "grant_id": grant.id,
                    "tool_id": grant.tool_id,
                    "max_operation_kind": grant.max_operation_kind,
                    "state_effects": grant.state_effects,
                    "sensitive_inputs": grant.sensitive_inputs,
                    "duration": grant.duration,
                    "status": grant.state.as_str(),
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "tool": PERMISSIONS_STATUS_TOOL_ID,
            "status": "ok",
            "current_mode": self.runtime_status.current_mode,
            "visible_tools": self.runtime_status.visible_tools,
            "dynamic_grants": self.runtime_status.dynamic_grants,
            "granted_visible_tools": self.runtime_status.granted_visible_tools,
            "ignored_grants": self.runtime_status.ignored_grants,
            "pending_request_count": pending_requests.len(),
            "active_grant_count": active_grants.len(),
            "default_duration": "one_turn",
            "supported_durations": ["one_turn", "session"],
            "elevated_effect_warnings": {
                "host_process_execution": "unrestricted daemon-user filesystem and network access",
                "shell_login_startup": "execution of user-controlled shell startup files"
            },
            "pending_requests": pending_requests,
            "active_grants": active_grants,
        }))
    }

    fn request(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<RequestArgs>(PERMISSIONS_REQUEST_TOOL_ID, arguments)?;
        let requested_tools = validate_requested_tools(args.tools)?;
        let max_operation_kind = args
            .max_operation_kind
            .unwrap_or(OperationKindArg::Write)
            .into();
        let duration = args.duration.unwrap_or_default().into();
        let requester_ref = args
            .requester_ref
            .unwrap_or_else(|| "tool:core.permission:request".to_string());
        let request = self.repository.create_request(PermissionRequestDraft {
            requested_tools,
            max_operation_kind,
            state_effects: parse_effects(args.state_effects)?,
            sensitive_inputs: parse_sensitive_inputs(args.sensitive_inputs)?,
            scope: args.scope.unwrap_or_else(|| serde_json::json!({})),
            duration,
            reason: args.reason,
            requester_ref,
        })?;
        Ok(render_permission_request_result(&request))
    }

    fn grant(&self, arguments: Value, operation_id: PermissionOperationId) -> Result<Value> {
        let args = parse_args::<GrantArgs>(PERMISSIONS_GRANT_TOOL_ID, arguments)?;
        let grants = match args {
            GrantArgs::Request(args) => self.repository.grant_request(
                &args.request_id,
                args.granted_by_ref
                    .as_deref()
                    .unwrap_or("tool:core.permission:grant"),
                operation_id,
                args.resolution_ref.as_deref(),
            )?,
            GrantArgs::Direct(args) => {
                let mut tools = validate_requested_tools(vec![args.tool_id])?;
                let tool_id = tools.pop().expect("one validated tool");
                let max_operation_kind = args
                    .max_operation_kind
                    .unwrap_or(OperationKindArg::Write)
                    .into();
                vec![
                    self.repository.create_grant(PermissionGrantDraft {
                        request_id: None,
                        tool_id,
                        max_operation_kind,
                        state_effects: parse_effects(args.state_effects)?,
                        sensitive_inputs: parse_sensitive_inputs(args.sensitive_inputs)?,
                        scope: args.scope.unwrap_or_else(|| serde_json::json!({})),
                        duration: args.duration.unwrap_or_default().into(),
                        granted_by_ref: args
                            .granted_by_ref
                            .unwrap_or_else(|| "tool:core.permission:grant".to_string()),
                    })?,
                ]
            }
        };
        let grants = grants
            .into_iter()
            .map(|grant| {
                json!({
                    "grant_id": grant.id,
                    "tool_id": grant.tool_id,
                    "max_operation_kind": grant.max_operation_kind,
                    "sensitive_inputs": grant.sensitive_inputs,
                    "duration": grant.duration,
                    "status": grant.state.as_str(),
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({
            "tool": PERMISSIONS_GRANT_TOOL_ID,
            "status": "granted",
            "grant_count": grants.len(),
            "grants": grants,
        }))
    }

    async fn revoke_live(
        &self,
        arguments: Value,
        policy_hash: &agl_kernel::PolicyHash,
        operation_id: PermissionOperationId,
    ) -> Result<Value> {
        let args = parse_args::<RevokeArgs>(PERMISSIONS_REVOKE_TOOL_ID, arguments)?;
        let terminated_executions = if let Some(endpoint) = &self.terminal_endpoint {
            let authority = AuthorityFingerprint::new(policy_hash.as_str())
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            endpoint
                .connect(authority)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?
                .revoke_grant(args.grant_id.clone(), CancellationToken::new())
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string()))?
        } else {
            0
        };
        self.commit_revocation(args, operation_id, terminated_executions)
    }

    fn commit_revocation(
        &self,
        args: RevokeArgs,
        operation_id: PermissionOperationId,
        terminated_executions: u32,
    ) -> Result<Value> {
        let grant = self.repository.revoke_grant(
            &args.grant_id,
            operation_id,
            args.revoke_ref.as_deref(),
        )?;
        Ok(json!({
            "tool": PERMISSIONS_REVOKE_TOOL_ID,
            "grant_id": grant.id,
            "tool_id": grant.tool_id,
            "status": grant.state.as_str(),
            "terminated_executions": terminated_executions,
        }))
    }
}

impl ToolHandler for PermissionTools {
    fn dispatch(&self, context: ToolDispatchContext) -> agl_kernel::ToolHandlerFuture<'_> {
        Box::pin(async move {
            let invocation = context.into_invocation();
            let data = match invocation.tool_id.as_str() {
                PERMISSIONS_GRANT_TOOL_ID => {
                    let operation_id = permission_operation_id(&invocation)?;
                    self.grant(invocation.arguments, operation_id)?
                }
                PERMISSIONS_REVOKE_TOOL_ID => {
                    let operation_id = permission_operation_id(&invocation)?;
                    self.revoke_live(invocation.arguments, &invocation.policy_hash, operation_id)
                        .await?
                }
                _ => self.dispatch(invocation.tool_id.as_str(), invocation.arguments)?,
            };
            Ok(ToolResult::new(data))
        })
    }
}

pub fn declaration() -> ExtensionDescriptor {
    ExtensionDescriptor::builtin(
        ExtensionId::new(EXTENSION_ID).expect("builtin permission extension id is valid"),
        "Permission Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin permission extension declaration is valid")
    .with_tool(action::<StatusArgs>(
        PERMISSIONS_STATUS_TOOL_ID,
        "Show pending permission requests and active grants.",
        OperationKind::Read,
        &[],
    ))
    .with_tool(action::<RequestArgs>(
        PERMISSIONS_REQUEST_TOOL_ID,
        "Create a pending permission request for exact tool IDs; this does not grant access.",
        OperationKind::Request,
        &[EffectId::store_permission_requests()],
    ))
    .with_tool(action::<GrantArgs>(
        PERMISSIONS_GRANT_TOOL_ID,
        "Grant an existing permission request or an exact tool ID.",
        OperationKind::Approve,
        &[
            EffectId::store_permission_grants(),
            EffectId::store_permission_requests(),
        ],
    ))
    .with_tool(action::<RevokeArgs>(
        PERMISSIONS_REVOKE_TOOL_ID,
        "Revoke an active permission grant.",
        OperationKind::Approve,
        &[EffectId::store_permission_grants()],
    ))
    .with_effects([
        EffectDeclaration::for_standard(EffectId::store_permission_requests()).unwrap(),
        EffectDeclaration::for_standard(EffectId::store_permission_grants()).unwrap(),
    ])
}

fn action<T: JsonSchema>(
    id: &str,
    description: &str,
    operation_kind: OperationKind,
    state_effects: &[EffectId],
) -> ToolDeclaration {
    ToolDeclaration::from_schema::<T>(
        ToolId::new(id).expect("builtin permission tool id is valid"),
        description,
        operation_kind,
    )
    .expect("builtin permission tool declaration schema is valid")
    .with_state_effects(state_effects.iter().cloned())
}

fn permission_operation_id(invocation: &ToolInvocation) -> Result<PermissionOperationId> {
    let identity = invocation
        .run_step_idempotency_key()
        .or_else(|| invocation.request_id.as_ref().map(ToString::to_string))
        .with_context(|| {
            format!(
                "{} requires a request or run-step idempotency identity",
                invocation.tool_id
            )
        })?;
    PermissionOperationId::new(format!("{}:{identity}", invocation.tool_id))
        .map_err(anyhow::Error::from)
}

fn validate_requested_tools(tools: Vec<String>) -> Result<Vec<ToolId>> {
    if tools.is_empty() {
        bail!("core.permission:request tools cannot be empty");
    }
    let mut normalized = Vec::with_capacity(tools.len());
    let mut seen = std::collections::BTreeSet::new();
    for tool in tools {
        let id = ToolId::new(tool.clone()).with_context(|| {
            format!("core.permission:request requested tool id is invalid: {tool}")
        })?;
        if id.extension_namespace() == "core.permission" {
            bail!("permission tools cannot request or grant permission tools");
        }
        if seen.insert(id.clone()) {
            normalized.push(id);
        }
    }
    Ok(normalized)
}

fn parse_effects(effects: Option<Vec<String>>) -> Result<BTreeSet<EffectId>> {
    effects
        .unwrap_or_default()
        .into_iter()
        .map(|effect| EffectId::new(effect).map_err(anyhow::Error::from))
        .collect()
}

fn parse_sensitive_inputs(inputs: Option<Vec<String>>) -> Result<BTreeSet<SensitiveInput>> {
    inputs
        .unwrap_or_default()
        .into_iter()
        .map(|input| match input.as_str() {
            "screen_capture" => Ok(SensitiveInput::ScreenCapture),
            _ => bail!("unknown sensitive input `{input}`"),
        })
        .collect()
}

fn render_permission_request_result(request: &PermissionRequestRecord) -> Value {
    json!({
        "tool": PERMISSIONS_REQUEST_TOOL_ID,
        "request_id": request.id,
        "status": request.state.as_str(),
        "tools": request.requested_tools,
        "max_operation_kind": request.max_operation_kind,
        "sensitive_inputs": request.sensitive_inputs,
        "duration": request.duration,
        "result": "pending_approval",
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum OperationKindArg {
    Read,
    Request,
    Write,
    Execute,
    Approve,
    Admin,
}

#[derive(Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum PermissionDurationArg {
    #[default]
    OneTurn,
    Session,
}

impl PermissionDurationArg {}

impl From<PermissionDurationArg> for PermissionDuration {
    fn from(value: PermissionDurationArg) -> Self {
        match value {
            PermissionDurationArg::OneTurn => Self::OneTurn,
            PermissionDurationArg::Session => Self::Session,
        }
    }
}

impl From<OperationKindArg> for OperationKind {
    fn from(value: OperationKindArg) -> Self {
        match value {
            OperationKindArg::Read => Self::Read,
            OperationKindArg::Request => Self::Request,
            OperationKindArg::Write => Self::Write,
            OperationKindArg::Execute => Self::Execute,
            OperationKindArg::Approve => Self::Approve,
            OperationKindArg::Admin => Self::Admin,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StatusArgs {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RequestArgs {
    tools: Vec<String>,
    reason: String,
    max_operation_kind: Option<OperationKindArg>,
    state_effects: Option<Vec<String>>,
    sensitive_inputs: Option<Vec<String>>,
    scope: Option<Value>,
    duration: Option<PermissionDurationArg>,
    requester_ref: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(untagged)]
enum GrantArgs {
    Request(GrantRequestArgs),
    Direct(GrantDirectArgs),
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GrantRequestArgs {
    request_id: String,
    granted_by_ref: Option<String>,
    resolution_ref: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GrantDirectArgs {
    tool_id: String,
    max_operation_kind: Option<OperationKindArg>,
    state_effects: Option<Vec<String>>,
    sensitive_inputs: Option<Vec<String>>,
    scope: Option<Value>,
    duration: Option<PermissionDurationArg>,
    granted_by_ref: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RevokeArgs {
    grant_id: String,
    revoke_ref: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::test_support::migrated_temp_store;

    use super::*;

    #[test]
    fn permission_request_creates_pending_one_turn_request() {
        let (_root, store) = migrated_temp_store("permission-request");
        let tools = PermissionTools::new(store);

        let output = tools
            .dispatch(
                PERMISSIONS_REQUEST_TOOL_ID,
                json!({
                    "tools": ["core.note:add"],
                    "reason": "Create one explicit note.",
                    "requester_ref": "chat:turn-1"
                }),
            )
            .unwrap();

        assert_eq!(output["tool"], PERMISSIONS_REQUEST_TOOL_ID);
        assert_eq!(output["result"], "pending_approval");
        assert_eq!(output["duration"], "one_turn");
        assert_eq!(output["tools"], json!(["core.note:add"]));

        let status = tools
            .dispatch(PERMISSIONS_STATUS_TOOL_ID, json!({}))
            .unwrap();
        assert_eq!(status["current_mode"], "unknown");
        assert_eq!(status["dynamic_grants"], false);
        assert_eq!(status["pending_request_count"], 1);
        assert_eq!(status["active_grant_count"], 0);
        assert_eq!(
            status["pending_requests"][0]["tools"],
            json!(["core.note:add"])
        );
    }

    #[test]
    fn permission_request_rejects_permission_tools() {
        let (_root, store) = migrated_temp_store("permission-reject");
        let tools = PermissionTools::new(store);

        let err = tools
            .dispatch(
                PERMISSIONS_REQUEST_TOOL_ID,
                json!({
                    "tools": ["core.permission:grant"],
                    "reason": "grant myself"
                }),
            )
            .unwrap_err();

        assert!(err.to_string().contains("permission tools cannot request"));
    }

    #[test]
    fn permission_status_reports_runtime_snapshot() {
        let (_root, store) = migrated_temp_store("permission-status");
        let tools = PermissionTools::new(store).with_runtime_status(PermissionRuntimeStatus {
            current_mode: "read-only".to_string(),
            visible_tools: vec![
                "core.workspace:fs.read".to_string(),
                "core.permission:status".to_string(),
                "core.permission:request".to_string(),
            ],
            dynamic_grants: false,
            granted_visible_tools: Vec::new(),
            ignored_grants: Vec::new(),
        });

        let status = tools
            .dispatch(PERMISSIONS_STATUS_TOOL_ID, json!({}))
            .unwrap();

        assert_eq!(status["current_mode"], "read-only");
        assert_eq!(
            status["visible_tools"],
            json!([
                "core.workspace:fs.read",
                "core.permission:status",
                "core.permission:request"
            ])
        );
        assert_eq!(status["dynamic_grants"], false);
        assert_eq!(status["granted_visible_tools"], json!([]));
        assert_eq!(status["ignored_grants"], json!([]));
    }

    #[test]
    fn permission_request_schema_is_complete_and_closed() {
        let declaration = declaration();
        declaration.validate().unwrap();
        let request = declaration
            .tool(&ToolId::new(PERMISSIONS_REQUEST_TOOL_ID).unwrap())
            .unwrap();
        assert_eq!(request.input_schema["additionalProperties"], false);
        assert_eq!(request.operation_kind, OperationKind::Request);
        assert_eq!(
            request.state_effects,
            [EffectId::store_permission_requests()]
                .into_iter()
                .collect()
        );
        let schema = request.compile_schema().unwrap();
        assert!(
            schema
                .validate(&json!({
                    "tools": ["core.note:add"],
                    "reason": "Create one explicit note."
                }))
                .is_ok()
        );
        assert!(
            schema
                .validate(&json!({"tools": ["core.note:add"]}))
                .is_err()
        );
        assert!(
            schema
                .validate(&json!({
                    "tools": ["core.note:add"],
                    "reason": "Create one explicit note.",
                    "extra": true
                }))
                .is_err()
        );
        assert!(
            schema
                .validate(&json!({
                    "tools": "core.note:add",
                    "reason": "Create one explicit note."
                }))
                .is_err()
        );
    }
}
