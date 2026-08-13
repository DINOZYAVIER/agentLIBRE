use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agl_exec::{
    AuthorityFingerprint, CallerNamespace, CallerOwnerKind, EnvironmentOverride,
    ExecutionAuthorization, ExecutionContextSnapshot, ExecutionCorrelation, ExecutionCursor,
    ExecutionExit, ExecutionGrantLease, ExecutionId, ExecutionIo, ExecutionKind,
    ExecutionLeaseOrigin, ExecutionLimits, ExecutionOwner, ExecutionProfile, ExecutionRequest,
    ExecutionStatus, KillMode, OpaqueOwnerId, ProcessBytes, ProcessBytesEncoding, TerminalSize,
    resolve_execution_directory,
};
#[cfg(test)]
use agl_exec::{CallerOwner, CallerRole};
use agl_ids::{ExecutionScope, RunId, SessionId, StepId};
use agl_kernel::{
    EffectDeclaration, EffectId, ExtensionDescriptor, ExtensionId, ObservedEffect, OperationKind,
    ToolDeclaration, ToolDispatchContext, ToolDispatchControl, ToolHandler, ToolHandlerError,
    ToolId, ToolInvocation, ToolResult,
};
use agl_process::TerminalEndpoint;
use agl_terminal::environment::TerminalEnvironmentRequest;
use agl_terminal::{
    AdmittedShellKind, AdmittedShellProfile, HostStartupPolicy, TerminalOperation, TerminalOwner,
    TerminalTopologyId,
};
use agl_terminal_client::{TerminalClient, UnixTerminalTransport};
use agl_terminal_protocol::{ExecutionAdmission, ExecutionOperation, TerminalAdmission};
use anyhow::{Context, Result, bail, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::parse_tool_args as parse_args;

pub const EXTENSION_ID: &str = "core.process";
pub const PROCESS_PWD_TOOL_ID: &str = "core.process:process.pwd";
pub const PROCESS_CD_TOOL_ID: &str = "core.process:process.cd";
pub const PROCESS_EXEC_TOOL_ID: &str = "core.process:process.exec";
pub const PROCESS_START_TOOL_ID: &str = "core.process:process.start";
pub const PROCESS_STATUS_TOOL_ID: &str = "core.process:process.status";
pub const PROCESS_READ_TOOL_ID: &str = "core.process:process.read";
pub const PROCESS_WRITE_TOOL_ID: &str = "core.process:process.write";
pub const PROCESS_RESIZE_TOOL_ID: &str = "core.process:process.resize";
pub const PROCESS_KILL_TOOL_ID: &str = "core.process:process.kill";
pub const SHELL_EXEC_TOOL_ID: &str = "core.process:shell.exec";

const MAX_PROCESS_PATH_BYTES: usize = 4_096;
const MAX_PROCESS_TEXT_BYTES: usize = 65_536;
const MAX_PROCESS_ARGUMENTS: usize = 1_024;
const MAX_ENVIRONMENT_ENTRIES: usize = 256;
const MAX_ENVIRONMENT_NAME_BYTES: usize = 256;
const MAX_COW_FILES: usize = 20_000;
const MAX_COW_BYTES: usize = 256 * 1024 * 1024;

pub const PROCESS_TOOL_IDS: &[&str] = &[
    PROCESS_PWD_TOOL_ID,
    PROCESS_CD_TOOL_ID,
    PROCESS_EXEC_TOOL_ID,
    PROCESS_START_TOOL_ID,
    PROCESS_STATUS_TOOL_ID,
    PROCESS_READ_TOOL_ID,
    PROCESS_WRITE_TOOL_ID,
    PROCESS_RESIZE_TOOL_ID,
    PROCESS_KILL_TOOL_ID,
    SHELL_EXEC_TOOL_ID,
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessExecutionAdmission {
    pub snapshot: ExecutionContextSnapshot,
    pub owner: ExecutionOwner,
    /// Durable Human session that owns the terminal topology. A session-owned
    /// root run uses its own session ID; a run-owned subagent resolves this to
    /// the root run's durable session before tool dispatch.
    pub durable_session_id: SessionId,
}

pub trait ProcessExecutionContext: Send + Sync {
    fn load(&self, scope: &ExecutionScope) -> Result<ProcessExecutionAdmission>;

    fn compare_and_set_working_directory(
        &self,
        scope: &ExecutionScope,
        expected_revision: u64,
        next: ExecutionContextSnapshot,
    ) -> Result<ProcessExecutionAdmission>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessToolRuntimeConfig {
    pub base_environment: EnvironmentOverride,
    pub maximum_environment_bytes: usize,
    pub runtime_read_only_roots: Vec<PathBuf>,
    pub default_foreground_timeout: Duration,
    pub maximum_foreground_timeout: Duration,
    pub max_input_bytes: usize,
    pub max_result_bytes: usize,
    pub max_spool_bytes: u64,
    pub default_terminal_size: TerminalSize,
}

impl ProcessToolRuntimeConfig {
    pub fn validate(&self) -> Result<()> {
        self.base_environment
            .validate()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        ensure!(
            self.maximum_environment_bytes > 0
                && !self.default_foreground_timeout.is_zero()
                && self.maximum_foreground_timeout >= self.default_foreground_timeout
                && self.max_input_bytes > 0
                && self.max_result_bytes > 0
                && self.max_spool_bytes > 0,
            "process tool runtime limits must be nonzero and internally ordered"
        );
        self.default_terminal_size
            .validate()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct ProcessTools {
    terminal: Arc<TerminalEndpoint>,
    context: Arc<dyn ProcessExecutionContext>,
    config: ProcessToolRuntimeConfig,
}

impl ProcessTools {
    pub fn new(
        terminal: Arc<TerminalEndpoint>,
        context: Arc<dyn ProcessExecutionContext>,
        config: ProcessToolRuntimeConfig,
    ) -> Result<Self> {
        config.validate()?;
        process_io_runtime()?;
        Ok(Self {
            terminal,
            context,
            config,
        })
    }

    pub fn terminal_endpoint(&self) -> Arc<TerminalEndpoint> {
        Arc::clone(&self.terminal)
    }

    async fn execute(&self, context: ToolDispatchContext) -> Result<Value> {
        ensure!(
            tokio::runtime::Handle::try_current().is_ok(),
            "process Tool dispatch is outside its owned Tokio runtime"
        );
        let id = context.invocation().tool_id.as_str();
        match id {
            PROCESS_PWD_TOOL_ID => self.pwd(context),
            PROCESS_CD_TOOL_ID => self.cd(context),
            PROCESS_EXEC_TOOL_ID => self.exec(context).await,
            PROCESS_START_TOOL_ID => self.start(context).await,
            PROCESS_STATUS_TOOL_ID => self.status(context).await,
            PROCESS_READ_TOOL_ID => self.read(context).await,
            PROCESS_WRITE_TOOL_ID => self.write(context).await,
            PROCESS_RESIZE_TOOL_ID => self.resize(context).await,
            PROCESS_KILL_TOOL_ID => self.kill(context).await,
            SHELL_EXEC_TOOL_ID => self.shell(context).await,
            _ => bail!("unknown process tool `{id}`"),
        }
    }

    fn pwd(&self, context: ToolDispatchContext) -> Result<Value> {
        let invocation = context.into_invocation();
        parse_args::<EmptyArgs>(PROCESS_PWD_TOOL_ID, invocation.arguments)?;
        let admission = self.context.load(&invocation.scope)?;
        Ok(json!({
            "tool": PROCESS_PWD_TOOL_ID,
            "status": "ok",
            "working_directory": admission.snapshot.working_directory,
            "workspace_root": admission.snapshot.workspace_root,
            "revision": admission.snapshot.revision,
        }))
    }

    fn cd(&self, context: ToolDispatchContext) -> Result<Value> {
        let host_authorized = context
            .authorized_conditional_effects()
            .contains(&EffectId::host_process_execution());
        let invocation = context.into_invocation();
        let args = parse_args::<CdArgs>(PROCESS_CD_TOOL_ID, invocation.arguments)?;
        validate_text(
            &args.path,
            "core.process:process.cd path",
            false,
            MAX_PROCESS_PATH_BYTES,
        )?;
        let profile: ExecutionProfile = args.profile.unwrap_or_default().into();
        let admission = self.context.load(&invocation.scope)?;
        let resolved = resolve_execution_directory(
            &admission.snapshot,
            Path::new(&args.path),
            profile,
            host_authorized,
        )
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let mut next = admission.snapshot.clone();
        next.working_directory = resolved;
        next.revision = next
            .revision
            .checked_add(1)
            .context("execution context revision overflow")?;
        next.profile_metadata = match profile {
            ExecutionProfile::Workspace => "workspace",
            ExecutionProfile::Host => "host",
        }
        .to_owned();
        let updated = self.context.compare_and_set_working_directory(
            &invocation.scope,
            admission.snapshot.revision,
            next,
        )?;
        Ok(json!({
            "tool": PROCESS_CD_TOOL_ID,
            "status": "changed",
            "working_directory": updated.snapshot.working_directory,
            "profile": profile,
            "revision": updated.snapshot.revision,
        }))
    }

    async fn exec(&self, context: ToolDispatchContext) -> Result<Value> {
        let args =
            parse_args::<ArgvArgs>(PROCESS_EXEC_TOOL_ID, context.invocation().arguments.clone())?;
        let request = self.argv_request(&context, args, ExecutionIo::Pipes, true)?;
        let client = self.client(&context)?;
        let started = start_execution(&client, &context, request).await?;
        let status = wait_execution(&client, &context, started).await?;
        self.foreground_result(&client, &context, PROCESS_EXEC_TOOL_ID, status)
            .await
    }

    async fn start(&self, context: ToolDispatchContext) -> Result<Value> {
        let args = parse_args::<StartArgs>(
            PROCESS_START_TOOL_ID,
            context.invocation().arguments.clone(),
        )?;
        let io = args.io.into();
        let terminal_size =
            terminal_size(args.terminal_size, io, self.config.default_terminal_size)?;
        let request = self.argv_request(&context, args.argv, io, false)?;
        let request = ExecutionRequest {
            terminal_size,
            ..request
        };
        let client = self.client(&context)?;
        let status = start_execution(&client, &context, request).await?;
        Ok(background_result(PROCESS_START_TOOL_ID, &status))
    }

    async fn shell(&self, context: ToolDispatchContext) -> Result<Value> {
        let args =
            parse_args::<ShellArgs>(SHELL_EXEC_TOOL_ID, context.invocation().arguments.clone())?;
        validate_shell_contract(&args).map_err(|error| {
            anyhow::Error::new(ToolHandlerError::new(
                "invalid_effect_envelope",
                format!("{error:#}"),
                json!({}),
            ))
        })?;
        validate_text(
            &args.command,
            "core.process:shell.exec command",
            true,
            MAX_PROCESS_TEXT_BYTES,
        )?;
        ensure!(
            !args.background.unwrap_or(false),
            "core.process:shell.exec is foreground-only; use the explicit process lifecycle Tools for background work"
        );
        let profile: ExecutionProfile = args.profile.unwrap_or_default().into();
        if profile == ExecutionProfile::Workspace {
            if args.workspace_access == WorkspaceAccessArg::Write {
                return self.cow_workspace_shell(context, args).await;
            }
            return self.agent_shell(context, args).await;
        }
        ensure!(
            args.workspace_access == WorkspaceAccessArg::ReadOnly,
            "host shell fallback cannot receive repository mutation authority"
        );
        self.one_shot_host_shell(context, args).await
    }

    async fn agent_shell(&self, context: ToolDispatchContext, args: ShellArgs) -> Result<Value> {
        ensure!(
            args.cwd.is_none() && args.env.is_none() && args.terminal_size.is_none(),
            "persistent workspace core.process:shell.exec uses the owner's durable cwd, environment, and terminal size"
        );
        ensure!(
            !args.background.unwrap_or(false),
            "persistent workspace core.process:shell.exec uses native shell job control; put `&` in the command"
        );
        ensure!(
            !args.login.unwrap_or(false),
            "persistent workspace core.process:shell.exec is interactive and non-login"
        );
        let invocation = context.invocation();
        let admission = self.context.load(&invocation.scope)?;
        admission
            .snapshot
            .shell
            .verify_executable()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let (owner, root_run_id) = terminal_owner(&admission)?;
        let shell = admitted_agent_shell(&admission.snapshot)?;
        let environment = self.agent_terminal_environment(&admission.snapshot, &shell)?;
        let timeout_ms = self.timeout_ms(args.timeout_ms, true, context.control().remaining())?;
        let mut terminal_admission = TerminalAdmission {
            topology_id: TerminalTopologyId::new(OpaqueOwnerId::new(
                admission.durable_session_id.as_str(),
            )?),
            owner,
            authority_scope: OpaqueOwnerId::new(root_run_id.as_str())?,
            correlation: execution_correlation(&invocation.scope)?,
            context: admission.snapshot,
            profile: ExecutionProfile::Workspace,
            shell,
            environment,
            runtime_read_only_roots: self.config.runtime_read_only_roots.clone(),
            host_startup: HostStartupPolicy::ManagedOnly,
            authorization: ExecutionAuthorization::default(),
            grant_lease: None,
            terminal_size: self.config.default_terminal_size,
            limits: self.execution_limits(None),
            history_seed: Vec::new(),
            authority_fingerprint: authority_fingerprint(&context)?,
            request_fingerprint: String::new(),
            operations: all_terminal_operations(),
        };
        terminal_admission
            .seal_request_fingerprint()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let client = self.client(&context)?;
        let token = CancellationToken::new();
        let result = controlled(
            context.control(),
            token.clone(),
            client.execute_agent_command(terminal_admission, args.command, timeout_ms, token),
        )
        .await?;
        let token = CancellationToken::new();
        let mut output = controlled(
            context.control(),
            token.clone(),
            client.read_execution(
                result.execution_id.clone(),
                ExecutionCursor {
                    after_sequence: result.output.after_sequence,
                },
                checked_maximum_bytes(self.config.max_result_bytes)?,
                token,
            ),
        )
        .await?;
        output
            .chunks
            .retain(|chunk| chunk.sequence <= result.output.through_sequence);
        let next_sequence = output
            .chunks
            .last()
            .map_or(result.output.after_sequence, |chunk| chunk.sequence);
        let output_truncated =
            output.output_truncated || next_sequence < result.output.through_sequence;
        Ok(json!({
            "tool": SHELL_EXEC_TOOL_ID,
            "guarantee_class": "degraded_shell_fallback",
            "semantic_intent": args.semantic_intent,
            "fallback_reason": args.fallback_reason,
            "requested_workspace_access": args.workspace_access,
            "expected_effects": args.expected_effects,
            "observed_effects": {
                "process": true,
                "workspace_paths": [],
                "external": [],
            },
            "terminal_id": result.terminal_id,
            "execution_id": result.execution_id,
            "command_sequence": result.command_sequence,
            "cwd": result.cwd,
            "exit": result.exit,
            "after_sequence": result.output.after_sequence,
            "through_sequence": result.output.through_sequence,
            "chunks": output.chunks,
            "next_sequence": next_sequence,
            "output_truncated": output_truncated,
            "output_expired": output.output_expired,
        }))
    }

    async fn cow_workspace_shell(
        &self,
        context: ToolDispatchContext,
        args: ShellArgs,
    ) -> Result<Value> {
        ensure!(
            args.cwd.is_none() && args.env.is_none() && args.terminal_size.is_none(),
            "workspace shell fallback uses the caller's durable cwd, environment, and terminal size"
        );
        ensure!(
            !args.login.unwrap_or(false),
            "workspace shell fallback is non-login"
        );
        let invocation = context.invocation();
        let admission = self.context.load(&invocation.scope)?;
        admission
            .snapshot
            .shell
            .verify_executable()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let transaction = CowWorkspaceTransaction::create(&admission.snapshot.workspace_root)?;
        let relative_cwd = admission
            .snapshot
            .working_directory
            .strip_prefix(&admission.snapshot.workspace_root)
            .context("workspace cwd is outside the workspace root")?;
        let cwd = transaction
            .view_root()
            .join(relative_cwd)
            .canonicalize()
            .context("failed to resolve copy-on-write workspace cwd")?;
        let shell = admitted_agent_shell(&admission.snapshot)?;
        let environment = self.environment(&admission.snapshot, None)?;
        let mut shell_args = admission.snapshot.shell.command_args.clone();
        shell_args.push(args.command.clone());
        let timeout_ms = self.timeout_ms(args.timeout_ms, true, context.control().remaining())?;
        let request = ExecutionRequest {
            owner: admission.owner.clone(),
            correlation: execution_correlation(&invocation.scope)?,
            kind: ExecutionKind::Shell,
            argv0: shell.snapshot.program.display().to_string(),
            program: shell.snapshot.program,
            program_digest: Some(admission.snapshot.shell.executable_digest.clone()),
            args: shell_args,
            workspace_root: transaction.view_root().to_path_buf(),
            cwd,
            read_only_roots: self.config.runtime_read_only_roots.clone(),
            environment,
            stdin: None,
            close_stdin_after_initial: false,
            io: ExecutionIo::Pty,
            terminal_size: Some(self.config.default_terminal_size),
            profile: ExecutionProfile::Workspace,
            authorization: ExecutionAuthorization {
                workspace_write: true,
                ..ExecutionAuthorization::default()
            },
            grant_lease: None,
            limits: self.execution_limits(timeout_ms),
        };
        let client = self.client(&context)?;
        let status = start_execution(&client, &context, request).await?;
        let status = wait_execution(&client, &context, status).await?;
        let diff = transaction
            .diff(&args.expected_effects.workspace_paths)
            .map_err(|error| match error.downcast::<EffectEnvelopeViolation>() {
                Ok(error) => anyhow::Error::new(ToolHandlerError::new(
                    "invalid_effect_envelope",
                    error.to_string(),
                    json!({}),
                )),
                Err(error) => error,
            })?;
        let committed = matches!(status.exit, Some(ExecutionExit::Code { code: 0 }));
        let patch_receipt = if committed && !diff.operations.is_empty() {
            Some(
                crate::CoreTools::new(&admission.snapshot.workspace_root)?
                    .apply_patch_for_tool(json!({"operations": diff.operations}))
                    .map_err(anyhow::Error::new)?,
            )
        } else {
            None
        };
        let mut result = self
            .foreground_result(&client, &context, SHELL_EXEC_TOOL_ID, status)
            .await?;
        let object = result
            .as_object_mut()
            .context("shell foreground result must be an object")?;
        object.insert(
            "guarantee_class".to_owned(),
            json!("degraded_shell_fallback"),
        );
        object.insert("semantic_intent".to_owned(), json!(args.semantic_intent));
        object.insert("fallback_reason".to_owned(), json!(args.fallback_reason));
        object.insert(
            "requested_workspace_access".to_owned(),
            json!(args.workspace_access),
        );
        object.insert("expected_effects".to_owned(), json!(args.expected_effects));
        object.insert(
            "observed_effects".to_owned(),
            json!({
                "process": true,
                "workspace_paths": diff.changed_paths,
                "external": [],
            }),
        );
        object.insert(
            "workspace_transaction".to_owned(),
            json!({
                "state": if committed { "committed" } else { "discarded" },
                "patch_receipt": patch_receipt,
            }),
        );
        Ok(result)
    }

    async fn one_shot_host_shell(
        &self,
        context: ToolDispatchContext,
        args: ShellArgs,
    ) -> Result<Value> {
        let profile = ExecutionProfile::Host;
        let login = args.login.unwrap_or(false);
        let invocation = context.invocation();
        let (authorization, grant_lease) = execution_authorization(
            &context,
            profile,
            login,
            args.workspace_access == WorkspaceAccessArg::Write,
        )?;
        let admission = self.context.load(&invocation.scope)?;
        admission
            .snapshot
            .shell
            .verify_executable()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let cwd = resolve_cwd(
            &admission.snapshot,
            args.cwd.as_deref(),
            profile,
            authorization.host_process_execution,
        )?;
        let mut shell_args = if login {
            admission
                .snapshot
                .shell
                .login_command_args
                .clone()
                .context("core.process:shell.exec login startup is not configured")?
        } else {
            admission.snapshot.shell.command_args.clone()
        };
        shell_args.push(args.command);
        let environment = self.environment(&admission.snapshot, args.env)?;
        let timeout_ms = self.timeout_ms(args.timeout_ms, true, context.control().remaining())?;
        let size = args.terminal_size.unwrap_or(TerminalSizeArgs {
            columns: self.config.default_terminal_size.columns,
            rows: self.config.default_terminal_size.rows,
        });
        let request = ExecutionRequest {
            owner: admission.owner.clone(),
            correlation: execution_correlation(&invocation.scope)?,
            kind: ExecutionKind::Shell,
            program: admission.snapshot.shell.program.clone(),
            argv0: admission.snapshot.shell.program.display().to_string(),
            program_digest: Some(admission.snapshot.shell.executable_digest.clone()),
            args: shell_args,
            workspace_root: admission.snapshot.workspace_root,
            cwd,
            read_only_roots: self.config.runtime_read_only_roots.clone(),
            environment,
            stdin: None,
            close_stdin_after_initial: false,
            io: ExecutionIo::Pty,
            terminal_size: Some(size.into()),
            profile,
            authorization,
            grant_lease,
            limits: self.execution_limits(timeout_ms),
        };
        let client = self.client(&context)?;
        let status = start_execution(&client, &context, request).await?;
        let status = wait_execution(&client, &context, status).await?;
        let mut result = self
            .foreground_result(&client, &context, SHELL_EXEC_TOOL_ID, status)
            .await?;
        let object = result
            .as_object_mut()
            .context("shell foreground result must be an object")?;
        object.insert(
            "guarantee_class".to_owned(),
            json!("degraded_shell_fallback"),
        );
        object.insert("semantic_intent".to_owned(), json!(args.semantic_intent));
        object.insert("fallback_reason".to_owned(), json!(args.fallback_reason));
        object.insert(
            "requested_workspace_access".to_owned(),
            json!(args.workspace_access),
        );
        object.insert("expected_effects".to_owned(), json!(args.expected_effects));
        object.insert(
            "observed_effects".to_owned(),
            json!({"process": true, "workspace_paths": [], "external": ["host_process"]}),
        );
        Ok(result)
    }

    async fn status(&self, context: ToolDispatchContext) -> Result<Value> {
        let invocation = context.invocation();
        let args =
            parse_args::<ExecutionIdArgs>(PROCESS_STATUS_TOOL_ID, invocation.arguments.clone())?;
        let execution_id = parse_execution_id(&args.execution_id)?;
        self.context.load(&invocation.scope)?;
        let client = self.client(&context)?;
        let token = CancellationToken::new();
        let status = controlled(
            context.control(),
            token.clone(),
            client.inspect_execution(execution_id, token),
        )
        .await?;
        Ok(json!({"tool": PROCESS_STATUS_TOOL_ID, "status": status}))
    }

    async fn read(&self, context: ToolDispatchContext) -> Result<Value> {
        let invocation = context.invocation();
        let args =
            parse_args::<ReadOutputArgs>(PROCESS_READ_TOOL_ID, invocation.arguments.clone())?;
        ensure!(
            args.max_bytes > 0 && args.max_bytes <= self.config.max_result_bytes,
            "core.process:process.read max_bytes must be between 1 and {}",
            self.config.max_result_bytes
        );
        let execution_id = parse_execution_id(&args.execution_id)?;
        self.context.load(&invocation.scope)?;
        let client = self.client(&context)?;
        let token = CancellationToken::new();
        let output = controlled(
            context.control(),
            token.clone(),
            client.read_execution(
                execution_id,
                ExecutionCursor {
                    after_sequence: args.after_sequence.unwrap_or(0),
                },
                checked_maximum_bytes(args.max_bytes)?,
                token,
            ),
        )
        .await?;
        Ok(json!({"tool": PROCESS_READ_TOOL_ID, "output": output}))
    }

    async fn write(&self, context: ToolDispatchContext) -> Result<Value> {
        let invocation = context.invocation();
        let args = parse_args::<WriteArgs>(PROCESS_WRITE_TOOL_ID, invocation.arguments.clone())?;
        let execution_id = parse_execution_id(&args.execution_id)?;
        let bytes: ProcessBytes = args.bytes.into();
        bytes
            .decode(self.config.max_input_bytes)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        self.context.load(&invocation.scope)?;
        let client = self.client(&context)?;
        let token = CancellationToken::new();
        let attachment = controlled(
            context.control(),
            token.clone(),
            client.attach_execution(execution_id.clone(), true, token),
        )
        .await?;
        let lease = attachment.lease;
        let token = CancellationToken::new();
        controlled(
            context.control(),
            token.clone(),
            client.write_execution(
                execution_id.clone(),
                lease.clone(),
                bytes,
                args.eof.unwrap_or(false),
                token,
            ),
        )
        .await?;
        let token = CancellationToken::new();
        controlled(
            context.control(),
            token.clone(),
            client.detach_execution(execution_id.clone(), lease, token),
        )
        .await?;
        Ok(json!({
            "tool": PROCESS_WRITE_TOOL_ID,
            "status": "accepted",
            "execution_id": execution_id,
            "eof": args.eof.unwrap_or(false),
        }))
    }

    async fn resize(&self, context: ToolDispatchContext) -> Result<Value> {
        let invocation = context.invocation();
        let args = parse_args::<ResizeArgs>(PROCESS_RESIZE_TOOL_ID, invocation.arguments.clone())?;
        let execution_id = parse_execution_id(&args.execution_id)?;
        self.context.load(&invocation.scope)?;
        let terminal_size = TerminalSize {
            columns: args.columns,
            rows: args.rows,
        };
        terminal_size
            .validate()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let client = self.client(&context)?;
        let token = CancellationToken::new();
        controlled(
            context.control(),
            token.clone(),
            client.resize_execution(execution_id.clone(), terminal_size, token),
        )
        .await?;
        Ok(json!({
            "tool": PROCESS_RESIZE_TOOL_ID,
            "status": "resized",
            "execution_id": execution_id,
            "terminal_size": terminal_size,
        }))
    }

    async fn kill(&self, context: ToolDispatchContext) -> Result<Value> {
        let invocation = context.invocation();
        let args = parse_args::<KillArgs>(PROCESS_KILL_TOOL_ID, invocation.arguments.clone())?;
        let execution_id = parse_execution_id(&args.execution_id)?;
        self.context.load(&invocation.scope)?;
        let mode = args.mode.unwrap_or_default().into();
        let client = self.client(&context)?;
        let token = CancellationToken::new();
        controlled(
            context.control(),
            token.clone(),
            client.terminate_execution(execution_id.clone(), mode, token),
        )
        .await?;
        Ok(json!({
            "tool": PROCESS_KILL_TOOL_ID,
            "status": "termination_requested",
            "execution_id": execution_id,
            "mode": mode,
        }))
    }

    fn argv_request(
        &self,
        context: &ToolDispatchContext,
        args: ArgvArgs,
        io: ExecutionIo,
        foreground: bool,
    ) -> Result<ExecutionRequest> {
        validate_text(
            &args.program,
            "process program",
            false,
            MAX_PROCESS_PATH_BYTES,
        )?;
        validate_argv(&args.args)?;
        let profile = args.profile.unwrap_or_default().into();
        let invocation = context.invocation();
        let (authorization, grant_lease) = execution_authorization(
            context,
            profile,
            false,
            profile == ExecutionProfile::Workspace,
        )?;
        let admission = self.context.load(&invocation.scope)?;
        let cwd = resolve_cwd(
            &admission.snapshot,
            args.cwd.as_deref(),
            profile,
            authorization.host_process_execution,
        )?;
        let environment = self.environment(&admission.snapshot, args.env)?;
        if program_candidate_exists(&args.program, &cwd, &environment.values) == Some(false) {
            return Err(anyhow::Error::new(ToolHandlerError::new(
                "not_found",
                format!("process program was not found: {:?}", args.program),
                json!({"program": args.program}),
            )));
        }
        let resolved_program = resolve_program(&args.program, &cwd, &environment.values)?;
        let stdin = args.stdin.map(ProcessBytes::from);
        if let Some(bytes) = &stdin {
            bytes
                .decode(self.config.max_input_bytes)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        }
        let timeout_ms =
            self.timeout_ms(args.timeout_ms, foreground, context.control().remaining())?;
        Ok(ExecutionRequest {
            owner: admission.owner,
            correlation: execution_correlation(&invocation.scope)?,
            kind: ExecutionKind::Argv,
            program: resolved_program.executable,
            argv0: resolved_program.argv0,
            program_digest: None,
            args: args.args,
            workspace_root: admission.snapshot.workspace_root,
            cwd,
            read_only_roots: self.config.runtime_read_only_roots.clone(),
            environment,
            stdin,
            close_stdin_after_initial: foreground,
            io,
            terminal_size: None,
            profile,
            authorization,
            grant_lease,
            limits: self.execution_limits(timeout_ms),
        })
    }

    fn environment(
        &self,
        snapshot: &ExecutionContextSnapshot,
        overrides: Option<BTreeMap<String, String>>,
    ) -> Result<EnvironmentOverride> {
        let mut values = frozen_base_environment(snapshot, &self.config.base_environment);
        let overrides = overrides.unwrap_or_default();
        ensure!(
            overrides.len() <= MAX_ENVIRONMENT_ENTRIES,
            "process environment exceeds the {MAX_ENVIRONMENT_ENTRIES}-entry limit"
        );
        for (name, value) in overrides {
            validate_environment_pair(&name, &value)?;
            values.insert(name, value);
        }
        let bytes = values.iter().try_fold(0usize, |total, (name, value)| {
            total
                .checked_add(name.len())
                .and_then(|value_total| value_total.checked_add(value.len()))
                .context("process environment byte count overflow")
        })?;
        ensure!(
            bytes <= self.config.maximum_environment_bytes,
            "process environment exceeds the {}-byte limit",
            self.config.maximum_environment_bytes
        );
        let environment = EnvironmentOverride { values };
        environment
            .validate()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(environment)
    }

    fn agent_terminal_environment(
        &self,
        snapshot: &ExecutionContextSnapshot,
        shell: &AdmittedShellProfile,
    ) -> Result<TerminalEnvironmentRequest> {
        let mut admitted_base = frozen_base_environment(snapshot, &self.config.base_environment);
        let inherited_path = admitted_base
            .get("PATH")
            .context("persistent agent terminal requires an admitted PATH")?;
        let roots = canonical_terminal_runtime_roots(&self.config.runtime_read_only_roots)?;
        let path = build_managed_terminal_path(inherited_path, &shell.snapshot.program, &roots)?;
        admitted_base.insert("PATH".to_owned(), path);
        Ok(TerminalEnvironmentRequest {
            admitted_base,
            selected_parent: BTreeMap::new(),
            agl_env: BTreeMap::new(),
            admitted_path_roots: roots,
        })
    }

    fn timeout_ms(
        &self,
        requested: Option<u64>,
        foreground: bool,
        remaining: Option<Duration>,
    ) -> Result<Option<u64>> {
        let maximum = duration_millis(self.config.maximum_foreground_timeout);
        let default = duration_millis(self.config.default_foreground_timeout);
        let mut timeout = match (requested, foreground) {
            (Some(0), _) => bail!("process timeout_ms must be nonzero"),
            (Some(value), _) => Some(value.min(maximum)),
            (None, true) => Some(default),
            (None, false) => None,
        };
        if let Some(remaining) = remaining {
            let remaining = duration_millis(remaining);
            ensure!(
                remaining > 0,
                "durable run deadline elapsed before process spawn"
            );
            timeout = Some(timeout.map_or(remaining, |value| value.min(remaining)));
        }
        Ok(timeout)
    }

    fn execution_limits(&self, timeout_ms: Option<u64>) -> ExecutionLimits {
        ExecutionLimits {
            timeout_ms,
            max_input_bytes: self.config.max_input_bytes as u64,
            max_output_bytes: self.config.max_spool_bytes,
        }
    }

    async fn foreground_result(
        &self,
        client: &TerminalClient<UnixTerminalTransport>,
        context: &ToolDispatchContext,
        tool_id: &str,
        status: ExecutionStatus,
    ) -> Result<Value> {
        let token = CancellationToken::new();
        let output = controlled(
            context.control(),
            token.clone(),
            client.read_execution(
                status.execution_id.clone(),
                ExecutionCursor::default(),
                checked_maximum_bytes(self.config.max_result_bytes)?,
                token,
            ),
        )
        .await?;
        let result_truncated = output.next_sequence < status.last_sequence;
        Ok(json!({
            "tool": tool_id,
            "execution_id": status.execution_id,
            "state": status.state,
            "exit": status.exit,
            "chunks": output.chunks,
            "next_sequence": output.next_sequence,
            "output_truncated": status.output_truncated || result_truncated,
            "output_expired": status.output_expired,
        }))
    }

    fn client(
        &self,
        context: &ToolDispatchContext,
    ) -> Result<TerminalClient<UnixTerminalTransport>> {
        let authority = authority_fingerprint(context)?;
        self.terminal
            .connect(authority)
            .map_err(|error| anyhow::anyhow!(error.to_string()))
    }
}

fn authority_fingerprint(context: &ToolDispatchContext) -> Result<AuthorityFingerprint> {
    AuthorityFingerprint::new(context.invocation().policy_hash.as_str())
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn all_execution_operations() -> BTreeSet<ExecutionOperation> {
    BTreeSet::from([
        ExecutionOperation::Inspect,
        ExecutionOperation::Read,
        ExecutionOperation::Write,
        ExecutionOperation::Resize,
        ExecutionOperation::Interrupt,
        ExecutionOperation::Terminate,
    ])
}

fn all_terminal_operations() -> BTreeSet<TerminalOperation> {
    BTreeSet::from([
        TerminalOperation::Inspect,
        TerminalOperation::Attach,
        TerminalOperation::Read,
        TerminalOperation::Write,
        TerminalOperation::Resize,
        TerminalOperation::Interrupt,
        TerminalOperation::Terminate,
    ])
}

fn checked_maximum_bytes(value: usize) -> Result<u32> {
    u32::try_from(value).context("terminal read byte limit exceeds u32")
}

async fn controlled<T, E, F>(
    control: &ToolDispatchControl,
    cancellation: CancellationToken,
    future: F,
) -> Result<T>
where
    E: std::fmt::Display,
    F: std::future::Future<Output = std::result::Result<T, E>>,
{
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => {
                return result.map_err(|error| anyhow::anyhow!(error.to_string()));
            }
            () = sleep(Duration::from_millis(10)) => {
                if control.is_cancelled() {
                    cancellation.cancel();
                    bail!("tool execution was cancelled");
                }
                if control.is_expired() {
                    cancellation.cancel();
                    bail!("tool execution deadline elapsed");
                }
            }
        }
    }
}

async fn start_execution(
    client: &TerminalClient<UnixTerminalTransport>,
    context: &ToolDispatchContext,
    request: ExecutionRequest,
) -> Result<ExecutionStatus> {
    let mut admission = ExecutionAdmission {
        authority_fingerprint: authority_fingerprint(context)?,
        request_fingerprint: String::new(),
        request,
        operations: all_execution_operations(),
    };
    admission
        .seal_request_fingerprint()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let token = CancellationToken::new();
    controlled(
        context.control(),
        token.clone(),
        client.start_execution(admission, token),
    )
    .await
}

async fn wait_execution(
    client: &TerminalClient<UnixTerminalTransport>,
    context: &ToolDispatchContext,
    mut status: ExecutionStatus,
) -> Result<ExecutionStatus> {
    while !status.state.is_terminal() {
        if context.control().is_cancelled() || context.control().is_expired() {
            let token = CancellationToken::new();
            client
                .terminate_execution(status.execution_id.clone(), KillMode::Graceful, token)
                .await
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            bail!(if context.control().is_cancelled() {
                "tool execution was cancelled"
            } else {
                "tool execution deadline elapsed"
            });
        }
        sleep(Duration::from_millis(10)).await;
        let token = CancellationToken::new();
        status = controlled(
            context.control(),
            token.clone(),
            client.inspect_execution(status.execution_id.clone(), token),
        )
        .await?;
    }
    Ok(status)
}

struct CowWorkspaceTransaction {
    root: PathBuf,
    view_root: PathBuf,
    before: WorkspaceTree,
}

#[derive(Debug)]
struct CowWorkspaceDiff {
    operations: Vec<Value>,
    changed_paths: Vec<String>,
}

#[derive(Debug)]
struct EffectEnvelopeViolation(String);

impl std::fmt::Display for EffectEnvelopeViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for EffectEnvelopeViolation {}

#[derive(Default)]
struct WorkspaceTree {
    files: BTreeMap<PathBuf, WorkspaceFile>,
    symlinks: BTreeMap<PathBuf, PathBuf>,
    total_bytes: usize,
}

struct WorkspaceFile {
    bytes: Vec<u8>,
    mode: u32,
}

impl CowWorkspaceTransaction {
    fn create(source_root: &Path) -> Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("agl-shell-cow-{}-{nonce}", std::process::id()));
        let view_root = root.join("workspace");
        fs::create_dir_all(&view_root).with_context(|| {
            format!(
                "failed to create copy-on-write workspace {}",
                view_root.display()
            )
        })?;
        let result = capture_workspace_tree(source_root, Some(&view_root));
        match result {
            Ok(before) => Ok(Self {
                root,
                view_root,
                before,
            }),
            Err(error) => {
                let _ = fs::remove_dir_all(&root);
                Err(error)
            }
        }
    }

    fn view_root(&self) -> &Path {
        &self.view_root
    }

    fn diff(&self, expected_paths: &[String]) -> Result<CowWorkspaceDiff> {
        let after = capture_workspace_tree(&self.view_root, None)?;
        if self.before.symlinks != after.symlinks {
            return Err(EffectEnvelopeViolation(
                "shell workspace transaction cannot create, remove, or change symbolic links"
                    .to_owned(),
            )
            .into());
        }
        let paths = self
            .before
            .files
            .keys()
            .chain(after.files.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut operations = Vec::new();
        let mut changed_paths = Vec::new();
        for path in paths {
            let before = self.before.files.get(&path);
            let after = after.files.get(&path);
            if before.zip(after).is_some_and(|(before, after)| {
                before.bytes == after.bytes && before.mode == after.mode
            }) {
                continue;
            }
            if before
                .zip(after)
                .is_some_and(|(before, after)| before.mode != after.mode)
            {
                return Err(EffectEnvelopeViolation(format!(
                    "shell workspace transaction cannot change file permissions: {}",
                    path.display()
                ))
                .into());
            }
            let display = path.to_string_lossy().replace('\\', "/");
            if !expected_paths
                .iter()
                .any(|expected| expected_workspace_path_matches(expected, &display))
            {
                return Err(EffectEnvelopeViolation(format!(
                    "shell changed `{display}` outside its expected-effect envelope"
                ))
                .into());
            }
            changed_paths.push(display.clone());
            let operation = match (before, after) {
                (None, Some(after)) => json!({
                    "op": "create",
                    "path": display,
                    "content": std::str::from_utf8(&after.bytes)
                        .context("shell-created files must be UTF-8 for atomic commit")?,
                    "expected_absent": true,
                }),
                (Some(before), None) => json!({
                    "op": "delete",
                    "path": display,
                    "expected_digest": shell_content_digest(&before.bytes),
                }),
                (Some(before), Some(after)) => json!({
                    "op": "update",
                    "path": display,
                    "expected_digest": shell_content_digest(&before.bytes),
                    "edits": [{
                        "old_text": std::str::from_utf8(&before.bytes)
                            .context("shell-updated files must be UTF-8 for atomic commit")?,
                        "new_text": std::str::from_utf8(&after.bytes)
                            .context("shell-updated files must be UTF-8 for atomic commit")?,
                    }],
                }),
                (None, None) => unreachable!("path originated in before or after"),
            };
            operations.push(operation);
        }
        if operations.len() > 64 {
            return Err(EffectEnvelopeViolation(
                "shell workspace transaction changed more than 64 files".to_owned(),
            )
            .into());
        }
        Ok(CowWorkspaceDiff {
            operations,
            changed_paths,
        })
    }
}

impl Drop for CowWorkspaceTransaction {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn capture_workspace_tree(root: &Path, copy_root: Option<&Path>) -> Result<WorkspaceTree> {
    let mut tree = WorkspaceTree::default();
    capture_workspace_directory(root, Path::new(""), copy_root, &mut tree)?;
    Ok(tree)
}

fn capture_workspace_directory(
    root: &Path,
    relative: &Path,
    copy_root: Option<&Path>,
    tree: &mut WorkspaceTree,
) -> Result<()> {
    let source = root.join(relative);
    let mut entries = fs::read_dir(&source)
        .with_context(|| format!("failed to scan shell workspace {}", source.display()))?
        .collect::<io::Result<Vec<_>>>()
        .with_context(|| format!("failed to read shell workspace {}", source.display()))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name();
        if relative.as_os_str().is_empty()
            && (name == ".git" && entry.file_type()?.is_dir() || name == "target")
        {
            continue;
        }
        let path = relative.join(name);
        let metadata = fs::symlink_metadata(entry.path())
            .with_context(|| format!("failed to inspect shell workspace {}", path.display()))?;
        if metadata.file_type().is_dir() {
            if let Some(copy_root) = copy_root {
                fs::create_dir(copy_root.join(&path)).with_context(|| {
                    format!(
                        "failed to copy shell workspace directory {}",
                        path.display()
                    )
                })?;
            }
            capture_workspace_directory(root, &path, copy_root, tree)?;
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(entry.path()).with_context(|| {
                format!("failed to read shell workspace symlink {}", path.display())
            })?;
            tree.symlinks.insert(path.clone(), target.clone());
            if let Some(copy_root) = copy_root {
                create_workspace_symlink(&target, &copy_root.join(path))?;
            }
        } else if metadata.file_type().is_file() {
            let bytes = fs::read(entry.path()).with_context(|| {
                format!("failed to read shell workspace file {}", path.display())
            })?;
            tree.total_bytes = tree
                .total_bytes
                .checked_add(bytes.len())
                .context("shell workspace byte count overflow")?;
            ensure!(
                tree.files.len() < MAX_COW_FILES && tree.total_bytes <= MAX_COW_BYTES,
                "shell workspace exceeds the copy-on-write bound of {MAX_COW_FILES} files or {MAX_COW_BYTES} bytes"
            );
            let mode = workspace_file_mode(&metadata);
            if let Some(copy_root) = copy_root {
                let destination = copy_root.join(&path);
                fs::write(&destination, &bytes).with_context(|| {
                    format!("failed to copy shell workspace file {}", path.display())
                })?;
                set_workspace_file_mode(&destination, mode)?;
            }
            tree.files.insert(path, WorkspaceFile { bytes, mode });
        } else {
            bail!(
                "shell workspace contains unsupported file type: {}",
                path.display()
            );
        }
    }
    Ok(())
}

fn expected_workspace_path_matches(expected: &str, observed: &str) -> bool {
    if let Some(prefix) = expected.strip_suffix('/') {
        observed == prefix
            || observed
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('/'))
    } else {
        observed == expected
    }
}

fn shell_content_digest(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(71);
    value.push_str("sha256:");
    use std::fmt::Write as _;
    for byte in Sha256::digest(bytes) {
        write!(&mut value, "{byte:02x}").expect("writing to a String cannot fail");
    }
    value
}

#[cfg(unix)]
fn workspace_file_mode(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn workspace_file_mode(_metadata: &fs::Metadata) -> u32 {
    0
}

#[cfg(unix)]
fn set_workspace_file_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("failed to preserve permissions for {}", path.display()))
}

#[cfg(not(unix))]
fn set_workspace_file_mode(_path: &Path, _mode: u32) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn create_workspace_symlink(target: &Path, path: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, path)
        .with_context(|| format!("failed to copy shell workspace symlink {}", path.display()))
}

