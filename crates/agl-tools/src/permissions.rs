use std::path::{Path, PathBuf};

use agl_capabilities::{
    ActionDeclaration, ActionHandler, ActionHandlerError, ActionInvocation, ActionResult,
    ActionVisibility, CapabilityId, OperationKind, ProviderDeclaration, ProviderId, StateEffect,
};
use agl_store::{AglStore, PermissionGrantDraft, PermissionRequestDraft, PermissionRequestRecord};
use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ToolCatalog, ToolCatalogError, parse_action_args as parse_args};

pub const PROVIDER_ID: &str = "permission-tools";
pub const PERMISSIONS_STATUS_TOOL_ID: &str = "permissions.status";
pub const PERMISSIONS_REQUEST_TOOL_ID: &str = "permissions.request";
pub const PERMISSIONS_GRANT_TOOL_ID: &str = "permissions.grant";
pub const PERMISSIONS_REVOKE_TOOL_ID: &str = "permissions.revoke";

#[derive(Clone, Debug)]
pub struct PermissionTools {
    store_root: PathBuf,
    runtime_status: PermissionRuntimeStatus,
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
    pub fn new(store_root: impl AsRef<Path>) -> Self {
        Self {
            store_root: store_root.as_ref().to_path_buf(),
            runtime_status: PermissionRuntimeStatus::default(),
        }
    }

    pub fn with_runtime_status(mut self, runtime_status: PermissionRuntimeStatus) -> Self {
        self.runtime_status = runtime_status;
        self
    }

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<Value> {
        match name {
            PERMISSIONS_STATUS_TOOL_ID => self.status(arguments),
            PERMISSIONS_REQUEST_TOOL_ID => self.request(arguments),
            PERMISSIONS_GRANT_TOOL_ID => self.grant(arguments),
            PERMISSIONS_REVOKE_TOOL_ID => self.revoke(arguments),
            _ => anyhow::bail!("unknown permission tool `{name}`"),
        }
    }

