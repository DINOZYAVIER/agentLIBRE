use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agl_capabilities::{
    ActionDeclaration, ActionDispatchContext, ActionHandler, ActionHandlerError, ActionInvocation,
    ActionResult, CapabilityId, OperationKind, ProviderDeclaration, ProviderId, StateEffect,
};
use agl_ids::{ExecutionId, ExecutionScope, RequestId, SessionId, StepId};
use agl_process::{
    AdmittedShellKind, AdmittedShellProfile, EnvironmentOverride, ExecutionAuthorization,
    ExecutionContextSnapshot, ExecutionCursor, ExecutionGrantLease, ExecutionIo, ExecutionKind,
    ExecutionLimits, ExecutionOwner, ExecutionProfile, ExecutionRequest, HostStartupPolicy,
    KillMode, ProcessBytes, ProcessBytesEncoding, ProcessHandle, TerminalEnsureRequest,
    TerminalEnvironmentRequest, TerminalHistorySeed, TerminalOwner, TerminalRegistry, TerminalSize,
    resolve_execution_directory,
};
use anyhow::{Context, Result, bail, ensure};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{ToolCatalog, ToolCatalogError, parse_action_args as parse_args};

pub const PROVIDER_ID: &str = "core.process";
pub const PROCESS_PWD_TOOL_ID: &str = "process.pwd";
pub const PROCESS_CD_TOOL_ID: &str = "process.cd";
pub const PROCESS_EXEC_TOOL_ID: &str = "process.exec";
pub const PROCESS_START_TOOL_ID: &str = "process.start";
pub const PROCESS_STATUS_TOOL_ID: &str = "process.status";
pub const PROCESS_READ_TOOL_ID: &str = "process.read";
pub const PROCESS_WRITE_TOOL_ID: &str = "process.write";
pub const PROCESS_RESIZE_TOOL_ID: &str = "process.resize";
pub const PROCESS_KILL_TOOL_ID: &str = "process.kill";
pub const SHELL_EXEC_TOOL_ID: &str = "shell.exec";

const MAX_PROCESS_PATH_BYTES: usize = 4_096;
const MAX_PROCESS_TEXT_BYTES: usize = 65_536;
const MAX_PROCESS_ARGUMENTS: usize = 1_024;
const MAX_ENVIRONMENT_ENTRIES: usize = 256;
const MAX_ENVIRONMENT_NAME_BYTES: usize = 256;

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
    process: ProcessHandle,
    terminals: Arc<TerminalRegistry>,
    context: Arc<dyn ProcessExecutionContext>,
    config: ProcessToolRuntimeConfig,
}