#[cfg(not(unix))]
fn create_workspace_symlink(_target: &Path, path: &Path) -> Result<()> {
    bail!(
        "copy-on-write shell workspace cannot copy symlink {} on this platform",
        path.display()
    )
}

fn frozen_base_environment(
    snapshot: &ExecutionContextSnapshot,
    base: &EnvironmentOverride,
) -> BTreeMap<String, String> {
    let admitted_names = snapshot
        .shell
        .environment_names
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    base.values
        .iter()
        .filter(|(name, _)| admitted_names.contains(name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn terminal_owner(
    admission: &ProcessExecutionAdmission,
) -> Result<(TerminalOwner, agl_ids::RunId)> {
    let caller = admission.owner.caller();
    let root_run_id = RunId::parse(admission.owner.authority_scope().as_str())
        .context("execution authority scope is not an agent run ID")?;
    match caller.owner_kind() {
        CallerOwnerKind::Persistent => {
            let session_id = SessionId::parse(caller.owner_id().as_str())
                .context("persistent execution owner is not an agent session ID")?;
            ensure!(
                session_id == admission.durable_session_id,
                "session process owner differs from its durable terminal session"
            );
            Ok((TerminalOwner::new(caller.clone()), root_run_id))
        }
        CallerOwnerKind::Ephemeral => {
            let run_id = RunId::parse(caller.owner_id().as_str())
                .context("ephemeral execution owner is not an agent run ID")?;
            let _ = run_id;
            Ok((TerminalOwner::new(caller.clone()), root_run_id))
        }
        CallerOwnerKind::Service => bail!("service execution owners do not own agent terminals"),
    }
}

fn execution_correlation(scope: &ExecutionScope) -> Result<ExecutionCorrelation> {
    Ok(ExecutionCorrelation::new(
        CallerNamespace::new("agentlibre", 1)?,
        OpaqueOwnerId::new(scope.run_id().as_str())?,
        OpaqueOwnerId::new(creating_step_id(scope)?.as_str())?,
    ))
}

#[cfg(test)]
fn agent_run_owner(run_id: &RunId, root_run_id: &RunId) -> Result<ExecutionOwner> {
    Ok(ExecutionOwner::new(
        CallerOwner::new(
            CallerNamespace::new("agentlibre", 1)?,
            OpaqueOwnerId::new(run_id.as_str())?,
            CallerOwnerKind::Ephemeral,
            CallerRole::Agent,
        ),
        OpaqueOwnerId::new(root_run_id.as_str())?,
    ))
}

fn admitted_agent_shell(snapshot: &ExecutionContextSnapshot) -> Result<AdmittedShellProfile> {
    let executable = snapshot
        .shell
        .program
        .file_name()
        .and_then(|name| name.to_str())
        .context("persistent agent terminal shell has no UTF-8 executable basename")?;
    let kind = match executable {
        "bash" => AdmittedShellKind::Bash,
        "zsh" => AdmittedShellKind::Zsh,
        _ => bail!("persistent agent terminal supports only admitted Bash or Zsh on Linux"),
    };
    let shell = AdmittedShellProfile {
        kind,
        snapshot: snapshot.shell.clone(),
    };
    shell
        .validate()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(shell)
}

fn canonical_terminal_runtime_roots(configured: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut roots =
        agl_pty::standard_runtime_roots().map_err(|error| anyhow::anyhow!(error.to_string()))?;
    for root in configured {
        let canonical = root.canonicalize().with_context(|| {
            format!(
                "failed to canonicalize terminal runtime root {}",
                root.display()
            )
        })?;
        ensure!(
            canonical == *root && canonical.is_dir(),
            "terminal runtime roots must be existing canonical directories"
        );
        roots.push(canonical);
    }
    roots.sort();
    roots.dedup();
    ensure!(
        !roots.is_empty(),
        "persistent terminal has no admitted Linux runtime roots"
    );
    Ok(roots)
}

fn build_managed_terminal_path(
    inherited_path: &str,
    shell_program: &Path,
    admitted_roots: &[PathBuf],
) -> Result<String> {
    ensure!(
        admitted_roots
            .iter()
            .any(|root| shell_program.starts_with(root)),
        "configured shell is outside admitted terminal runtime roots"
    );
    let mut seen = BTreeSet::new();
    let mut paths = Vec::new();
    let mut admit = |candidate: PathBuf| {
        if candidate.is_dir()
            && admitted_roots
                .iter()
                .any(|root| candidate.starts_with(root))
            && seen.insert(candidate.clone())
        {
            paths.push(candidate);
        }
    };
    for candidate in std::env::split_paths(inherited_path) {
        if let Ok(canonical) = candidate.canonicalize() {
            admit(canonical);
        }
    }
    if let Some(parent) = shell_program.parent() {
        admit(parent.to_path_buf());
    }
    for root in admitted_roots {
        if root.file_name().and_then(|name| name.to_str()) == Some("bin") {
            admit(root.clone());
        }
        if let Ok(bin) = root.join("bin").canonicalize() {
            admit(bin);
        }
    }
    ensure!(
        paths.iter().any(|path| path.join("ls").is_file()),
        "admitted terminal PATH does not provide the required `ls` utility"
    );
    std::env::join_paths(paths)
        .context("failed to join admitted terminal PATH")?
        .into_string()
        .map_err(|_| anyhow::anyhow!("admitted terminal PATH is not valid UTF-8"))
}

impl ToolHandler for ProcessTools {
    fn preflight(
        &self,
        invocation: &ToolInvocation,
    ) -> std::result::Result<BTreeSet<EffectId>, ToolHandlerError> {
        Ok(requested_conditional_effects(invocation)?)
    }

    fn dispatch(&self, context: ToolDispatchContext) -> agl_kernel::ToolHandlerFuture<'_> {
        let runtime = match process_io_runtime() {
            Ok(runtime) => runtime,
            Err(error) => return Box::pin(async move { Err(error.into()) }),
        };
        let tool_id = context.invocation().tool_id.as_str().to_owned();
        let conditional_effects = context.authorized_conditional_effects().clone();
        let tools = self.clone();
        let task = runtime.spawn(async move { tools.execute(context).await });
        Box::pin(async move {
            let result = task.await.map_err(|error| {
                ToolHandlerError::execution_failed(format!(
                    "process Tool runtime task failed: {error}"
                ))
            })?;
            match result {
                Ok(data) => {
                    let observed = observed_process_effects(&tool_id, &conditional_effects, &data);
                    Ok(ToolResult::new(data).with_observed_effects(observed))
                }
                Err(error) => match error.downcast::<ToolHandlerError>() {
                    Ok(error) => Err(error),
                    Err(error) => Err(error.into()),
                },
            }
        })
    }
}

fn process_io_runtime() -> Result<&'static tokio::runtime::Runtime> {
    static RUNTIME: OnceLock<std::result::Result<tokio::runtime::Runtime, String>> =
        OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("agl-process-tool-io")
                .build()
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| anyhow::anyhow!("failed to start process Tool I/O runtime: {error}"))
}