    fn status(&self, arguments: Value) -> Result<Value> {
        parse_args::<StatusArgs>(PERMISSIONS_STATUS_TOOL_ID, arguments)?;
        let store = self.open_store_read_only()?;
        let pending = store.pending_permission_requests()?;
        let active = store.active_permission_grants()?;
        let pending_requests = pending
            .into_iter()
            .map(|request| {
                json!({
                    "request_id": request.id,
                    "tools": request.requested_tools,
                    "max_operation_kind": request.max_operation_kind,
                    "duration": request.duration,
                    "status": request.status.as_str(),
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
                    "duration": grant.duration,
                    "status": grant.status.as_str(),
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
            .as_str()
            .to_string();
        let duration = args.duration.unwrap_or_else(|| "one_turn".to_string());
        let requester_ref = args
            .requester_ref
            .unwrap_or_else(|| "tool:permissions.request".to_string());
        let store = self.open_store_writable()?;
        let request = store.create_permission_request(PermissionRequestDraft {
            requested_tools,
            max_operation_kind,
            state_effects: args.state_effects.unwrap_or_default(),
            scope: args.scope.unwrap_or_else(|| serde_json::json!({})),
            duration,
            reason: args.reason,
            requester_ref,
        })?;
        Ok(render_permission_request_result(&request))
    }

    fn grant(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<GrantArgs>(PERMISSIONS_GRANT_TOOL_ID, arguments)?;
        let store = self.open_store_writable()?;
        let grants = match args {
            GrantArgs::Request(args) => store.grant_permission_request(
                &args.request_id,
                args.granted_by_ref
                    .as_deref()
                    .unwrap_or("tool:permissions.grant"),
                args.resolution_ref.as_deref(),
            )?,
            GrantArgs::Direct(args) => {
                validate_requested_tools(vec![args.tool_id.clone()])?;
                let max_operation_kind = args
                    .max_operation_kind
                    .unwrap_or(OperationKindArg::Write)
                    .as_str()
                    .to_string();
                vec![
                    store.create_permission_grant(PermissionGrantDraft {
                        request_id: None,
                        tool_id: args.tool_id,
                        max_operation_kind,
                        state_effects: args.state_effects.unwrap_or_default(),
                        scope: args.scope.unwrap_or_else(|| serde_json::json!({})),
                        duration: args.duration.unwrap_or_else(|| "one_turn".to_string()),
                        granted_by_ref: args
                            .granted_by_ref
                            .unwrap_or_else(|| "tool:permissions.grant".to_string()),
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
                    "duration": grant.duration,
                    "status": grant.status.as_str(),
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

    fn revoke(&self, arguments: Value) -> Result<Value> {
        let args = parse_args::<RevokeArgs>(PERMISSIONS_REVOKE_TOOL_ID, arguments)?;
        let store = self.open_store_writable()?;
        let grant = store.revoke_permission_grant(&args.grant_id, args.revoke_ref.as_deref())?;
        Ok(json!({
            "tool": PERMISSIONS_REVOKE_TOOL_ID,
            "grant_id": grant.id,
            "tool_id": grant.tool_id,
            "status": grant.status.as_str(),
        }))
    }

    fn open_store_read_only(&self) -> Result<AglStore> {
        AglStore::open_current_read_only_at(&self.store_root).with_context(|| {
            format!(
                "failed to open permission store {}",
                self.store_root.display()
            )
        })
    }

    fn open_store_writable(&self) -> Result<AglStore> {
        AglStore::open_current_at(&self.store_root).with_context(|| {
            format!(
                "failed to open permission store {}",
                self.store_root.display()
            )
        })
    }
}

impl ActionHandler for PermissionTools {
    fn dispatch(
        &self,
        invocation: ActionInvocation,
    ) -> std::result::Result<ActionResult, ActionHandlerError> {
        let data = self.dispatch(invocation.capability_id.as_str(), invocation.arguments)?;
        Ok(ActionResult::new(data))
    }
}

pub fn declaration() -> ProviderDeclaration {
    ProviderDeclaration::builtin(
        ProviderId::new(PROVIDER_ID).expect("builtin permission provider id is valid"),
        "Permission Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin permission provider declaration is valid")
    .with_action(action::<StatusArgs>(
        PERMISSIONS_STATUS_TOOL_ID,
        "Show pending permission requests and active grants.",
        OperationKind::Read,
        &[],
        true,
    ))
    .with_action(action::<RequestArgs>(
        PERMISSIONS_REQUEST_TOOL_ID,
        "Create a pending permission request for exact tool IDs; this does not grant access.",
        OperationKind::Approve,
        &[StateEffect::StorePermissionRequests],
        true,
    ))
    .with_action(action::<GrantArgs>(
        PERMISSIONS_GRANT_TOOL_ID,
        "Grant an existing permission request or an exact tool ID.",
        OperationKind::Approve,
        &[
            StateEffect::StorePermissionGrants,
            StateEffect::StorePermissionRequests,
        ],
        false,
    ))
    .with_action(action::<RevokeArgs>(
        PERMISSIONS_REVOKE_TOOL_ID,
        "Revoke an active permission grant.",
        OperationKind::Approve,
        &[StateEffect::StorePermissionGrants],
        false,
    ))
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn action<T: JsonSchema>(
    id: &str,
    description: &str,
    operation_kind: OperationKind,
    state_effects: &[StateEffect],
    visible_in_read_only: bool,
) -> ActionDeclaration {
    ActionDeclaration::from_schema::<T>(
        CapabilityId::new(id).expect("builtin permission tool id is valid"),
        description,
        operation_kind,
    )
    .expect("builtin permission tool declaration schema is valid")
    .with_state_effects(state_effects.iter().copied())
    .with_visibility(ActionVisibility {
        visible_in_read_only,
    })
}

fn validate_requested_tools(tools: Vec<String>) -> Result<Vec<String>> {
    if tools.is_empty() {
        bail!("permissions.request tools cannot be empty");
    }
    let mut normalized = Vec::with_capacity(tools.len());
    let mut seen = std::collections::BTreeSet::new();
    for tool in tools {
        let id = CapabilityId::new(tool.clone())
            .with_context(|| format!("permissions.request requested tool id is invalid: {tool}"))?;
        if id.as_str().starts_with("permissions.") {
            bail!("permission tools cannot request or grant permission tools");
        }
        if seen.insert(id.as_str().to_string()) {
            normalized.push(id.as_str().to_string());
        }
    }
    Ok(normalized)
}

fn render_permission_request_result(request: &PermissionRequestRecord) -> Value {
    json!({
        "tool": PERMISSIONS_REQUEST_TOOL_ID,
        "request_id": request.id,
        "status": request.status.as_str(),
        "tools": request.requested_tools,
        "max_operation_kind": request.max_operation_kind,
        "duration": request.duration,
        "result": "pending_approval",
    })
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum OperationKindArg {
    Read,
    Write,
    Execute,
    Approve,
    Admin,
}

impl OperationKindArg {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Execute => "execute",
            Self::Approve => "approve",
            Self::Admin => "admin",
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
    scope: Option<Value>,
    duration: Option<String>,
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
    scope: Option<Value>,
    duration: Option<String>,
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

    use crate::test_support::{migrated_temp_root, temp_root};

    use super::*;

    #[test]
    fn permission_request_creates_pending_one_turn_request() {
        let root = migrated_temp_root("permission-request");
        let tools = PermissionTools::new(&root);

        let output = tools
            .dispatch(
                PERMISSIONS_REQUEST_TOOL_ID,
                json!({
                    "tools": ["notes.add"],
                    "reason": "Create one explicit note.",
                    "requester_ref": "chat:turn-1"
                }),
            )
            .unwrap();

        assert_eq!(output["tool"], PERMISSIONS_REQUEST_TOOL_ID);
        assert_eq!(output["result"], "pending_approval");
        assert_eq!(output["duration"], "one_turn");
        assert_eq!(output["tools"], json!(["notes.add"]));

        let status = tools
            .dispatch(PERMISSIONS_STATUS_TOOL_ID, json!({}))
            .unwrap();
        assert_eq!(status["current_mode"], "unknown");
        assert_eq!(status["dynamic_grants"], false);
        assert_eq!(status["pending_request_count"], 1);
        assert_eq!(status["active_grant_count"], 0);
        assert_eq!(status["pending_requests"][0]["tools"], json!(["notes.add"]));
    }

    #[test]
    fn permission_request_rejects_permission_tools() {
        let root = temp_root("permission-reject");
        let tools = PermissionTools::new(&root);

        let err = tools
            .dispatch(
                PERMISSIONS_REQUEST_TOOL_ID,
                json!({
                    "tools": ["permissions.grant"],
                    "reason": "grant myself"
                }),
            )
            .unwrap_err();

        assert!(err.to_string().contains("permission tools cannot request"));
    }

    #[test]
    fn permission_status_reports_runtime_snapshot() {
        let root = migrated_temp_root("permission-status");
        let tools = PermissionTools::new(&root).with_runtime_status(PermissionRuntimeStatus {
            current_mode: "read-only".to_string(),
            visible_tools: vec![
                "fs.read".to_string(),
                "permissions.status".to_string(),
                "permissions.request".to_string(),
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
            json!(["fs.read", "permissions.status", "permissions.request"])
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
            .action(&CapabilityId::new(PERMISSIONS_REQUEST_TOOL_ID).unwrap())
            .unwrap();
        assert_eq!(request.input_schema["additionalProperties"], false);
        assert!(request.visibility.visible_in_read_only);
        let schema = request.compile_schema().unwrap();
        assert!(
            schema
                .validate(&json!({
                    "tools": ["notes.add"],
                    "reason": "Create one explicit note."
                }))
                .is_ok()
        );
        assert!(schema.validate(&json!({"tools": ["notes.add"]})).is_err());
        assert!(
            schema
                .validate(&json!({
                    "tools": ["notes.add"],
                    "reason": "Create one explicit note.",
                    "extra": true
                }))
                .is_err()
        );
        assert!(
            schema
                .validate(&json!({
                    "tools": "notes.add",
                    "reason": "Create one explicit note."
                }))
                .is_err()
        );
    }
}