impl ProcessTools {
    pub fn new(
        process: ProcessHandle,
        terminals: Arc<TerminalRegistry>,
        context: Arc<dyn ProcessExecutionContext>,
        config: ProcessToolRuntimeConfig,
    ) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            process,
            terminals,
            context,
            config,
        })
    }

    pub fn process_handle(&self) -> ProcessHandle {
        self.process.clone()
    }

    fn execute(&self, context: ActionDispatchContext) -> Result<Value> {
        let id = context.invocation().capability_id.as_str();
        match id {
            PROCESS_PWD_TOOL_ID => self.pwd(context),
            PROCESS_CD_TOOL_ID => self.cd(context),
            PROCESS_EXEC_TOOL_ID => self.exec(context),
            PROCESS_START_TOOL_ID => self.start(context),
            PROCESS_STATUS_TOOL_ID => self.status(context),
            PROCESS_READ_TOOL_ID => self.read(context),
            PROCESS_WRITE_TOOL_ID => self.write(context),
            PROCESS_RESIZE_TOOL_ID => self.resize(context),
            PROCESS_KILL_TOOL_ID => self.kill(context),
            SHELL_EXEC_TOOL_ID => self.shell(context),
            _ => bail!("unknown process tool `{id}`"),
        }
    }

    fn pwd(&self, context: ActionDispatchContext) -> Result<Value> {
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

    fn cd(&self, context: ActionDispatchContext) -> Result<Value> {
        let host_authorized = context
            .authorized_conditional_effects()
            .contains(&StateEffect::HostProcessExecution);
        let invocation = context.into_invocation();
        let args = parse_args::<CdArgs>(PROCESS_CD_TOOL_ID, invocation.arguments)?;
        validate_text(&args.path, "process.cd path", false, MAX_PROCESS_PATH_BYTES)?;
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

    fn exec(&self, context: ActionDispatchContext) -> Result<Value> {
        let args =
            parse_args::<ArgvArgs>(PROCESS_EXEC_TOOL_ID, context.invocation().arguments.clone())?;
        let request = self.argv_request(&context, args, ExecutionIo::Pipes, true)?;
        let owner = request.owner.clone();
        let started = self
            .process
            .start_cancellable(request, context.control().deadline(), || {
                context.control().is_cancelled()
            })
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let status = self
            .process
            .wait(
                &started.execution_id,
                &owner,
                context.control().deadline(),
                || context.control().is_cancelled(),
            )
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        self.foreground_result(PROCESS_EXEC_TOOL_ID, &owner, status)
    }

    fn start(&self, context: ActionDispatchContext) -> Result<Value> {
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
        let status = self
            .process
            .start_cancellable(request, context.control().deadline(), || {
                context.control().is_cancelled()
            })
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(background_result(PROCESS_START_TOOL_ID, &status))
    }

    fn shell(&self, context: ActionDispatchContext) -> Result<Value> {
        let args =
            parse_args::<ShellArgs>(SHELL_EXEC_TOOL_ID, context.invocation().arguments.clone())?;
        validate_text(
            &args.command,
            "shell.exec command",
            true,
            MAX_PROCESS_TEXT_BYTES,
        )?;
        let profile: ExecutionProfile = args.profile.unwrap_or_default().into();
        if profile == ExecutionProfile::Workspace {
            return self.agent_shell(context, args);
        }
        self.one_shot_host_shell(context, args)
    }

    fn agent_shell(&self, context: ActionDispatchContext, args: ShellArgs) -> Result<Value> {
        ensure!(
            args.cwd.is_none() && args.env.is_none() && args.terminal_size.is_none(),
            "persistent workspace shell.exec uses the owner's durable cwd, environment, and terminal size"
        );
        ensure!(
            !args.background.unwrap_or(false),
            "persistent workspace shell.exec uses native shell job control; put `&` in the command"
        );
        ensure!(
            !args.login.unwrap_or(false),
            "persistent workspace shell.exec is interactive and non-login"
        );
        let invocation = context.invocation();
        let admission = self.context.load(&invocation.scope)?;
        let execution_owner = admission.owner.clone();
        admission
            .snapshot
            .shell
            .verify_executable()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let (owner, root_run_id) = terminal_owner(&admission)?;
        let shell = admitted_agent_shell(&admission.snapshot)?;
        let environment = self.agent_terminal_environment(&admission.snapshot, &shell)?;
        let timeout_ms = self.timeout_ms(args.timeout_ms, true, context.control().remaining())?;
        let deadline = timeout_ms
            .map(Duration::from_millis)
            .and_then(|duration| std::time::Instant::now().checked_add(duration));
        let result = self
            .terminals
            .execute_agent_command_cancellable(
                TerminalEnsureRequest {
                    session_id: admission.durable_session_id,
                    owner,
                    root_run_id,
                    creating_run_id: invocation.scope.run_id().clone(),
                    creating_step_id: creating_step_id(&invocation.scope)?,
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
                    history_seed: TerminalHistorySeed::empty(),
                },
                args.command,
                deadline,
                || context.control().is_cancelled(),
            )
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let mut output = self
            .process
            .read(
                &result.execution_id,
                &execution_owner,
                ExecutionCursor {
                    after_sequence: result.output.after_sequence,
                },
                self.config.max_result_bytes,
            )
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
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

    fn one_shot_host_shell(
        &self,
        context: ActionDispatchContext,
        args: ShellArgs,
    ) -> Result<Value> {
        let profile = ExecutionProfile::Host;
        let login = args.login.unwrap_or(false);
        let background = args.background.unwrap_or(false);
        let invocation = context.invocation();
        let (authorization, grant_lease) = execution_authorization(&context, profile, login)?;
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
                .context("shell.exec login startup is not configured")?
        } else {
            admission.snapshot.shell.command_args.clone()
        };
        shell_args.push(args.command);
        let environment = self.environment(&admission.snapshot, args.env)?;
        let timeout_ms =
            self.timeout_ms(args.timeout_ms, !background, context.control().remaining())?;
        let size = args.terminal_size.unwrap_or(TerminalSizeArgs {
            columns: self.config.default_terminal_size.columns,
            rows: self.config.default_terminal_size.rows,
        });
        let request = ExecutionRequest {
            owner: admission.owner.clone(),
            creating_run_id: invocation.scope.run_id().clone(),
            creating_step_id: creating_step_id(&invocation.scope)?,
            kind: ExecutionKind::Shell,
            program: admission.snapshot.shell.program.clone(),
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
        let status = self
            .process
            .start_cancellable(request, context.control().deadline(), || {
                context.control().is_cancelled()
            })
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        if background {
            return Ok(background_result(SHELL_EXEC_TOOL_ID, &status));
        }
        let status = self
            .process
            .wait(
                &status.execution_id,
                &admission.owner,
                context.control().deadline(),
                || context.control().is_cancelled(),
            )
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        self.foreground_result(SHELL_EXEC_TOOL_ID, &admission.owner, status)
    }

    fn status(&self, context: ActionDispatchContext) -> Result<Value> {
        let invocation = context.into_invocation();
        let args = parse_args::<ExecutionIdArgs>(PROCESS_STATUS_TOOL_ID, invocation.arguments)?;
        let execution_id = parse_execution_id(&args.execution_id)?;
        let owner = self.context.load(&invocation.scope)?.owner;
        let status = self
            .process
            .status(&execution_id, &owner)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(json!({"tool": PROCESS_STATUS_TOOL_ID, "status": status}))
    }

    fn read(&self, context: ActionDispatchContext) -> Result<Value> {
        let invocation = context.into_invocation();
        let args = parse_args::<ReadOutputArgs>(PROCESS_READ_TOOL_ID, invocation.arguments)?;
        ensure!(
            args.max_bytes > 0 && args.max_bytes <= self.config.max_result_bytes,
            "process.read max_bytes must be between 1 and {}",
            self.config.max_result_bytes
        );
        let execution_id = parse_execution_id(&args.execution_id)?;
        let owner = self.context.load(&invocation.scope)?.owner;
        let output = self
            .process
            .read(
                &execution_id,
                &owner,
                ExecutionCursor {
                    after_sequence: args.after_sequence.unwrap_or(0),
                },
                args.max_bytes,
            )
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(json!({"tool": PROCESS_READ_TOOL_ID, "output": output}))
    }

    fn write(&self, context: ActionDispatchContext) -> Result<Value> {
        let invocation = context.into_invocation();
        let args = parse_args::<WriteArgs>(PROCESS_WRITE_TOOL_ID, invocation.arguments)?;
        let execution_id = parse_execution_id(&args.execution_id)?;
        let bytes: ProcessBytes = args.bytes.into();
        bytes
            .decode(self.config.max_input_bytes)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let owner = self.context.load(&invocation.scope)?.owner;
        let lease = self
            .process
            .attach(&execution_id, &owner, RequestId::generate(), true)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        let written = self.process.write(
            &execution_id,
            &owner,
            lease.clone(),
            bytes,
            args.eof.unwrap_or(false),
        );
        let detached = self.process.detach(&execution_id, &owner, lease);
        written.map_err(|error| anyhow::anyhow!(error.to_string()))?;
        detached.map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(json!({
            "tool": PROCESS_WRITE_TOOL_ID,
            "status": "accepted",
            "execution_id": execution_id,
            "eof": args.eof.unwrap_or(false),
        }))
    }

    fn resize(&self, context: ActionDispatchContext) -> Result<Value> {
        let invocation = context.into_invocation();
        let args = parse_args::<ResizeArgs>(PROCESS_RESIZE_TOOL_ID, invocation.arguments)?;
        let execution_id = parse_execution_id(&args.execution_id)?;
        let owner = self.context.load(&invocation.scope)?.owner;
        let terminal_size = TerminalSize {
            columns: args.columns,
            rows: args.rows,
        };
        terminal_size
            .validate()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        self.process
            .resize(&execution_id, &owner, terminal_size)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(json!({
            "tool": PROCESS_RESIZE_TOOL_ID,
            "status": "resized",
            "execution_id": execution_id,
            "terminal_size": terminal_size,
        }))
    }

    fn kill(&self, context: ActionDispatchContext) -> Result<Value> {
        let invocation = context.into_invocation();
        let args = parse_args::<KillArgs>(PROCESS_KILL_TOOL_ID, invocation.arguments)?;
        let execution_id = parse_execution_id(&args.execution_id)?;
        let owner = self.context.load(&invocation.scope)?.owner;
        let mode = args.mode.unwrap_or_default().into();
        self.process
            .kill(&execution_id, &owner, mode)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(json!({
            "tool": PROCESS_KILL_TOOL_ID,
            "status": "termination_requested",
            "execution_id": execution_id,
            "mode": mode,
        }))
    }

    fn argv_request(
        &self,
        context: &ActionDispatchContext,
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
        let (authorization, grant_lease) = execution_authorization(context, profile, false)?;
        let admission = self.context.load(&invocation.scope)?;
        let cwd = resolve_cwd(
            &admission.snapshot,
            args.cwd.as_deref(),
            profile,
            authorization.host_process_execution,
        )?;
        let environment = self.environment(&admission.snapshot, args.env)?;
        let program = resolve_program(&args.program, &cwd, &environment.values)?;
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
            creating_run_id: invocation.scope.run_id().clone(),
            creating_step_id: creating_step_id(&invocation.scope)?,
            kind: ExecutionKind::Argv,
            program,
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

    fn foreground_result(
        &self,
        tool_id: &str,
        owner: &ExecutionOwner,
        status: agl_process::ExecutionStatus,
    ) -> Result<Value> {
        let output = self
            .process
            .read(
                &status.execution_id,
                owner,
                ExecutionCursor::default(),
                self.config.max_result_bytes,
            )
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
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
    match &admission.owner {
        ExecutionOwner::Session {
            session_id,
            root_run_id,
        } => {
            ensure!(
                session_id == &admission.durable_session_id,
                "session process owner differs from its durable terminal session"
            );
            Ok((
                TerminalOwner::MainAgent {
                    session_id: session_id.clone(),
                },
                root_run_id.clone(),
            ))
        }
        ExecutionOwner::Run {
            run_id,
            root_run_id,
        } => Ok((
            TerminalOwner::Subagent {
                root_run_id: root_run_id.clone(),
                owner_run_id: run_id.clone(),
            },
            root_run_id.clone(),
        )),
    }
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
    let mut roots = agl_process::process_standard_runtime_roots()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
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

impl ActionHandler for ProcessTools {
    fn preflight(
        &self,
        invocation: &ActionInvocation,
    ) -> std::result::Result<BTreeSet<StateEffect>, ActionHandlerError> {
        Ok(requested_conditional_effects(invocation)?)
    }

    fn dispatch(
        &self,
        context: ActionDispatchContext,
    ) -> std::result::Result<ActionResult, ActionHandlerError> {
        Ok(ActionResult::new(self.execute(context)?))
    }
}

pub fn declaration() -> ProviderDeclaration {
    ProviderDeclaration::builtin(
        ProviderId::new(PROVIDER_ID).expect("process provider id is valid"),
        "Core Process",
        env!("CARGO_PKG_VERSION"),
    )
    .expect("process provider declaration is valid")
    .with_action(action::<EmptyArgs>(
        PROCESS_PWD_TOOL_ID,
        "Return the caller's durable logical working directory.",
        OperationKind::Read,
        &[],
        &[],
    ))
    .with_action(action::<CdArgs>(
        PROCESS_CD_TOOL_ID,
        "Change the caller's durable logical working directory.",
        OperationKind::Write,
        &[StateEffect::SessionWorkingDirectory],
        &[StateEffect::HostProcessExecution],
    ))
    .with_action(action::<ArgvArgs>(
        PROCESS_EXEC_TOOL_ID,
        "Run an exact argv with pipes and wait for its bounded result.",
        OperationKind::Execute,
        &[StateEffect::SpawnProcess],
        &[StateEffect::HostProcessExecution],
    ))
    .with_action(action::<StartArgs>(
        PROCESS_START_TOOL_ID,
        "Start an exact argv with pipes or a PTY and return its execution handle.",
        OperationKind::Execute,
        &[StateEffect::SpawnProcess],
        &[StateEffect::HostProcessExecution],
    ))
    .with_action(action::<ExecutionIdArgs>(
        PROCESS_STATUS_TOOL_ID,
        "Return safe lifecycle and retention status for an owned execution.",
        OperationKind::Read,
        &[],
        &[],
    ))
    .with_action(action::<ReadOutputArgs>(
        PROCESS_READ_TOOL_ID,
        "Read bounded ordered output chunks from an owned execution.",
        OperationKind::Read,
        &[],
        &[],
    ))
    .with_action(action::<WriteArgs>(
        PROCESS_WRITE_TOOL_ID,
        "Write bounded bytes or EOF to an owned live execution.",
        OperationKind::Execute,
        &[StateEffect::ControlProcess],
        &[],
    ))
    .with_action(action::<ResizeArgs>(
        PROCESS_RESIZE_TOOL_ID,
        "Resize an owned live PTY.",
        OperationKind::Execute,
        &[StateEffect::ControlProcess],
        &[],
    ))
    .with_action(action::<KillArgs>(
        PROCESS_KILL_TOOL_ID,
        "Request graceful or immediate termination of an owned process tree.",
        OperationKind::Execute,
        &[StateEffect::ControlProcess],
        &[],
    ))
    .with_action(action::<ShellArgs>(
        SHELL_EXEC_TOOL_ID,
        "Run one command through the admitted shell profile with a real PTY.",
        OperationKind::Execute,
        &[StateEffect::SpawnProcess],
        &[
            StateEffect::HostProcessExecution,
            StateEffect::ShellLoginStartup,
        ],
    ))
}

pub fn register(catalog: &mut ToolCatalog) -> Result<(), ToolCatalogError> {
    catalog.register(declaration())
}

fn action<T: JsonSchema>(
    id: &str,
    description: &str,
    operation: OperationKind,
    effects: &[StateEffect],
    conditional_effects: &[StateEffect],
) -> ActionDeclaration {
    ActionDeclaration::from_schema::<T>(
        CapabilityId::new(id).expect("process tool id is valid"),
        description,
        operation,
    )
    .expect("process action schema is valid")
    .with_state_effects(effects.iter().copied())
    .with_conditional_state_effects(conditional_effects.iter().copied())
}

fn requested_conditional_effects(invocation: &ActionInvocation) -> Result<BTreeSet<StateEffect>> {
    let mut effects = BTreeSet::new();
    match invocation.capability_id.as_str() {
        PROCESS_CD_TOOL_ID => {
            let args = parse_args::<CdArgs>(PROCESS_CD_TOOL_ID, invocation.arguments.clone())?;
            if args.profile.unwrap_or_default() == ProfileArg::Host {
                effects.insert(StateEffect::HostProcessExecution);
            }
        }
        PROCESS_EXEC_TOOL_ID => {
            let args = parse_args::<ArgvArgs>(PROCESS_EXEC_TOOL_ID, invocation.arguments.clone())?;
            if args.profile.unwrap_or_default() == ProfileArg::Host {
                effects.insert(StateEffect::HostProcessExecution);
            }
        }
        PROCESS_START_TOOL_ID => {
            let args =
                parse_args::<StartArgs>(PROCESS_START_TOOL_ID, invocation.arguments.clone())?;
            if args.argv.profile.unwrap_or_default() == ProfileArg::Host {
                effects.insert(StateEffect::HostProcessExecution);
            }
        }
        SHELL_EXEC_TOOL_ID => {
            let args = parse_args::<ShellArgs>(SHELL_EXEC_TOOL_ID, invocation.arguments.clone())?;
            if args.profile.unwrap_or_default() == ProfileArg::Host || args.login.unwrap_or(false) {
                effects.insert(StateEffect::HostProcessExecution);
            }
            if args.login.unwrap_or(false) {
                effects.insert(StateEffect::ShellLoginStartup);
            }
        }
        _ => {}
    }
    Ok(effects)
}

fn execution_authorization(
    context: &ActionDispatchContext,
    profile: ExecutionProfile,
    login: bool,
) -> Result<(ExecutionAuthorization, Option<ExecutionGrantLease>)> {
    let effects = context.authorized_conditional_effects();
    let host = effects.contains(&StateEffect::HostProcessExecution);
    let login_authorized = effects.contains(&StateEffect::ShellLoginStartup);
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
            .effective_capability()
            .grant_provenance()
            .context("host process execution lacks immutable grant provenance")?;
        Some(ExecutionGrantLease {
            origin: agl_process::ExecutionLeaseOrigin::CapabilityGrant,
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

fn resolve_program(
    program: &str,
    cwd: &Path,
    environment: &BTreeMap<String, String>,
) -> Result<PathBuf> {
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
    Ok(resolved)
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

fn background_result(tool_id: &str, status: &agl_process::ExecutionStatus) -> Value {
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

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ShellArgs {
    #[schemars(length(max = 65536))]
    command: String,
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use agl_capabilities::{
        ActionDispatchControl, CapabilityPolicyInput, DispatchDenialCode, ToolAccessMode,
    };
    use agl_ids::{RunId, StepId};

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
    fn declaration_has_exact_provider_actions_and_effect_classes() {
        let declaration = declaration();
        assert_eq!(declaration.id.as_str(), PROVIDER_ID);
        assert_eq!(
            declaration
                .actions
                .iter()
                .map(|action| action.id.as_str())
                .collect::<BTreeSet<_>>(),
            PROCESS_TOOL_IDS.iter().copied().collect()
        );
        let shell = declaration
            .actions
            .iter()
            .find(|action| action.id.as_str() == SHELL_EXEC_TOOL_ID)
            .unwrap();
        assert_eq!(shell.operation_kind, OperationKind::Execute);
        assert_eq!(shell.state_effects, [StateEffect::SpawnProcess].into());
        assert_eq!(
            shell.conditional_state_effects,
            [
                StateEffect::HostProcessExecution,
                StateEffect::ShellLoginStartup,
            ]
            .into()
        );
    }

    #[test]
    fn conditional_preflight_is_exact_and_login_implies_host() {
        let provider = declaration();
        let shell = provider
            .actions
            .iter()
            .find(|action| action.id.as_str() == SHELL_EXEC_TOOL_ID)
            .unwrap();
        let invocation = ActionInvocation::new(
            ExecutionScope::builder(agl_ids::RunId::generate())
                .build()
                .unwrap(),
            shell.id.clone(),
            provider.id.clone(),
            shell.digest(),
            agl_capabilities::PolicyHash::parse(&format!("sha256:{}", "0".repeat(64))).unwrap(),
            json!({"command": "true", "profile": "host", "login": true}),
        );
        assert_eq!(
            requested_conditional_effects(&invocation).unwrap(),
            [
                StateEffect::HostProcessExecution,
                StateEffect::ShellLoginStartup,
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

    #[test]
    fn resumed_context_keeps_its_frozen_environment_allowlist() {
        let workspace = std::env::temp_dir().canonicalize().unwrap();
        let snapshot = ExecutionContextSnapshot {
            workspace_root: workspace.clone(),
            working_directory: workspace,
            private_execution_roots: Vec::new(),
            shell: agl_process::ShellProfileSnapshot {
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

        let provider = declaration();
        let schema = |id: &str| {
            let action = provider
                .actions
                .iter()
                .find(|action| action.id.as_str() == id)
                .unwrap();
            agl_capabilities::ActionSchema::compile(&action.input_schema).unwrap()
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
                .validate(&json!({"command": "x".repeat(MAX_PROCESS_TEXT_BYTES + 1)}))
                .is_err()
        );
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
    fn logical_cd_is_cas_scoped_and_host_denial_happens_before_the_handler() {
        let root = std::env::temp_dir().join(format!(
            "agl-process-tools-cd-{}-{}",
            std::process::id(),
            agl_ids::ExecutionId::generate()
        ));
        std::fs::create_dir_all(root.join("workspace/child")).unwrap();
        std::fs::create_dir_all(root.join("spool")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        let workspace = root.join("workspace").canonicalize().unwrap();
        let run_id = RunId::generate();
        let owner = ExecutionOwner::Run {
            run_id: run_id.clone(),
            root_run_id: run_id.clone(),
        };
        let execution_context = Arc::new(TestExecutionContext {
            snapshot: Mutex::new(ExecutionContextSnapshot {
                workspace_root: workspace.clone(),
                working_directory: workspace.clone(),
                private_execution_roots: Vec::new(),
                shell: agl_process::ShellProfileSnapshot {
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
        let repository = Arc::new(agl_process::InMemoryExecutionRepository::new());
        let spool = Arc::new(agl_process::FileOutputSpool::new(root.join("spool")).unwrap());
        let supervisor = agl_process::ProcessSupervisor::start(
            agl_process::ProcessSupervisorOptions {
                launcher_path: root.join("missing-launcher"),
                data_root: root.join("spool"),
                state_root: root.join("state"),
                max_active: 1,
                command_capacity: 8,
                poll_interval: Duration::from_millis(1),
                setup_timeout: Duration::from_millis(100),
                termination_grace: Duration::from_millis(10),
                max_input_bytes: 1024,
                max_result_bytes: 1024,
                max_spool_bytes: 4096,
                termination_output_headroom_bytes: 1024,
                finished_retention: Duration::from_secs(60),
                runtime_read_only_roots: Vec::new(),
            },
            repository,
            spool,
        )
        .unwrap();
        let tools = ProcessTools::new(
            supervisor.handle(),
            Arc::new(
                TerminalRegistry::new(
                    supervisor.handle(),
                    Arc::new(agl_process::RejectTerminalSecrets),
                    Arc::new(agl_process::InMemoryTerminalRepository::new()),
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
        let provider = declaration();
        let mut runtime = crate::ToolRuntime::new();
        runtime.register_provider(provider.clone()).unwrap();
        for id in PROCESS_TOOL_IDS {
            runtime
                .register_handler(CapabilityId::new(*id).unwrap(), tools.clone())
                .unwrap();
        }
        let capability_id = CapabilityId::new(PROCESS_CD_TOOL_ID).unwrap();
        let effective = CapabilityPolicyInput::new(
            [provider.clone()],
            [capability_id.clone()],
            ToolAccessMode::Write,
        )
        .resolve()
        .unwrap();
        let action = provider.action(&capability_id).unwrap();
        let scope = ExecutionScope::builder(run_id)
            .step_id(StepId::generate())
            .build()
            .unwrap();
        let before = std::env::current_dir().unwrap();
        let invocation = ActionInvocation::new(
            scope.clone(),
            capability_id.clone(),
            provider.id.clone(),
            action.digest(),
            effective.policy_hash().clone(),
            json!({"path": "child"}),
        );
        let result = runtime
            .dispatch(
                invocation,
                &effective,
                ActionDispatchControl::uncancellable(),
            )
            .unwrap();
        assert_eq!(
            result.data["working_directory"],
            workspace.join("child").display().to_string()
        );
        assert_eq!(execution_context.snapshot.lock().unwrap().revision, 2);
        assert_eq!(std::env::current_dir().unwrap(), before);

        let host_invocation = ActionInvocation::new(
            scope,
            capability_id,
            provider.id.clone(),
            action.digest(),
            effective.policy_hash().clone(),
            json!({"path": root.clone(), "profile": "host"}),
        );
        let error = runtime
            .dispatch(
                host_invocation,
                &effective,
                ActionDispatchControl::uncancellable(),
            )
            .unwrap_err();
        assert_eq!(
            error.denial().unwrap().code,
            DispatchDenialCode::ConditionalEffectDenied
        );
        assert_eq!(execution_context.snapshot.lock().unwrap().revision, 2);
        supervisor.shutdown().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