fn observed_process_effects(
    tool_id: &str,
    conditional_effects: &BTreeSet<EffectId>,
    data: &Value,
) -> Vec<ObservedEffect> {
    let mut observed = Vec::new();
    let execution_id = data
        .get("execution_id")
        .and_then(Value::as_str)
        .unwrap_or("not_returned")
        .to_owned();
    let tool_scope = || {
        [
            ("tool".to_owned(), tool_id.to_owned()),
            ("execution_id".to_owned(), execution_id.clone()),
        ]
    };
    match tool_id {
        PROCESS_CD_TOOL_ID => observed.push(ObservedEffect::new(
            EffectId::session_working_directory(),
            [(
                "working_directory".to_owned(),
                data["working_directory"]
                    .as_str()
                    .unwrap_or("not_returned")
                    .to_owned(),
            )],
        )),
        PROCESS_EXEC_TOOL_ID | PROCESS_START_TOOL_ID | SHELL_EXEC_TOOL_ID => {
            observed.push(ObservedEffect::new(EffectId::spawn_process(), tool_scope()))
        }
        PROCESS_WRITE_TOOL_ID | PROCESS_RESIZE_TOOL_ID | PROCESS_KILL_TOOL_ID => observed.push(
            ObservedEffect::new(EffectId::control_process(), tool_scope()),
        ),
        _ => {}
    }
    for effect in conditional_effects {
        if effect == &EffectId::repo_workspace() {
            let paths = data["observed_effects"]["workspace_paths"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>();
            if paths.is_empty() {
                observed.push(ObservedEffect::new(
                    effect.clone(),
                    [
                        ("tool".to_owned(), tool_id.to_owned()),
                        ("workspace_change".to_owned(), "none".to_owned()),
                    ],
                ));
            } else {
                observed.extend(paths.into_iter().map(|path| {
                    ObservedEffect::new(
                        effect.clone(),
                        [
                            ("tool".to_owned(), tool_id.to_owned()),
                            ("path".to_owned(), path.to_owned()),
                        ],
                    )
                }));
            }
        } else {
            observed.push(ObservedEffect::new(effect.clone(), tool_scope()));
        }
    }
    observed
}

pub fn declaration() -> ExtensionDescriptor {
    let descriptor = ExtensionDescriptor::builtin(
        ExtensionId::new(EXTENSION_ID).expect("process extension id is valid"),
        "Core Process",
        "1.2.0",
    )
    .expect("process extension declaration is valid")
    .with_tool(action::<EmptyArgs, PwdOutput>(
        PROCESS_PWD_TOOL_ID,
        "Return the caller's durable logical working directory.",
        OperationKind::Read,
        &[],
        &[],
    ))
    .with_tool(action::<CdArgs, CdOutput>(
        PROCESS_CD_TOOL_ID,
        "Change the caller's durable logical working directory.",
        OperationKind::Write,
        &[EffectId::session_working_directory()],
        &[EffectId::host_process_execution()],
    ))
    .with_tool(
        action::<ArgvArgs, ForegroundOutput>(
            PROCESS_EXEC_TOOL_ID,
            "Run an exact argv with pipes and wait for its bounded result.",
            OperationKind::Execute,
            &[EffectId::spawn_process()],
            &[EffectId::host_process_execution()],
        )
        .with_errors(process_launch_errors())
        .expect("foreground process error declarations are valid"),
    )
    .with_tool(
        action::<StartArgs, BackgroundOutput>(
            PROCESS_START_TOOL_ID,
            "Start an exact argv with pipes or a PTY and return its execution handle.",
            OperationKind::Execute,
            &[EffectId::spawn_process()],
            &[EffectId::host_process_execution()],
        )
        .with_errors(process_launch_errors())
        .expect("background process error declarations are valid"),
    )
    .with_tool(action::<ExecutionIdArgs, StatusOutput>(
        PROCESS_STATUS_TOOL_ID,
        "Return safe lifecycle and retention status for an owned execution.",
        OperationKind::Read,
        &[],
        &[],
    ))
    .with_tool(action::<ReadOutputArgs, ReadOutput>(
        PROCESS_READ_TOOL_ID,
        "Read bounded ordered output chunks from an owned execution.",
        OperationKind::Read,
        &[],
        &[],
    ))
    .with_tool(action::<WriteArgs, WriteOutput>(
        PROCESS_WRITE_TOOL_ID,
        "Write bounded bytes or EOF to an owned live execution.",
        OperationKind::Execute,
        &[EffectId::control_process()],
        &[],
    ))
    .with_tool(action::<ResizeArgs, ResizeOutput>(
        PROCESS_RESIZE_TOOL_ID,
        "Resize an owned live PTY.",
        OperationKind::Execute,
        &[EffectId::control_process()],
        &[],
    ))
    .with_tool(action::<KillArgs, KillOutput>(
        PROCESS_KILL_TOOL_ID,
        "Request graceful or immediate termination of an owned process tree.",
        OperationKind::Execute,
        &[EffectId::control_process()],
        &[],
    ))
    .with_tool(
        action::<ShellArgs, ShellOutput>(
            SHELL_EXEC_TOOL_ID,
            "Run one explicitly degraded shell fallback inside the admitted effect envelope.",
            OperationKind::Execute,
            &[EffectId::spawn_process()],
            &[
                EffectId::host_process_execution(),
                EffectId::shell_login_startup(),
                EffectId::repo_workspace(),
            ],
        )
        .with_errors([
            agl_kernel::ToolErrorDeclaration::recoverable("invalid_effect_envelope")
                .with_data_schema::<EmptyToolErrorData>(),
            agl_kernel::ToolErrorDeclaration::recoverable("invalid_patch")
                .with_data_schema::<EmptyToolErrorData>(),
            agl_kernel::ToolErrorDeclaration::recoverable("not_found")
                .with_data_schema::<PathNotFoundErrorData>(),
            agl_kernel::ToolErrorDeclaration::recoverable("conflict")
                .with_data_schema::<PatchConflictErrorData>(),
            agl_kernel::ToolErrorDeclaration::terminal("execution_failed")
                .with_data_schema::<EmptyToolErrorData>(),
            agl_kernel::ToolErrorDeclaration::terminal("outcome_unknown")
                .with_data_schema::<EmptyToolErrorData>(),
        ])
        .expect("shell error declarations are valid"),
    );
    crate::with_observation_workflow(descriptor.with_effects([
        EffectDeclaration::for_standard(EffectId::session_working_directory()).unwrap(),
        EffectDeclaration::for_standard(EffectId::spawn_process()).unwrap(),
        EffectDeclaration::for_standard(EffectId::control_process()).unwrap(),
        EffectDeclaration::for_standard(EffectId::host_process_execution()).unwrap(),
        EffectDeclaration::for_standard(EffectId::shell_login_startup()).unwrap(),
        EffectDeclaration::for_standard(EffectId::repo_workspace()).unwrap(),
    ]))
}

fn action<I: JsonSchema, O: JsonSchema>(
    id: &str,
    description: &str,
    operation: OperationKind,
    effects: &[EffectId],
    conditional_effects: &[EffectId],
) -> ToolDeclaration {
    ToolDeclaration::from_schema::<I>(
        ToolId::new(id).expect("process tool id is valid"),
        description,
        operation,
    )
    .expect("process action schema is valid")
    .with_output_schema::<O>()
    .expect("process result schema is valid")
    .with_errors([
        agl_kernel::ToolErrorDeclaration::terminal("execution_failed")
            .with_data_schema::<EmptyToolErrorData>(),
    ])
    .expect("process error schema is valid")
    .with_state_effects(effects.iter().cloned())
    .with_conditional_state_effects(conditional_effects.iter().cloned())
}

fn process_launch_errors() -> Vec<agl_kernel::ToolErrorDeclaration> {
    vec![
        agl_kernel::ToolErrorDeclaration::recoverable("not_found")
            .with_data_schema::<ProgramNotFoundErrorData>(),
        agl_kernel::ToolErrorDeclaration::terminal("execution_failed")
            .with_data_schema::<EmptyToolErrorData>(),
    ]
}

fn requested_conditional_effects(invocation: &ToolInvocation) -> Result<BTreeSet<EffectId>> {
    let mut effects = BTreeSet::new();
    match invocation.tool_id.as_str() {
        PROCESS_CD_TOOL_ID => {
            let args = parse_args::<CdArgs>(PROCESS_CD_TOOL_ID, invocation.arguments.clone())?;
            if args.profile.unwrap_or_default() == ProfileArg::Host {
                effects.insert(EffectId::host_process_execution());
            }
        }
        PROCESS_EXEC_TOOL_ID => {
            let args = parse_args::<ArgvArgs>(PROCESS_EXEC_TOOL_ID, invocation.arguments.clone())?;
            if args.profile.unwrap_or_default() == ProfileArg::Host {
                effects.insert(EffectId::host_process_execution());
            }
        }
        PROCESS_START_TOOL_ID => {
            let args =
                parse_args::<StartArgs>(PROCESS_START_TOOL_ID, invocation.arguments.clone())?;
            if args.argv.profile.unwrap_or_default() == ProfileArg::Host {
                effects.insert(EffectId::host_process_execution());
            }
        }
        SHELL_EXEC_TOOL_ID => {
            let args = parse_args::<ShellArgs>(SHELL_EXEC_TOOL_ID, invocation.arguments.clone())?;
            if args.profile.unwrap_or_default() == ProfileArg::Host || args.login.unwrap_or(false) {
                effects.insert(EffectId::host_process_execution());
            }
            if args.login.unwrap_or(false) {
                effects.insert(EffectId::shell_login_startup());
            }
            if args.workspace_access == WorkspaceAccessArg::Write {
                effects.insert(EffectId::repo_workspace());
            }
        }
        _ => {}
    }
    Ok(effects)
}

fn execution_authorization(
    context: &ToolDispatchContext,
    profile: ExecutionProfile,
    login: bool,
    workspace_write: bool,
) -> Result<(ExecutionAuthorization, Option<ExecutionGrantLease>)> {
    let effects = context.authorized_conditional_effects();
    let host = effects.contains(&EffectId::host_process_execution());
    let login_authorized = effects.contains(&EffectId::shell_login_startup());
    ensure!(
        profile != ExecutionProfile::Host || host,
        "host process execution was not admitted"
    );
    ensure!(
        !login || login_authorized,
        "shell login startup was not admitted"
    );
    let grant_lease = if profile == ExecutionProfile::Host {
        let provenance = context
            .grant_provenance()
            .context("host process execution lacks immutable grant provenance")?;
        Some(ExecutionGrantLease {
            origin: ExecutionLeaseOrigin::ToolGrant,
            grant_id: provenance.grant_id.clone(),
            duration: provenance.duration.clone(),
            scope_digest: provenance.scope_digest.clone(),
        })
    } else {
        None
    };
    Ok((
        ExecutionAuthorization {
            host_process_execution: host,
            shell_login_startup: login_authorized,
            workspace_write,
        },
        grant_lease,
    ))
}

fn resolve_cwd(
    snapshot: &ExecutionContextSnapshot,
    requested: Option<&str>,
    profile: ExecutionProfile,
    host_authorized: bool,
) -> Result<PathBuf> {
    let requested = requested.unwrap_or(".");
    validate_text(requested, "process cwd", false, MAX_PROCESS_PATH_BYTES)?;
    resolve_execution_directory(snapshot, Path::new(requested), profile, host_authorized)
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

struct ResolvedProgram {
    executable: PathBuf,
    argv0: String,
}

fn resolve_program(
    program: &str,
    cwd: &Path,
    environment: &BTreeMap<String, String>,
) -> Result<ResolvedProgram> {
    let requested = Path::new(program);
    let resolved = if requested.is_absolute() {
        requested.canonicalize()
    } else if requested.components().count() > 1 {
        cwd.join(requested).canonicalize()
    } else {
        let path = environment
            .get("PATH")
            .context("relative process program requires admitted PATH")?;
        let mut found = None;
        for root in std::env::split_paths(path) {
            let candidate = root.join(requested);
            if candidate.is_file() {
                found = Some(candidate.canonicalize());
                break;
            }
        }
        found.context("process program was not found in admitted PATH")?
    }
    .with_context(|| format!("failed to resolve process program {program:?}"))?;
    let metadata = std::fs::metadata(&resolved)
        .with_context(|| format!("failed to inspect process program {}", resolved.display()))?;
    ensure!(metadata.is_file(), "process program is not a regular file");
    ensure!(
        is_executable(&metadata),
        "process program is not executable"
    );
    Ok(ResolvedProgram {
        executable: resolved,
        argv0: program.to_owned(),
    })
}

fn program_candidate_exists(
    program: &str,
    cwd: &Path,
    environment: &BTreeMap<String, String>,
) -> Option<bool> {
    let requested = Path::new(program);
    if requested.is_absolute() {
        return Some(requested.is_file());
    }
    if requested.components().count() > 1 {
        return Some(cwd.join(requested).is_file());
    }
    environment.get("PATH").map(|path| {
        std::env::split_paths(path)
            .map(|root| root.join(requested))
            .any(|candidate| candidate.is_file())
    })
}

fn terminal_size(
    requested: Option<TerminalSizeArgs>,
    io: ExecutionIo,
    default: TerminalSize,
) -> Result<Option<TerminalSize>> {
    match (io, requested) {
        (ExecutionIo::Pipes, Some(_)) => bail!("terminal_size is valid only when io=pty"),
        (ExecutionIo::Pipes, None) => Ok(None),
        (ExecutionIo::Pty, Some(size)) => {
            let size: TerminalSize = size.into();
            size.validate()
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            Ok(Some(size))
        }
        (ExecutionIo::Pty, None) => Ok(Some(default)),
    }
}

fn background_result(tool_id: &str, status: &ExecutionStatus) -> Value {
    json!({
        "tool": tool_id,
        "execution_id": status.execution_id,
        "state": status.state,
        "cwd": status.cwd,
        "io": status.io,
        "profile": status.profile,
        "terminal_size": status.terminal_size,
    })
}

fn parse_execution_id(value: &str) -> Result<ExecutionId> {
    ExecutionId::parse(value).with_context(|| format!("invalid execution_id {value:?}"))
}

fn creating_step_id(scope: &ExecutionScope) -> Result<StepId> {
    scope
        .step_id()
        .cloned()
        .context("process start requires an admitted durable step ID")
}

fn validate_text(value: &str, label: &str, allow_empty: bool, maximum_bytes: usize) -> Result<()> {
    ensure!(allow_empty || !value.is_empty(), "{label} cannot be empty");
    ensure!(!value.contains('\0'), "{label} contains NUL");
    ensure!(
        value.len() <= maximum_bytes,
        "{label} exceeds the {maximum_bytes}-byte limit"
    );
    Ok(())
}

fn validate_shell_contract(args: &ShellArgs) -> Result<()> {
    validate_text(&args.semantic_intent, "shell semantic_intent", false, 4_096)?;
    validate_text(&args.fallback_reason, "shell fallback_reason", false, 4_096)?;
    ensure!(
        args.expected_effects.process,
        "shell expected_effects.process must be true"
    );
    ensure!(
        !args.background.unwrap_or(false),
        "shell fallback is foreground-only"
    );
    for effect in &args.expected_effects.external {
        validate_text(effect, "shell expected external effect", false, 256)?;
    }
    for path in &args.expected_effects.workspace_paths {
        validate_text(path, "shell expected workspace path", false, 4_096)?;
        ensure!(
            !path.contains('\\') && !path.contains('\0'),
            "shell expected workspace paths must use forward slashes without NUL"
        );
        let path = Path::new(path.trim_end_matches('/'));
        ensure!(
            !path.as_os_str().is_empty() && !path.is_absolute(),
            "shell expected workspace paths must be nonempty repository-relative paths"
        );
        ensure!(
            path.components().all(|component| matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )),
            "shell expected workspace paths cannot contain traversal"
        );
        ensure!(
            !matches!(
                path.components().next(),
                Some(std::path::Component::Normal(component)) if component == ".git"
            ),
            "shell expected workspace paths cannot enter .git"
        );
    }

    let profile = args.profile.unwrap_or_default();
    match (profile, args.workspace_access) {
        (ProfileArg::Workspace, WorkspaceAccessArg::ReadOnly) => {
            ensure!(
                args.expected_effects.workspace_paths.is_empty(),
                "read-only shell fallback cannot expect workspace mutations"
            );
            ensure!(
                args.expected_effects.external.is_empty(),
                "workspace shell fallback cannot expect external effects"
            );
        }
        (ProfileArg::Workspace, WorkspaceAccessArg::Write) => {
            ensure!(
                !args.expected_effects.workspace_paths.is_empty(),
                "writable shell fallback requires expected workspace paths"
            );
            ensure!(
                args.expected_effects.external.is_empty(),
                "workspace shell fallback cannot expect external effects"
            );
        }
        (ProfileArg::Host, WorkspaceAccessArg::ReadOnly) => {
            ensure!(
                args.expected_effects.workspace_paths.is_empty(),
                "host shell fallback cannot expect workspace mutations"
            );
            ensure!(
                args.expected_effects
                    .external
                    .iter()
                    .any(|effect| effect == "host_process"),
                "host shell fallback must expect the `host_process` effect"
            );
        }
        (ProfileArg::Host, WorkspaceAccessArg::Write) => {
            bail!("host shell fallback cannot request workspace writes")
        }
    }
    Ok(())
}

fn validate_argv(values: &[String]) -> Result<()> {
    ensure!(
        values.len() <= MAX_PROCESS_ARGUMENTS,
        "process argv exceeds the {MAX_PROCESS_ARGUMENTS}-argument limit"
    );
    ensure!(
        values.iter().all(|value| !value.contains('\0')),
        "process arguments contain NUL"
    );
    let bytes = values.iter().try_fold(0usize, |total, value| {
        total
            .checked_add(value.len())
            .context("process argv byte count overflow")
    })?;
    ensure!(
        bytes <= MAX_PROCESS_TEXT_BYTES,
        "process argv exceeds the {MAX_PROCESS_TEXT_BYTES}-byte limit"
    );
    Ok(())
}

fn validate_environment_pair(name: &str, value: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && !name.contains(['=', '\0']) && !value.contains('\0'),
        "process environment override is invalid"
    );
    ensure!(
        name.len() <= MAX_ENVIRONMENT_NAME_BYTES && value.len() <= MAX_PROCESS_TEXT_BYTES,
        "process environment override exceeds its field limit"
    );
    Ok(())
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(unix)]
fn is_executable(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &std::fs::Metadata) -> bool {
    true
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ProfileArg {
    #[default]
    Workspace,
    Host,
}

impl From<ProfileArg> for ExecutionProfile {
    fn from(value: ProfileArg) -> Self {
        match value {
            ProfileArg::Workspace => Self::Workspace,
            ProfileArg::Host => Self::Host,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum IoArg {
    Pipes,
    Pty,
}

impl From<IoArg> for ExecutionIo {
    fn from(value: IoArg) -> Self {
        match value {
            IoArg::Pipes => Self::Pipes,
            IoArg::Pty => Self::Pty,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum KillModeArg {
    #[default]
    Graceful,
    Immediate,
}

impl From<KillModeArg> for KillMode {
    fn from(value: KillModeArg) -> Self {
        match value {
            KillModeArg::Graceful => Self::Graceful,
            KillModeArg::Immediate => Self::Immediate,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum BytesEncodingArg {
    Utf8,
    Base64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct BytesArg {
    encoding: BytesEncodingArg,
    #[schemars(length(max = 87384))]
    data: String,
}

impl From<BytesArg> for ProcessBytes {
    fn from(value: BytesArg) -> Self {
        Self {
            encoding: match value.encoding {
                BytesEncodingArg::Utf8 => ProcessBytesEncoding::Utf8,
                BytesEncodingArg::Base64 => ProcessBytesEncoding::Base64,
            },
            data: value.data,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct TerminalSizeArgs {
    #[schemars(range(min = 1))]
    columns: u16,
    #[schemars(range(min = 1))]
    rows: u16,
}

impl From<TerminalSizeArgs> for TerminalSize {
    fn from(value: TerminalSizeArgs) -> Self {
        Self {
            columns: value.columns,
            rows: value.rows,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CdArgs {
    #[schemars(length(min = 1, max = 4096))]
    path: String,
    profile: Option<ProfileArg>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ArgvArgs {
    #[schemars(length(min = 1, max = 4096))]
    program: String,
    #[serde(default)]
    #[schemars(length(max = 1024))]
    args: Vec<String>,
    #[schemars(length(max = 4096))]
    cwd: Option<String>,
    #[schemars(length(max = 256))]
    env: Option<BTreeMap<String, String>>,
    stdin: Option<BytesArg>,
    #[schemars(range(min = 1))]
    timeout_ms: Option<u64>,
    profile: Option<ProfileArg>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct StartArgs {
    #[serde(flatten)]
    argv: ArgvArgs,
    io: IoArg,
    terminal_size: Option<TerminalSizeArgs>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ShellArgs {
    #[schemars(length(max = 65536))]
    command: String,
    #[schemars(length(min = 1, max = 4096))]
    semantic_intent: String,
    workspace_access: WorkspaceAccessArg,
    expected_effects: ShellExpectedEffects,
    #[schemars(length(min = 1, max = 4096))]
    fallback_reason: String,
    #[schemars(length(max = 4096))]
    cwd: Option<String>,
    #[schemars(length(max = 256))]
    env: Option<BTreeMap<String, String>>,
    #[schemars(range(min = 1))]
    timeout_ms: Option<u64>,
    background: Option<bool>,
    terminal_size: Option<TerminalSizeArgs>,
    profile: Option<ProfileArg>,
    login: Option<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "snake_case")]
enum WorkspaceAccessArg {
    ReadOnly,
    Write,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
struct ShellExpectedEffects {
    process: bool,
    #[serde(default)]
    #[schemars(length(max = 64))]
    workspace_paths: Vec<String>,
    #[serde(default)]
    #[schemars(length(max = 16))]
    external: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExecutionIdArgs {
    #[schemars(length(min = 41, max = 41))]
    execution_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadOutputArgs {
    #[schemars(length(min = 41, max = 41))]
    execution_id: String,
    after_sequence: Option<u64>,
    #[schemars(range(min = 1, max = 65536))]
    max_bytes: usize,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    #[schemars(length(min = 41, max = 41))]
    execution_id: String,
    bytes: BytesArg,
    eof: Option<bool>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ResizeArgs {
    #[schemars(length(min = 41, max = 41))]
    execution_id: String,
    #[schemars(range(min = 1))]
    columns: u16,
    #[schemars(range(min = 1))]
    rows: u16,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct KillArgs {
    #[schemars(length(min = 41, max = 41))]
    execution_id: String,
    mode: Option<KillModeArg>,
}

#[derive(JsonSchema)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
enum ExecutionStateOutput {
    Admitting,
    Starting,
    Running,
    Exited,
    Signalled,
    Cancelled,
    TimedOut,
    Failed,
    OutcomeUnknown,
}

#[derive(JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind")]
#[allow(dead_code)]
enum ExecutionExitOutput {
    Code { code: i32 },
    Signal { signal: i32 },
    Error { code: String },
}

#[derive(JsonSchema)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
enum ExecutionChannelOutput {
    Stdout,
    Stderr,
    Terminal,
    Lifecycle,
}

#[derive(JsonSchema)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
enum ExecutionOwnerOutput {
    Session {
        session_id: String,
        root_run_id: String,
    },
    Run {
        run_id: String,
        root_run_id: String,
    },
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct ProcessBytesOutput {
    encoding: BytesEncodingArg,
    data: String,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct OutputChunk {
    sequence: u64,
    channel: ExecutionChannelOutput,
    bytes: ProcessBytesOutput,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct EmptyToolErrorData {}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct ProgramNotFoundErrorData {
    program: String,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct PathNotFoundErrorData {
    path: String,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct PatchConflictErrorData {
    path: String,
    expected_digest: Option<String>,
    actual_digest: Option<String>,
    expected_absent: Option<bool>,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct PwdOutput {
    tool: String,
    status: String,
    working_directory: String,
    workspace_root: String,
    revision: u64,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct CdOutput {
    tool: String,
    status: String,
    working_directory: String,
    profile: ProfileArg,
    revision: u64,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct ForegroundOutput {
    tool: String,
    execution_id: String,
    state: ExecutionStateOutput,
    exit: Option<ExecutionExitOutput>,
    chunks: Vec<OutputChunk>,
    next_sequence: u64,
    output_truncated: bool,
    output_expired: bool,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct BackgroundOutput {
    tool: String,
    execution_id: String,
    state: ExecutionStateOutput,
    cwd: String,
    io: IoArg,
    profile: ProfileArg,
    terminal_size: Option<TerminalSizeArgs>,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct FullExecutionStatusOutput {
    execution_id: String,
    owner: ExecutionOwnerOutput,
    state: ExecutionStateOutput,
    profile: ProfileArg,
    io: IoArg,
    cwd: String,
    terminal_size: Option<TerminalSizeArgs>,
    exit: Option<ExecutionExitOutput>,
    first_retained_sequence: Option<u64>,
    last_sequence: u64,
    retained_bytes: u64,
    discarded_output_bytes: u64,
    output_truncated: bool,
    output_expired: bool,
    started_at_unix_ms: Option<i64>,
    finished_at_unix_ms: Option<i64>,
    error_code: Option<String>,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct StatusOutput {
    tool: String,
    status: FullExecutionStatusOutput,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct ExecutionReadOutput {
    execution_id: String,
    chunks: Vec<OutputChunk>,
    next_sequence: u64,
    state: ExecutionStateOutput,
    output_truncated: bool,
    output_expired: bool,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct ReadOutput {
    tool: String,
    output: ExecutionReadOutput,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct WriteOutput {
    tool: String,
    status: String,
    execution_id: String,
    eof: bool,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct ResizeOutput {
    tool: String,
    status: String,
    execution_id: String,
    terminal_size: TerminalSizeArgs,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct KillOutput {
    tool: String,
    status: String,
    execution_id: String,
    mode: KillModeArg,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct ObservedEffectsOutput {
    process: bool,
    workspace_paths: Vec<String>,
    external: Vec<String>,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct PatchReceiptOutput {
    operation: String,
    path: Option<String>,
    from: Option<String>,
    to: Option<String>,
    before_digest: Option<String>,
    after_digest: Option<String>,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct PatchOutput {
    tool: String,
    status: String,
    change_count: usize,
    changes: Vec<PatchReceiptOutput>,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct WorkspaceTransactionOutput {
    state: String,
    patch_receipt: Option<PatchOutput>,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct ManagedShellOutput {
    tool: String,
    guarantee_class: String,
    semantic_intent: String,
    fallback_reason: String,
    requested_workspace_access: WorkspaceAccessArg,
    expected_effects: ShellExpectedEffects,
    observed_effects: ObservedEffectsOutput,
    terminal_id: String,
    execution_id: String,
    command_sequence: u64,
    cwd: String,
    exit: ExecutionExitOutput,
    after_sequence: u64,
    through_sequence: u64,
    chunks: Vec<OutputChunk>,
    next_sequence: u64,
    output_truncated: bool,
    output_expired: bool,
}

#[derive(JsonSchema)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct OneShotShellOutput {
    tool: String,
    execution_id: String,
    state: ExecutionStateOutput,
    exit: Option<ExecutionExitOutput>,
    chunks: Vec<OutputChunk>,
    next_sequence: u64,
    output_truncated: bool,
    output_expired: bool,
    guarantee_class: String,
    semantic_intent: String,
    fallback_reason: String,
    requested_workspace_access: WorkspaceAccessArg,
    expected_effects: ShellExpectedEffects,
    observed_effects: ObservedEffectsOutput,
    workspace_transaction: Option<WorkspaceTransactionOutput>,
}

#[derive(JsonSchema)]
#[serde(untagged)]
#[allow(dead_code)]
enum ShellOutput {
    Managed(ManagedShellOutput),
    OneShot(OneShotShellOutput),
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use agl_ids::{RunId, StepId};
    use agl_kernel::{DispatchDenialCode, ToolAccessMode, ToolOutcomeStatus, ToolPolicyInput};
    use agl_kernel::{ExtensionRegistration, ToolBinding, ToolDispatchControl};
    use agl_terminal_protocol::{TERMINAL_PROTOCOL_VERSION, TerminalGenerationIdentity};

    use super::*;

    struct TestExecutionContext {
        snapshot: Mutex<ExecutionContextSnapshot>,
        owner: ExecutionOwner,
        durable_session_id: SessionId,
    }

    impl ProcessExecutionContext for TestExecutionContext {
        fn load(&self, _scope: &ExecutionScope) -> Result<ProcessExecutionAdmission> {
            Ok(ProcessExecutionAdmission {
                snapshot: self.snapshot.lock().unwrap().clone(),
                owner: self.owner.clone(),
                durable_session_id: self.durable_session_id.clone(),
            })
        }

        fn compare_and_set_working_directory(
            &self,
            _scope: &ExecutionScope,
            expected_revision: u64,
            next: ExecutionContextSnapshot,
        ) -> Result<ProcessExecutionAdmission> {
            let mut snapshot = self.snapshot.lock().unwrap();
            ensure!(snapshot.revision == expected_revision, "test CAS lost");
            *snapshot = next.clone();
            Ok(ProcessExecutionAdmission {
                snapshot: next,
                owner: self.owner.clone(),
                durable_session_id: self.durable_session_id.clone(),
            })
        }
    }

    #[test]
    fn declaration_has_exact_extension_actions_and_effect_classes() {
        let declaration = declaration();
        assert_eq!(declaration.id.as_str(), EXTENSION_ID);
        assert_eq!(
            declaration
                .tools
                .iter()
                .map(|action| action.id.as_str())
                .collect::<BTreeSet<_>>(),
            PROCESS_TOOL_IDS.iter().copied().collect()
        );
        let shell = declaration
            .tools
            .iter()
            .find(|action| action.id.as_str() == SHELL_EXEC_TOOL_ID)
            .unwrap();
        assert_eq!(shell.operation_kind, OperationKind::Execute);
        assert_eq!(shell.state_effects, [EffectId::spawn_process()].into());
        assert_eq!(
            shell.conditional_state_effects,
            [
                EffectId::host_process_execution(),
                EffectId::shell_login_startup(),
                EffectId::repo_workspace(),
            ]
            .into()
        );
    }

    #[test]
    fn conditional_preflight_is_exact_and_login_implies_host() {
        let extension = declaration();
        let shell = extension
            .tools
            .iter()
            .find(|action| action.id.as_str() == SHELL_EXEC_TOOL_ID)
            .unwrap();
        let invocation = ToolInvocation::new(
            ExecutionScope::builder(agl_ids::RunId::generate())
                .build()
                .unwrap(),
            shell.id.clone(),
            extension.id.clone(),
            shell.digest(),
            agl_kernel::PolicyHash::parse(&format!("sha256:{}", "0".repeat(64))).unwrap(),
            json!({
                "command": "true",
                "semantic_intent": "verify the host shell contract",
                "workspace_access": "read_only",
                "expected_effects": {
                    "process": true,
                    "workspace_paths": [],
                    "external": ["host_process"]
                },
                "fallback_reason": "the operation intentionally exercises shell startup",
                "profile": "host",
                "login": true
            }),
        );
        assert_eq!(
            requested_conditional_effects(&invocation).unwrap(),
            [
                EffectId::host_process_execution(),
                EffectId::shell_login_startup(),
            ]
            .into()
        );
    }

    #[test]
    fn exact_argv_resolution_does_not_interpret_shell_metacharacters() {
        let values = vec![
            "two words".to_owned(),
            ";".to_owned(),
            "$(touch nope)".to_owned(),
            "*.rs".to_owned(),
            "a|b".to_owned(),
        ];
        validate_argv(&values).unwrap();
        assert_eq!(values[2], "$(touch nope)");
        assert!(validate_argv(&vec![String::new(); MAX_PROCESS_ARGUMENTS + 1]).is_err());
        assert!(validate_argv(&["x".repeat(MAX_PROCESS_TEXT_BYTES + 1)]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn exact_argv_resolution_preserves_symlink_name_as_argv0() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let root = crate::test_support::temp_root("process-argv0");
        let target = root.join("multicall");
        let requested = root.join("applet");
        fs::write(&target, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&target, &requested).unwrap();

        let resolved =
            resolve_program(requested.to_str().unwrap(), &root, &BTreeMap::new()).unwrap();
        assert_eq!(resolved.executable, target.canonicalize().unwrap());
        assert_eq!(resolved.argv0, requested.display().to_string());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resumed_context_keeps_its_frozen_environment_allowlist() {
        let workspace = std::env::temp_dir().canonicalize().unwrap();
        let snapshot = ExecutionContextSnapshot {
            workspace_root: workspace.clone(),
            working_directory: workspace,
            private_execution_roots: Vec::new(),
            shell: agl_exec::ShellProfileSnapshot {
                program: PathBuf::from("/bin/sh"),
                command_args: vec!["-c".to_owned()],
                login_command_args: None,
                environment_names: vec!["PATH".to_owned(), "LANG".to_owned()],
                executable_digest: "sha256:test-shell".to_owned(),
                config_digest: "sha256:test-config".to_owned(),
            },
            revision: 1,
            profile_metadata: "workspace".to_owned(),
        };
        let base = EnvironmentOverride {
            values: BTreeMap::from([
                ("PATH".to_owned(), "/bin".to_owned()),
                ("LANG".to_owned(), "C.UTF-8".to_owned()),
                ("NEW_AFTER_RESUME".to_owned(), "private".to_owned()),
            ]),
        };

        assert_eq!(
            frozen_base_environment(&snapshot, &base),
            BTreeMap::from([
                ("LANG".to_owned(), "C.UTF-8".to_owned()),
                ("PATH".to_owned(), "/bin".to_owned()),
            ])
        );
    }

    #[test]
    fn schemas_reject_unknown_fields_and_invalid_enums() {
        assert!(serde_json::from_value::<EmptyArgs>(json!({"old": true})).is_err());
        assert!(
            serde_json::from_value::<KillArgs>(
                json!({"execution_id": "exec_bad", "mode": "signal_9"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<StartArgs>(json!({
                "program": "/bin/true",
                "io": "pipes",
                "terminal_size": {"columns": 80, "rows": 24},
                "extra": true
            }))
            .is_err()
        );

        let extension = declaration();
        let schema = |id: &str| {
            let action = extension
                .tools
                .iter()
                .find(|action| action.id.as_str() == id)
                .unwrap();
            agl_kernel::ToolSchema::compile(&action.input_schema).unwrap()
        };
        assert!(
            schema(PROCESS_EXEC_TOOL_ID)
                .validate(&json!({"program": "", "timeout_ms": 0}))
                .is_err()
        );
        assert!(
            schema(PROCESS_START_TOOL_ID)
                .validate(&json!({
                    "program": "/bin/true",
                    "args": vec![""; MAX_PROCESS_ARGUMENTS + 1],
                    "io": "pty",
                    "terminal_size": {"columns": 0, "rows": 24}
                }))
                .is_err()
        );
        assert!(
            schema(SHELL_EXEC_TOOL_ID)
                .validate(&json!({
                    "command": "x".repeat(MAX_PROCESS_TEXT_BYTES + 1),
                    "semantic_intent": "exercise shell parsing",
                    "workspace_access": "read_only",
                    "expected_effects": {
                        "process": true,
                        "workspace_paths": [],
                        "external": []
                    },
                    "fallback_reason": "exact argv cannot express this test pipeline"
                }))
                .is_err()
        );
        for missing in [
            "semantic_intent",
            "workspace_access",
            "expected_effects",
            "fallback_reason",
        ] {
            let mut arguments = json!({
                "command": "true",
                "semantic_intent": "check a repository property",
                "workspace_access": "read_only",
                "expected_effects": {
                    "process": true,
                    "workspace_paths": [],
                    "external": []
                },
                "fallback_reason": "no structured Tool represents the property"
            });
            arguments.as_object_mut().unwrap().remove(missing);
            assert!(
                schema(SHELL_EXEC_TOOL_ID).validate(&arguments).is_err(),
                "missing required shell field `{missing}` was accepted"
            );
        }
        assert!(
            schema(PROCESS_READ_TOOL_ID)
                .validate(&json!({
                    "execution_id": ExecutionId::generate(),
                    "max_bytes": 65_537
                }))
                .is_err()
        );
    }

    #[test]
    fn cow_workspace_diff_is_scoped_and_source_changes_only_at_commit() {
        let root = crate::test_support::temp_root("shell-cow");
        fs::write(root.join("existing.txt"), "before\n").unwrap();
        let transaction = CowWorkspaceTransaction::create(&root).unwrap();
        fs::write(transaction.view_root().join("existing.txt"), "after\n").unwrap();
        fs::write(transaction.view_root().join("created.txt"), "new\n").unwrap();
        let diff = transaction
            .diff(&["existing.txt".to_owned(), "created.txt".to_owned()])
            .unwrap();

        assert_eq!(
            fs::read_to_string(root.join("existing.txt")).unwrap(),
            "before\n"
        );
        assert!(!root.join("created.txt").exists());
        assert_eq!(
            diff.changed_paths,
            vec!["created.txt".to_owned(), "existing.txt".to_owned()]
        );

        crate::CoreTools::new(&root)
            .unwrap()
            .apply_patch_for_tool(json!({"operations": diff.operations}))
            .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("existing.txt")).unwrap(),
            "after\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("created.txt")).unwrap(),
            "new\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cow_workspace_discards_out_of_envelope_changes() {
        let root = crate::test_support::temp_root("shell-cow-scope");
        fs::write(root.join("allowed.txt"), "before\n").unwrap();
        fs::write(root.join("outside.txt"), "before\n").unwrap();
        {
            let transaction = CowWorkspaceTransaction::create(&root).unwrap();
            fs::write(transaction.view_root().join("outside.txt"), "after\n").unwrap();
            assert!(
                transaction
                    .diff(&["allowed.txt".to_owned()])
                    .unwrap_err()
                    .to_string()
                    .contains("outside its expected-effect envelope")
            );
        }
        assert_eq!(
            fs::read_to_string(root.join("outside.txt")).unwrap(),
            "before\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn logical_cd_is_cas_scoped_and_host_denial_happens_before_the_handler() {
        let root = std::env::temp_dir().join(format!(
            "agl-process-tools-cd-{}-{}",
            std::process::id(),
            ExecutionId::generate()
        ));
        std::fs::create_dir_all(root.join("workspace/child")).unwrap();
        std::fs::create_dir_all(root.join("spool")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        let workspace = root.join("workspace").canonicalize().unwrap();
        let run_id = RunId::generate();
        let owner = agent_run_owner(&run_id, &run_id).unwrap();
        let execution_context = Arc::new(TestExecutionContext {
            snapshot: Mutex::new(ExecutionContextSnapshot {
                workspace_root: workspace.clone(),
                working_directory: workspace.clone(),
                private_execution_roots: Vec::new(),
                shell: agl_exec::ShellProfileSnapshot {
                    program: PathBuf::from("/bin/sh"),
                    command_args: vec!["-c".to_string()],
                    login_command_args: Some(vec!["-l".to_string(), "-c".to_string()]),
                    environment_names: vec!["PATH".to_string()],
                    executable_digest: "sha256:test-shell".to_string(),
                    config_digest: "sha256:test-config".to_string(),
                },
                revision: 1,
                profile_metadata: "workspace".to_string(),
            }),
            owner,
            durable_session_id: SessionId::generate(),
        });
        let tools = ProcessTools::new(
            Arc::new(
                TerminalEndpoint::new(
                    root.join("terminal.sock"),
                    root.join("service-identity.json"),
                    TerminalGenerationIdentity::new(
                        AuthorityFingerprint::new(format!("sha256:{}", "a".repeat(64))).unwrap(),
                        "b".repeat(40),
                        AuthorityFingerprint::new(format!("sha256:{}", "c".repeat(64))).unwrap(),
                        TERMINAL_PROTOCOL_VERSION,
                    )
                    .unwrap(),
                )
                .unwrap(),
            ),
            execution_context.clone(),
            ProcessToolRuntimeConfig {
                base_environment: EnvironmentOverride {
                    values: BTreeMap::from([("PATH".to_string(), "/usr/bin:/bin".to_string())]),
                },
                maximum_environment_bytes: 4096,
                runtime_read_only_roots: Vec::new(),
                default_foreground_timeout: Duration::from_secs(1),
                maximum_foreground_timeout: Duration::from_secs(10),
                max_input_bytes: 1024,
                max_result_bytes: 1024,
                max_spool_bytes: 4096,
                default_terminal_size: TerminalSize::default(),
            },
        )
        .unwrap();
        let extension = declaration();
        let mut runtime = agl_kernel::ToolRuntime::new();
        let bindings = PROCESS_TOOL_IDS
            .iter()
            .map(|id| ToolBinding::new(ToolId::new(*id).unwrap(), tools.clone()))
            .collect::<Vec<_>>();
        runtime
            .register_extension(ExtensionRegistration::new(extension.clone(), bindings))
            .unwrap();
        let tool_id = ToolId::new(PROCESS_CD_TOOL_ID).unwrap();
        let effective = ToolPolicyInput::new(
            [extension.clone()],
            [tool_id.clone()],
            ToolAccessMode::Write,
        )
        .resolve()
        .unwrap();
        let action = extension.tool(&tool_id).unwrap();
        let scope = ExecutionScope::builder(run_id)
            .step_id(StepId::generate())
            .build()
            .unwrap();
        let before = std::env::current_dir().unwrap();
        let invocation = ToolInvocation::new(
            scope.clone(),
            tool_id.clone(),
            extension.id.clone(),
            action.digest(),
            effective.policy_hash().clone(),
            json!({"path": "child"}),
        );
        let result = runtime
            .dispatch(invocation, &effective, ToolDispatchControl::uncancellable())
            .unwrap();
        assert_eq!(
            result.data.as_ref().unwrap()["working_directory"],
            workspace.join("child").display().to_string()
        );
        assert_eq!(execution_context.snapshot.lock().unwrap().revision, 2);
        assert_eq!(std::env::current_dir().unwrap(), before);

        let exec_id = ToolId::new(PROCESS_EXEC_TOOL_ID).unwrap();
        let exec_effective = ToolPolicyInput::new(
            [extension.clone()],
            [exec_id.clone()],
            ToolAccessMode::Execute,
        )
        .resolve()
        .unwrap();
        let exec_action = extension.tool(&exec_id).unwrap();
        let missing_program = root.join("definitely-missing-program");
        let exec_invocation = ToolInvocation::new(
            ExecutionScope::builder(RunId::generate())
                .step_id(StepId::generate())
                .build()
                .unwrap(),
            exec_id,
            extension.id.clone(),
            exec_action.digest(),
            exec_effective.policy_hash().clone(),
            json!({"program": missing_program}),
        );
        let outcome = runtime
            .dispatch(
                exec_invocation,
                &exec_effective,
                ToolDispatchControl::uncancellable(),
            )
            .unwrap();
        assert_eq!(outcome.status, ToolOutcomeStatus::RecoverableError);
        let error = outcome.error.unwrap();
        assert_eq!(error.code, "not_found");
        assert_eq!(
            error.data,
            json!({"program": root.join("definitely-missing-program")})
        );

        let host_invocation = ToolInvocation::new(
            scope,
            tool_id,
            extension.id.clone(),
            action.digest(),
            effective.policy_hash().clone(),
            json!({"path": root.clone(), "profile": "host"}),
        );
        let error = runtime
            .dispatch(
                host_invocation,
                &effective,
                ToolDispatchControl::uncancellable(),
            )
            .unwrap_err();
        assert_eq!(
            error.denial().unwrap().code,
            DispatchDenialCode::ConditionalEffectDenied
        );
        assert_eq!(execution_context.snapshot.lock().unwrap().revision, 2);
        std::fs::remove_dir_all(root).unwrap();
    }
}
