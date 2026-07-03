use std::path::{Path, PathBuf};

use agl_store::{AglStore, PermissionGrantDraft, PermissionRequestDraft, PermissionRequestRecord};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    ToolCapability, ToolCatalog, ToolCatalogError, ToolDeclaration, ToolHandler, ToolId, ToolInput,
    ToolOperationKind, ToolOutput, ToolProviderDeclaration, ToolProviderId, ToolStateEffect,
    parse_tool_args as parse_args,
};

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

    pub fn dispatch(&self, name: &str, arguments: Value) -> Result<String> {
        match name {
            PERMISSIONS_STATUS_TOOL_ID => self.status(arguments),
            PERMISSIONS_REQUEST_TOOL_ID => self.request(arguments),
            PERMISSIONS_GRANT_TOOL_ID => self.grant(arguments),
            PERMISSIONS_REVOKE_TOOL_ID => self.revoke(arguments),
            _ => anyhow::bail!("unknown permission tool `{name}`"),
        }
    }

    fn status(&self, arguments: Value) -> Result<String> {
        parse_args::<StatusArgs>(PERMISSIONS_STATUS_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let pending = store.pending_permission_requests()?;
        let active = store.active_permission_grants()?;
        let mut output = format!(
            "tool=permissions.status\ncurrent_mode={}\nvisible_tools={}\ndynamic_grants={}\ngranted_visible_tools={}\nignored_grants={}\npending_requests={}\nactive_grants={}\ndefault_duration=one_turn",
            self.runtime_status.current_mode,
            self.runtime_status.visible_tools.join(","),
            self.runtime_status.dynamic_grants,
            self.runtime_status.granted_visible_tools.join(","),
            self.runtime_status.ignored_grants.join(","),
            pending.len(),
            active.len()
        );
        for request in pending {
            output.push('\n');
            output.push_str(&format!(
                "request id={} tools={} max_operation_kind={} duration={} status={}",
                request.id,
                request.requested_tools.join(","),
                request.max_operation_kind,
                request.duration,
                request.status.as_str()
            ));
        }
        for grant in active {
            output.push('\n');
            output.push_str(&format!(
                "grant id={} tool={} max_operation_kind={} duration={} status={}",
                grant.id,
                grant.tool_id,
                grant.max_operation_kind,
                grant.duration,
                grant.status.as_str()
            ));
        }
        Ok(output)
    }

    fn request(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<RequestArgs>(PERMISSIONS_REQUEST_TOOL_ID, arguments)?;
        let requested_tools = validate_requested_tools(args.tools)?;
        let max_operation_kind = args
            .max_operation_kind
            .or(args.mode)
            .unwrap_or_else(|| "write".to_string());
        validate_operation_kind(&max_operation_kind)?;
        let duration = args.duration.unwrap_or_else(|| "one_turn".to_string());
        let requester_ref = args
            .requester_ref
            .unwrap_or_else(|| "tool:permissions.request".to_string());
        let store = self.open_store()?;
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

    fn grant(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<GrantArgs>(PERMISSIONS_GRANT_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let grants = if let Some(request_id) = args.request_id {
            store.grant_permission_request(
                &request_id,
                args.granted_by_ref
                    .as_deref()
                    .unwrap_or("tool:permissions.grant"),
                args.resolution_ref.as_deref(),
            )?
        } else {
            let tool_id = args
                .tool_id
                .context("permissions.grant requires request_id or tool_id")?;
            validate_requested_tools(vec![tool_id.clone()])?;
            let max_operation_kind = args
                .max_operation_kind
                .unwrap_or_else(|| "write".to_string());
            validate_operation_kind(&max_operation_kind)?;
            vec![
                store.create_permission_grant(PermissionGrantDraft {
                    request_id: None,
                    tool_id,
                    max_operation_kind,
                    state_effects: args.state_effects.unwrap_or_default(),
                    scope: args.scope.unwrap_or_else(|| serde_json::json!({})),
                    duration: args.duration.unwrap_or_else(|| "one_turn".to_string()),
                    granted_by_ref: args
                        .granted_by_ref
                        .unwrap_or_else(|| "tool:permissions.grant".to_string()),
                })?,
            ]
        };
        let mut output = format!(
            "tool=permissions.grant\nstatus=granted\ngrants={}",
            grants.len()
        );
        for grant in grants {
            output.push('\n');
            output.push_str(&format!(
                "grant id={} tool={} max_operation_kind={} duration={} status={}",
                grant.id,
                grant.tool_id,
                grant.max_operation_kind,
                grant.duration,
                grant.status.as_str()
            ));
        }
        Ok(output)
    }

    fn revoke(&self, arguments: Value) -> Result<String> {
        let args = parse_args::<RevokeArgs>(PERMISSIONS_REVOKE_TOOL_ID, arguments)?;
        let store = self.open_store()?;
        let grant = store.revoke_permission_grant(&args.grant_id, args.revoke_ref.as_deref())?;
        Ok(format!(
            "tool=permissions.revoke\ngrant_id={}\ntool_id={}\nstatus={}",
            grant.id,
            grant.tool_id,
            grant.status.as_str()
        ))
    }

    fn open_store(&self) -> Result<AglStore> {
        AglStore::open_at(&self.store_root).with_context(|| {
            format!(
                "failed to open permission store {}",
                self.store_root.display()
            )
        })
    }
}

impl ToolHandler for PermissionTools {
    fn dispatch(&self, input: ToolInput) -> Result<ToolOutput> {
        let observation = self.dispatch(input.id.as_str(), input.arguments)?;
        Ok(ToolOutput { observation })
    }
}

pub fn declaration() -> ToolProviderDeclaration {
    ToolProviderDeclaration::new(
        ToolProviderId::new(PROVIDER_ID).expect("builtin permission provider id is valid"),
        "Permission Tools",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("builtin permission provider declaration is valid")
    .with_tool(tool(
        PERMISSIONS_STATUS_TOOL_ID,
        "Show pending permission requests and active grants.",
        ToolCapability::Read,
        ToolOperationKind::Read,
        &[],
        &[],
        true,
    ))
    .with_tool(tool(
        PERMISSIONS_REQUEST_TOOL_ID,
        "Create a pending permission request for exact tool IDs; this does not grant access.",
        ToolCapability::Write,
        ToolOperationKind::Approve,
        &[ToolStateEffect::StorePermissionRequests],
        &["tools", "reason"],
        true,
    ))
    .with_tool(tool(
        PERMISSIONS_GRANT_TOOL_ID,
        "Grant an existing permission request or an exact tool ID.",
        ToolCapability::Write,
        ToolOperationKind::Approve,
        &[ToolStateEffect::StorePermissionGrants],
        &[],
        false,
    ))
    .with_tool(tool(
        PERMISSIONS_REVOKE_TOOL_ID,
        "Revoke an active permission grant.",
        ToolCapability::Write,
        ToolOperationKind::Approve,
        &[ToolStateEffect::StorePermissionGrants],
        &["grant_id"],
        false,
    ))
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn tool(
    id: &str,
    description: &str,
    capability: ToolCapability,
    operation_kind: ToolOperationKind,
    state_effects: &[ToolStateEffect],
    required_arguments: &[&str],
    visible_in_read_only: bool,
) -> ToolDeclaration {
    ToolDeclaration::new(
        ToolId::new(id).expect("builtin permission tool id is valid"),
        description,
        capability,
        required_arguments.iter().copied(),
    )
    .with_operation_kind(operation_kind)
    .with_state_effects(state_effects.iter().copied())
    .visible_in_read_only(visible_in_read_only)
}

fn validate_requested_tools(tools: Vec<String>) -> Result<Vec<String>> {
    if tools.is_empty() {
        bail!("permissions.request tools cannot be empty");
    }
    let mut normalized = Vec::with_capacity(tools.len());
    let mut seen = std::collections::BTreeSet::new();
    for tool in tools {
        let id = ToolId::new(tool.clone())
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

fn validate_operation_kind(value: &str) -> Result<ToolOperationKind> {
    ToolOperationKind::parse(value)
        .with_context(|| format!("unknown permission operation kind `{value}`"))
}

fn render_permission_request_result(request: &PermissionRequestRecord) -> String {
    format!(
        "tool=permissions.request\nrequest_id={}\nstatus={}\ntools={}\nmax_operation_kind={}\nduration={}\nresult=pending_approval",
        request.id,
        request.status.as_str(),
        request.requested_tools.join(","),
        request.max_operation_kind,
        request.duration
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestArgs {
    tools: Vec<String>,
    reason: String,
    max_operation_kind: Option<String>,
    mode: Option<String>,
    state_effects: Option<Vec<String>>,
    scope: Option<Value>,
    duration: Option<String>,
    requester_ref: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantArgs {
    request_id: Option<String>,
    tool_id: Option<String>,
    max_operation_kind: Option<String>,
    state_effects: Option<Vec<String>>,
    scope: Option<Value>,
    duration: Option<String>,
    granted_by_ref: Option<String>,
    resolution_ref: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeArgs {
    grant_id: String,
    revoke_ref: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::test_support::temp_root;

    use super::*;

    #[test]
    fn permission_request_creates_pending_one_turn_request() {
        let root = temp_root("permission-request");
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

        assert!(output.contains("tool=permissions.request"));
        assert!(output.contains("result=pending_approval"));
        assert!(output.contains("duration=one_turn"));

        let status = tools
            .dispatch(PERMISSIONS_STATUS_TOOL_ID, json!({}))
            .unwrap();
        assert!(status.contains("current_mode=unknown"));
        assert!(status.contains("dynamic_grants=false"));
        assert!(status.contains("pending_requests=1"));
        assert!(status.contains("active_grants=0"));
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
        let root = temp_root("permission-status");
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

        assert!(status.contains("current_mode=read-only"));
        assert!(status.contains("visible_tools=fs.read,permissions.status,permissions.request"));
        assert!(status.contains("dynamic_grants=false"));
        assert!(status.contains("granted_visible_tools="));
        assert!(status.contains("ignored_grants="));
    }
}
