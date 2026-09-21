use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agl_core::ToolId;
use agl_core::agent::{EffectReceipt, ToolFailure, ToolFailureKind, ToolResult};
use agl_execution_api::{
    ExecutionClient, ExecutionIo, ExecutionOutputStream, ExecutionOwner, ExecutionSignal,
    ExecutionStartRequest, ExecutionState, ExecutionStatus, TerminalId, TerminalSize,
};
use agl_runtime::extension::{
    ExtensionBindings, ToolBinding, ToolContext, ToolFuture, ToolHandler, parse_package_view,
};
use agl_runtime::package::{InMemoryPackageView, PackageRelativePath, compute_package_digest};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const EXEC_EFFECT: &str = "agentlibre.execution:process.execute";
const TERMINAL_EFFECT: &str = "agentlibre.execution:terminal.control";
const COMMAND_TOOL: &str = "agentlibre.execution:command.exec";
const TERMINAL_TOOL: &str = "agentlibre.execution:terminal.session";
const MAX_EXECUTION_TIMEOUT_MS: u64 = 3_600_000;

pub(crate) fn bindings(client: ExecutionClient) -> ExtensionBindings {
    let declaration = declaration_view();
    let package = parse_package_view(&declaration).expect("embedded execution Extension");
    let tools = package
        .definition
        .tools
        .iter()
        .cloned()
        .map(|definition| ToolBinding {
            tool_id: definition.id.clone(),
            definition_digest: definition_digest(&definition),
            handler: Arc::new(ExecutionTool {
                tool_id: definition.id,
                client: client.clone(),
            }),
        })
        .collect();
    ExtensionBindings {
        version: package.manifest.version,
        content_digest: compute_package_digest(&declaration)
            .expect("embedded execution Extension digest"),
        definition: package.definition,
        tools,
        allows_authority: Arc::new(valid_execution_scope),
    }
}

fn valid_execution_scope(grant: &agl_core::AuthorityGrant) -> bool {
    if !matches!(grant.effect.as_str(), EXEC_EFFECT | TERMINAL_EFFECT) {
        return false;
    }
    let scope = grant.scope.as_value();
    scope
        .get("root")
        .and_then(Value::as_str)
        .is_some_and(|root| Path::new(root).is_absolute())
        && scope
            .get("executables")
            .and_then(Value::as_array)
            .is_some_and(|executables| {
                !executables.is_empty()
                    && executables.iter().all(|value| {
                        value.as_str().is_some_and(|name| {
                            !name.is_empty()
                                && name.len() <= 128
                                && name.bytes().all(|byte| {
                                    byte.is_ascii_alphanumeric()
                                        || matches!(byte, b'.' | b'_' | b'+' | b'-')
                                })
                        })
                    })
            })
}

struct ExecutionTool {
    tool_id: ToolId,
    client: ExecutionClient,
}

impl ToolHandler for ExecutionTool {
    fn call(&self, context: ToolContext, input: Value) -> ToolFuture {
        let tool_id = self.tool_id.clone();
        let client = self.client.clone();
        Box::pin(async move {
            match tool_id.as_str() {
                COMMAND_TOOL => command_exec(&client, context, input).await,
                TERMINAL_TOOL => terminal_session(&client, context, input).await,
                _ => Err(failure(ToolFailureKind::Unavailable)),
            }
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandExecInput {
    argv: Vec<String>,
    cwd: Option<String>,
    timeout_ms: Option<u64>,
    max_output_bytes: Option<u64>,
}

async fn command_exec(
    client: &ExecutionClient,
    context: ToolContext,
    input: Value,
) -> Result<ToolResult, ToolFailure> {
    let input: CommandExecInput = serde_json::from_value(input)
        .map_err(|_| rejection(ToolFailureKind::InvalidInput, "input"))?;
    if context.read_only_workspace && !planner_command_is_read_only(&input.argv) {
        return Err(rejection(
            ToolFailureKind::Unauthorized,
            "planner_workspace",
        ));
    }
    let executable = admitted_executable(&context, EXEC_EFFECT, &input.argv)?;
    let cwd = contained_cwd(&context, input.cwd.as_deref())?;
    let owner = owner(&context)?;
    let remaining = remaining_ms(context.deadline_at_ms)?;
    let timeout_ms = input
        .timeout_ms
        .unwrap_or(remaining)
        .min(remaining)
        .clamp(1, MAX_EXECUTION_TIMEOUT_MS);
    let max_output_bytes = input
        .max_output_bytes
        .unwrap_or(context.result_bytes)
        .min(context.result_bytes)
        .max(1);
    let mut argv = input.argv;
    argv[0] = executable.to_string_lossy().into_owned();
    let status = client
        .start(ExecutionStartRequest {
            owner,
            argv,
            cwd: cwd.to_string_lossy().into_owned(),
            environment: Default::default(),
            clear_environment: false,
            io: ExecutionIo::Pipes,
            timeout_ms,
            max_output_bytes,
            terminal_size: None,
            isolation: agl_execution_api::ExecutionIsolation::Standard,
        })
        .await
        .map_err(client_failure)?;
    let status = wait_for_terminal(client, &context, status).await?;
    let (stdout, stderr, truncated) = read_command_output(client, status.execution_id).await?;
    tool_result(
        json!({
            "execution_id": status.execution_id,
            "state": status.state,
            "outcome": status.outcome,
            "stdout": String::from_utf8_lossy(&stdout),
            "stderr": String::from_utf8_lossy(&stderr),
            "truncated": truncated,
        }),
        receipt(&context, EXEC_EFFECT)?,
        context.result_bytes,
    )
}

fn planner_command_is_read_only(argv: &[String]) -> bool {
    let Some(command) = argv.first().and_then(|value| Path::new(value).file_name()) else {
        return false;
    };
    let command = command.to_string_lossy();
    let forbidden_argument = argv.iter().skip(1).any(|argument| {
        matches!(
            argument.as_str(),
            "-i" | "--in-place" | "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir"
        ) || argument.starts_with("--git-dir")
            || argument.starts_with("--work-tree")
            || argument == "-C"
    });
    if forbidden_argument {
        return false;
    }
    match command.as_ref() {
        "cat" | "head" | "tail" | "sed" | "rg" | "grep" | "find" | "ls" | "pwd" | "stat"
        | "file" | "sort" | "wc" => true,
        "git" => matches!(
            argv.get(1).map(String::as_str),
            Some("status")
                | Some("diff")
                | Some("log")
                | Some("show")
                | Some("ls-files")
                | Some("rev-parse")
                | Some("cat-file")
        ),
        _ => false,
    }
}

async fn read_command_output(
    client: &ExecutionClient,
    execution_id: agl_execution_api::ExecutionId,
) -> Result<(Vec<u8>, Vec<u8>, bool), ToolFailure> {
    let mut cursor = 0;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        let output = client
            .read(
                execution_id,
                cursor,
                agl_execution_api::MAX_EXECUTION_OUTPUT_READ_BYTES,
            )
            .await
            .map_err(client_failure)?;
        for chunk in output.chunks {
            match chunk.stream {
                ExecutionOutputStream::Stdout => stdout.extend(chunk.data),
                ExecutionOutputStream::Stderr => stderr.extend(chunk.data),
                ExecutionOutputStream::Pty => return Err(failure(ToolFailureKind::InvalidResult)),
            }
        }
        if output.eof {
            return Ok((stdout, stderr, output.truncated));
        }
        if output.next <= cursor {
            return Err(failure(ToolFailureKind::Execution));
        }
        cursor = output.next;
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum TerminalInput {
    Open {
        argv: Vec<String>,
        cwd: Option<String>,
        rows: Option<u16>,
        columns: Option<u16>,
    },
    Inspect {
        terminal_id: TerminalId,
    },
    Read {
        terminal_id: TerminalId,
        after: Option<u64>,
        max_bytes: Option<u32>,
    },
    Write {
        terminal_id: TerminalId,
        input: String,
    },
    Resize {
        terminal_id: TerminalId,
        rows: u16,
        columns: u16,
    },
    Interrupt {
        terminal_id: TerminalId,
    },
    Terminate {
        terminal_id: TerminalId,
    },
}

async fn terminal_session(
    client: &ExecutionClient,
    context: ToolContext,
    input: Value,
) -> Result<ToolResult, ToolFailure> {
    let input: TerminalInput = serde_json::from_value(input)
        .map_err(|_| rejection(ToolFailureKind::InvalidInput, "input"))?;
    let value = match input {
        TerminalInput::Open {
            mut argv,
            cwd,
            rows,
            columns,
        } => {
            let executable = admitted_executable(&context, TERMINAL_EFFECT, &argv)?;
            argv[0] = executable.to_string_lossy().into_owned();
            let cwd = contained_cwd(&context, cwd.as_deref())?;
            let status = client
                .start(ExecutionStartRequest {
                    owner: owner(&context)?,
                    argv,
                    cwd: cwd.to_string_lossy().into_owned(),
                    environment: Default::default(),
                    clear_environment: false,
                    io: ExecutionIo::Pty,
                    timeout_ms: remaining_ms(context.deadline_at_ms)?.min(MAX_EXECUTION_TIMEOUT_MS),
                    max_output_bytes: 16 * 1024 * 1024,
                    terminal_size: Some(TerminalSize {
                        rows: rows.unwrap_or(24),
                        columns: columns.unwrap_or(80),
                    }),
                    isolation: agl_execution_api::ExecutionIsolation::Standard,
                })
                .await
                .map_err(client_failure)?;
            serde_json::to_value(status).map_err(|_| failure(ToolFailureKind::InvalidResult))?
        }
        TerminalInput::Inspect { terminal_id } => {
            let status = owned_terminal(client, &context, terminal_id).await?;
            serde_json::to_value(status).map_err(|_| failure(ToolFailureKind::InvalidResult))?
        }
        TerminalInput::Read {
            terminal_id,
            after,
            max_bytes,
        } => {
            let status = owned_terminal(client, &context, terminal_id).await?;
            let output = client
                .read(
                    status.execution_id,
                    after.unwrap_or(0),
                    max_bytes.unwrap_or(64 * 1024),
                )
                .await
                .map_err(client_failure)?;
            let bytes = output_bytes(&output, None);
            json!({
                "execution_id": output.execution_id,
                "terminal_id": terminal_id,
                "after": output.after,
                "next": output.next,
                "output": String::from_utf8_lossy(&bytes),
                "eof": output.eof,
                "truncated": output.truncated,
                "state": status.state,
                "outcome": status.outcome,
                "next_actions": terminal_next_actions(status.state),
            })
        }
        TerminalInput::Write { terminal_id, input } => {
            let status =
                require_running_terminal(owned_terminal(client, &context, terminal_id).await?)?;
            if let Err(error) = client.write(terminal_id, input.into_bytes()).await {
                return Err(terminal_action_failure(client, &context, status, error).await);
            }
            json!({"terminal_id": terminal_id, "written": true})
        }
        TerminalInput::Resize {
            terminal_id,
            rows,
            columns,
        } => {
            let status =
                require_running_terminal(owned_terminal(client, &context, terminal_id).await?)?;
            if let Err(error) = client
                .resize(terminal_id, TerminalSize { rows, columns })
                .await
            {
                return Err(terminal_action_failure(client, &context, status, error).await);
            }
            json!({"terminal_id": terminal_id, "resized": true})
        }
        TerminalInput::Interrupt { terminal_id } => {
            let status =
                require_running_terminal(owned_terminal(client, &context, terminal_id).await?)?;
            if let Err(error) = client
                .signal(status.execution_id, ExecutionSignal::Interrupt)
                .await
            {
                return Err(terminal_action_failure(client, &context, status, error).await);
            }
            json!({"terminal_id": terminal_id, "interrupted": true})
        }
        TerminalInput::Terminate { terminal_id } => {
            let status =
                require_running_terminal(owned_terminal(client, &context, terminal_id).await?)?;
            if let Err(error) = client
                .signal(status.execution_id, ExecutionSignal::Terminate)
                .await
            {
                return Err(terminal_action_failure(client, &context, status, error).await);
            }
            json!({"terminal_id": terminal_id, "terminated": true})
        }
    };
    tool_result(
        value,
        receipt(&context, TERMINAL_EFFECT)?,
        context.result_bytes,
    )
}

fn require_running_terminal(status: ExecutionStatus) -> Result<ExecutionStatus, ToolFailure> {
    match status.state {
        ExecutionState::Running => Ok(status),
        ExecutionState::Exited => Err(exited_terminal(&status)),
        ExecutionState::OutcomeUnknown => Err(failure(ToolFailureKind::OutcomeUnknown)),
    }
}

async fn terminal_action_failure(
    client: &ExecutionClient,
    context: &ToolContext,
    previous: ExecutionStatus,
    error: agl_execution_api::ExecutionClientError,
) -> ToolFailure {
    let conflict = matches!(
        &error,
        agl_execution_api::ExecutionClientError::Protocol(error)
            if error.code == agl_execution_api::ExecutionProtocolErrorCode::Conflict
    );
    if conflict
        && let Ok(status) = owned_terminal(
            client,
            context,
            previous.terminal_id.expect("terminal status"),
        )
        .await
    {
        return match status.state {
            ExecutionState::Exited => exited_terminal(&status),
            ExecutionState::OutcomeUnknown => failure(ToolFailureKind::OutcomeUnknown),
            ExecutionState::Running => client_failure(error),
        };
    }
    client_failure(error)
}

fn exited_terminal(status: &ExecutionStatus) -> ToolFailure {
    let details = agl_core::CanonicalJson::new(json!({
        "execution_id": status.execution_id,
        "terminal_id": status.terminal_id,
        "state": status.state,
        "outcome": status.outcome,
    }))
    .expect("bounded terminal lifecycle details");
    ToolFailure::no_effect_with_details(
        ToolFailureKind::Unavailable,
        Some("terminal_id"),
        &[
            "Read the terminal's final output or inspect its exit status.",
            "Open a new terminal session before writing, resizing, interrupting or terminating.",
        ],
        details,
    )
}

fn terminal_next_actions(state: ExecutionState) -> &'static [&'static str] {
    match state {
        ExecutionState::Running => &["Continue reading or control this terminal session."],
        ExecutionState::Exited => {
            &["Inspect the exit status or open a new terminal session before sending more input."]
        }
        ExecutionState::OutcomeUnknown => {
            &["Inspect the execution record; do not assume the process completed successfully."]
        }
    }
}

fn output_bytes(
    output: &agl_execution_api::ExecutionOutput,
    stream: Option<ExecutionOutputStream>,
) -> Vec<u8> {
    output
        .chunks
        .iter()
        .filter(|chunk| stream.is_none_or(|stream| chunk.stream == stream))
        .flat_map(|chunk| chunk.data.iter().copied())
        .collect()
}

async fn owned_terminal(
    client: &ExecutionClient,
    context: &ToolContext,
    terminal_id: TerminalId,
) -> Result<ExecutionStatus, ToolFailure> {
    let status = client
        .inspect_terminal(terminal_id)
        .await
        .map_err(client_failure)?;
    let conversation_id = context
        .conversation_id
        .ok_or_else(|| failure(ToolFailureKind::Unauthorized))?;
    if !matches!(
        status.owner,
        ExecutionOwner::Agent {
            conversation_id: owner,
            ..
        } if owner == conversation_id
    ) {
        return Err(failure(ToolFailureKind::Unauthorized));
    }
    Ok(status)
}

async fn wait_for_terminal(
    client: &ExecutionClient,
    context: &ToolContext,
    mut status: ExecutionStatus,
) -> Result<ExecutionStatus, ToolFailure> {
    while status.state == ExecutionState::Running {
        if context.is_cancelled() {
            client
                .signal(status.execution_id, ExecutionSignal::Terminate)
                .await
                .map_err(client_failure)?;
            return Err(failure(ToolFailureKind::Cancelled));
        }
        if now_ms() >= context.deadline_at_ms {
            client
                .signal(status.execution_id, ExecutionSignal::Terminate)
                .await
                .map_err(client_failure)?;
            return Err(failure(ToolFailureKind::Deadline));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        status = client
            .inspect(status.execution_id)
            .await
            .map_err(client_failure)?;
    }
    if status.state == ExecutionState::OutcomeUnknown {
        return Err(failure(ToolFailureKind::OutcomeUnknown));
    }
    Ok(status)
}

fn owner(context: &ToolContext) -> Result<ExecutionOwner, ToolFailure> {
    Ok(ExecutionOwner::Agent {
        conversation_id: context
            .conversation_id
            .ok_or_else(|| failure(ToolFailureKind::Unauthorized))?,
        run_id: context.operation.run_id,
    })
}

fn admitted_executable(
    context: &ToolContext,
    effect: &str,
    argv: &[String],
) -> Result<PathBuf, ToolFailure> {
    let name = argv
        .first()
        .filter(|name| !name.is_empty() && !name.contains('/'))
        .ok_or_else(|| rejection(ToolFailureKind::InvalidInput, "argv"))?;
    let admitted = context.authority.0.iter().any(|grant| {
        grant.effect.as_str() == effect
            && grant
                .scope
                .as_value()
                .get("executables")
                .and_then(Value::as_array)
                .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(name)))
    });
    if !admitted {
        return Err(rejection(ToolFailureKind::Unauthorized, "argv"));
    }
    resolve_program(name).ok_or_else(|| rejection(ToolFailureKind::Unavailable, "argv"))
}

fn resolve_program(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find_map(|candidate| {
            if !candidate.is_absolute() {
                return None;
            }
            let metadata = std::fs::metadata(&candidate).ok()?;
            if !metadata.is_file() {
                return None;
            }
            Some(candidate)
        })
}

fn contained_cwd(context: &ToolContext, requested: Option<&str>) -> Result<PathBuf, ToolFailure> {
    let root = context
        .workspace
        .root
        .as_path()
        .canonicalize()
        .map_err(|_| rejection(ToolFailureKind::InvalidInput, "cwd"))?;
    let relative = requested
        .map(str::to_owned)
        .map(agl_core::agent::RelativePath::try_from)
        .transpose()
        .map_err(|_| rejection(ToolFailureKind::InvalidInput, "cwd"))?
        .unwrap_or_else(|| context.workspace.working_directory.clone());
    let cwd = root
        .join(relative.as_path())
        .canonicalize()
        .map_err(|_| rejection(ToolFailureKind::InvalidInput, "cwd"))?;
    if !cwd.starts_with(&root) {
        return Err(rejection(ToolFailureKind::Unauthorized, "cwd"));
    }
    Ok(cwd)
}

fn receipt(context: &ToolContext, effect: &str) -> Result<EffectReceipt, ToolFailure> {
    context
        .authority
        .0
        .iter()
        .find(|grant| grant.effect.as_str() == effect)
        .map(|grant| EffectReceipt {
            effect: grant.effect.clone(),
            scope: grant.scope.clone(),
        })
        .ok_or_else(|| failure(ToolFailureKind::Unauthorized))
}

fn tool_result(
    value: Value,
    receipt: EffectReceipt,
    result_bytes: u64,
) -> Result<ToolResult, ToolFailure> {
    let text =
        serde_json::to_string(&value).map_err(|_| failure(ToolFailureKind::InvalidResult))?;
    super::committed_result(text, receipt, result_bytes)
}

fn remaining_ms(deadline_at_ms: i64) -> Result<u64, ToolFailure> {
    u64::try_from(deadline_at_ms.saturating_sub(now_ms()))
        .ok()
        .filter(|remaining| *remaining > 0)
        .ok_or_else(|| failure(ToolFailureKind::Deadline))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn client_failure(error: agl_execution_api::ExecutionClientError) -> ToolFailure {
    tracing::debug!(%error, "execution service request failed");
    match error {
        agl_execution_api::ExecutionClientError::Protocol(error)
            if error.code == agl_execution_api::ExecutionProtocolErrorCode::NotFound =>
        {
            failure(ToolFailureKind::InvalidInput)
        }
        agl_execution_api::ExecutionClientError::Protocol(error)
            if error.code == agl_execution_api::ExecutionProtocolErrorCode::Unavailable =>
        {
            failure(ToolFailureKind::Unavailable)
        }
        _ => failure(ToolFailureKind::Execution),
    }
}

fn failure(kind: ToolFailureKind) -> ToolFailure {
    ToolFailure::unknown(kind)
}

fn rejection(kind: ToolFailureKind, field: &str) -> ToolFailure {
    ToolFailure::no_effect(
        kind,
        Some(field),
        &[
            "Use an admitted logical executable and a workspace-relative cwd; correct the rejected field before submitting a new call.",
        ],
    )
}

fn definition_digest(
    definition: &agl_core::agent::ToolDefinition,
) -> agl_core::agent::ToolDefinitionDigest {
    let bytes = serde_json::to_vec(definition).expect("ToolDefinition serializes");
    agl_core::agent::ToolDefinitionDigest::from_bytes(Sha256::digest(bytes).into())
}

fn declaration_view() -> InMemoryPackageView {
    InMemoryPackageView::new([
        embedded(
            "EXTENSION.toml",
            include_bytes!("../../../../../extensions/agentlibre-execution/EXTENSION.toml"),
        ),
        embedded(
            "schemas/execution-scope.json",
            include_bytes!(
                "../../../../../extensions/agentlibre-execution/schemas/execution-scope.json"
            ),
        ),
        embedded(
            "schemas/command-exec.json",
            include_bytes!(
                "../../../../../extensions/agentlibre-execution/schemas/command-exec.json"
            ),
        ),
        embedded(
            "schemas/terminal-session.json",
            include_bytes!(
                "../../../../../extensions/agentlibre-execution/schemas/terminal-session.json"
            ),
        ),
    ])
    .expect("embedded execution package")
}

fn embedded(path: &str, bytes: &[u8]) -> (PackageRelativePath, Vec<u8>) {
    (
        PackageRelativePath::new(path).expect("embedded package path"),
        bytes.to_vec(),
    )
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;
    use std::path::Path;

    use agl_core::agent::{
        AbsolutePath, AgentOperationKey, AuthorityGrantSet, RelativePath, WorkspaceScope,
    };
    use agl_core::{AgentRunId, CanonicalJson, EffectId};
    use agl_execution_api::{
        ExecutionCommand, ExecutionOutcome, ExecutionOutput, ExecutionOutputChunk,
        ExecutionProtocolRequest, ExecutionProtocolResponse, ExecutionResponse,
    };
    use agl_runtime::extension::ToolCancellation;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    use super::*;

    #[test]
    fn stock_execution_bindings_match_the_declarative_package() {
        let bindings = bindings(ExecutionClient::new("/unreachable-test-execd"));
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../extensions/agentlibre-execution");
        let declaration = agl_runtime::package::DirectoryPackageView::new(root).unwrap();
        let package = parse_package_view(&declaration).unwrap();
        assert_eq!(bindings.version, package.manifest.version);
        assert_eq!(bindings.definition, package.definition);
        assert_eq!(
            bindings.content_digest,
            compute_package_digest(&declaration).unwrap()
        );
        assert_eq!(bindings.tools.len(), 2);
        assert!(bindings.definition.tools.iter().all(|definition| {
            bindings.tools.iter().any(|binding| {
                binding.tool_id == definition.id
                    && binding.definition_digest == definition_digest(definition)
            })
        }));
    }

    #[test]
    fn execution_authority_requires_realized_workspace_and_explicit_executables() {
        let grant = |root: &str, executables: Value| agl_core::AuthorityGrant {
            effect: agl_core::EffectId::new(EXEC_EFFECT).unwrap(),
            scope: agl_core::CanonicalJson::new(json!({
                "root": root,
                "executables": executables,
            }))
            .unwrap(),
        };
        assert!(valid_execution_scope(&grant(
            "/workspace",
            json!(["git", "cargo"]),
        )));
        assert!(!valid_execution_scope(&grant("workspace", json!(["sh"]))));
        assert!(!valid_execution_scope(&grant("/workspace", json!([]))));
        assert!(!valid_execution_scope(&grant(
            "/workspace",
            json!(["../sh"]),
        )));
    }

    #[tokio::test]
    async fn planner_write_attempt_is_rejected_by_the_daemon_tool_handler() {
        let root = std::env::temp_dir().join(format!(
            "agl-planner-write-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let target = root.join("planner-must-not-write");
        let mut context = tool_context(
            &root,
            EXEC_EFFECT,
            agl_core::ConversationId::generate(),
            AgentRunId::generate(),
        );
        context.read_only_workspace = true;
        let binding = bindings(ExecutionClient::new(root.join("unreachable-execd")))
            .tools
            .into_iter()
            .find(|binding| binding.tool_id.as_str() == COMMAND_TOOL)
            .unwrap();
        let failure = binding
            .handler
            .call(context, json!({"argv":["touch", target.to_string_lossy()]}))
            .await
            .unwrap_err();
        assert_eq!(failure.kind, ToolFailureKind::Unauthorized);
        assert!(!target.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn stock_handlers_translate_command_and_terminal_calls_to_execution_api() {
        let root = std::env::temp_dir().join(format!(
            "agl-daemon-execution-tools-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let socket = root.join("agl.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
        let conversation_id = agl_core::ConversationId::generate();
        let run_id = AgentRunId::generate();
        let command_id = agl_execution_api::ExecutionId::generate();
        let terminal_execution_id = agl_execution_api::ExecutionId::generate();
        let terminal_id = TerminalId::generate();
        let server = tokio::spawn(mock_execution_server(
            listener,
            conversation_id,
            run_id,
            command_id,
            terminal_execution_id,
            terminal_id,
            b"ok".to_vec(),
        ));
        let client = ExecutionClient::new(&socket);

        let command = command_exec(
            &client,
            tool_context(&root, EXEC_EFFECT, conversation_id, run_id),
            json!({"argv": ["sh", "literal argument"]}),
        )
        .await
        .unwrap();
        let command: Value = serde_json::from_str(command.content.as_text()).unwrap();
        assert_eq!(command["stdout"], "ok");
        assert_eq!(command["outcome"]["type"], "exit");

        let opened = terminal_session(
            &client,
            tool_context(&root, TERMINAL_EFFECT, conversation_id, run_id),
            json!({"action": "open", "argv": ["sh"], "rows": 30, "columns": 100}),
        )
        .await
        .unwrap();
        let opened: Value = serde_json::from_str(opened.content.as_text()).unwrap();
        assert_eq!(opened["terminal_id"], terminal_id.to_string());
        let read = terminal_session(
            &client,
            tool_context(&root, TERMINAL_EFFECT, conversation_id, run_id),
            json!({"action": "read", "terminal_id": terminal_id, "after": 0}),
        )
        .await
        .unwrap();
        let read: Value = serde_json::from_str(read.content.as_text()).unwrap();
        assert_eq!(read["output"], "prompt");
        assert_eq!(read["next"], 6);

        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn committed_command_keeps_receipt_when_output_exceeds_result_or_content_limit() {
        for (budget, output) in [(1024, vec![b'x'; 1024]), (1_048_576, vec![0; 200_000])] {
            let root =
                std::env::temp_dir().join(format!("agl-command-receipt-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(&root).unwrap();
            let socket = root.join("execd.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let conversation_id = agl_core::ConversationId::generate();
            let run_id = AgentRunId::generate();
            let server = tokio::spawn(mock_execution_server(
                listener,
                conversation_id,
                run_id,
                agl_execution_api::ExecutionId::generate(),
                agl_execution_api::ExecutionId::generate(),
                TerminalId::generate(),
                output,
            ));
            let mut context = tool_context(&root, EXEC_EFFECT, conversation_id, run_id);
            context.result_bytes = budget;
            let expected_receipt = receipt(&context, EXEC_EFFECT).unwrap();
            let result = command_exec(
                &ExecutionClient::new(&socket),
                context,
                json!({"argv": ["sh", "literal argument"]}),
            )
            .await
            .unwrap();
            assert_eq!(result.effect_receipts, vec![expected_receipt]);
            assert!(serde_json::to_vec(&result).unwrap().len() as u64 <= budget);
            let notice: Value = serde_json::from_str(result.content.as_text()).unwrap();
            assert_eq!(notice["effect"], "committed");
            assert_eq!(notice["output"], "omitted");
            server.abort();
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn exited_terminal_returns_status_output_and_corrective_actions() {
        for race_after_inspect in [false, true] {
            let root = std::env::temp_dir()
                .join(format!("agl-terminal-lifecycle-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir_all(&root).unwrap();
            let socket = root.join("execd.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let conversation_id = agl_core::ConversationId::generate();
            let run_id = AgentRunId::generate();
            let terminal_id = TerminalId::generate();
            let server = tokio::spawn(mock_exited_terminal_server(
                listener,
                conversation_id,
                run_id,
                terminal_id,
                race_after_inspect,
            ));
            let client = ExecutionClient::new(&socket);

            if !race_after_inspect {
                let read = terminal_session(
                    &client,
                    tool_context(&root, TERMINAL_EFFECT, conversation_id, run_id),
                    json!({"action":"read", "terminal_id":terminal_id}),
                )
                .await
                .unwrap();
                let read: Value = serde_json::from_str(read.content.as_text()).unwrap();
                assert_eq!(read["state"], "exited");
                assert_eq!(read["outcome"]["type"], "exit");
                assert_eq!(read["outcome"]["code"], 7);
                assert_eq!(read["output"], "final output");
                assert!(!read["next_actions"].as_array().unwrap().is_empty());
            }

            let action = if race_after_inspect {
                json!({"action":"write", "terminal_id":terminal_id, "input":"late\n"})
            } else {
                json!({"action":"resize", "terminal_id":terminal_id, "rows":30, "columns":100})
            };
            let failure = terminal_session(
                &client,
                tool_context(&root, TERMINAL_EFFECT, conversation_id, run_id),
                action,
            )
            .await
            .unwrap_err();
            assert_eq!(failure.kind, ToolFailureKind::Unavailable);
            let agl_core::agent::ToolFailureEffect::None {
                field,
                next_actions,
                details,
            } = failure.effect
            else {
                panic!("exited terminal must prove no effect")
            };
            assert_eq!(field.as_deref(), Some("terminal_id"));
            assert!(!next_actions.is_empty());
            let details = details.unwrap().into_value();
            assert_eq!(details["state"], "exited");
            assert_eq!(details["outcome"]["type"], "exit");
            assert_eq!(details["outcome"]["code"], 7);

            server.abort();
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    fn tool_context(
        root: &Path,
        effect: &str,
        conversation_id: agl_core::ConversationId,
        run_id: AgentRunId,
    ) -> ToolContext {
        ToolContext::new(
            AgentOperationKey {
                run_id,
                ordinal: NonZeroU32::MIN,
            },
            Some(conversation_id),
            WorkspaceScope {
                root: AbsolutePath::try_from(root.to_string_lossy().into_owned()).unwrap(),
                working_directory: RelativePath::try_from(".".to_owned()).unwrap(),
            },
            AuthorityGrantSet(vec![agl_core::AuthorityGrant {
                effect: EffectId::new(effect).unwrap(),
                scope: CanonicalJson::new(json!({
                    "root": "workspace",
                    "executables": ["sh"],
                }))
                .unwrap(),
            }]),
            i64::MAX,
            65_536,
            ToolCancellation::new(),
        )
    }

    async fn mock_execution_server(
        listener: tokio::net::UnixListener,
        conversation_id: agl_core::ConversationId,
        run_id: AgentRunId,
        command_id: agl_execution_api::ExecutionId,
        terminal_execution_id: agl_execution_api::ExecutionId,
        terminal_id: TerminalId,
        command_output: Vec<u8>,
    ) {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut line = Vec::new();
            BufReader::new(reader)
                .read_until(b'\n', &mut line)
                .await
                .unwrap();
            let request: ExecutionProtocolRequest = serde_json::from_slice(&line).unwrap();
            let owner = ExecutionOwner::Agent {
                conversation_id,
                run_id,
            };
            let response = match request.command {
                ExecutionCommand::Start { request } if request.io == ExecutionIo::Pipes => {
                    assert_eq!(request.owner, owner);
                    assert_eq!(request.argv[1], "literal argument");
                    ExecutionResponse::Started {
                        status: ExecutionStatus {
                            execution_id: command_id,
                            terminal_id: None,
                            owner,
                            state: ExecutionState::Exited,
                            outcome: Some(ExecutionOutcome::Exit { code: 0 }),
                            output_bytes: command_output.len() as u64,
                            output_truncated: false,
                        },
                    }
                }
                ExecutionCommand::Start { request } => {
                    assert_eq!(request.owner, owner);
                    assert_eq!(
                        request.terminal_size,
                        Some(TerminalSize {
                            rows: 30,
                            columns: 100
                        })
                    );
                    ExecutionResponse::Started {
                        status: ExecutionStatus {
                            execution_id: terminal_execution_id,
                            terminal_id: Some(terminal_id),
                            owner,
                            state: ExecutionState::Running,
                            outcome: None,
                            output_bytes: 6,
                            output_truncated: false,
                        },
                    }
                }
                ExecutionCommand::InspectTerminal {
                    terminal_id: requested,
                } => {
                    assert_eq!(requested, terminal_id);
                    ExecutionResponse::Status {
                        status: ExecutionStatus {
                            execution_id: terminal_execution_id,
                            terminal_id: Some(terminal_id),
                            owner,
                            state: ExecutionState::Running,
                            outcome: None,
                            output_bytes: 6,
                            output_truncated: false,
                        },
                    }
                }
                ExecutionCommand::Read { execution_id, .. } => {
                    let (stream, data) = if execution_id == command_id {
                        (ExecutionOutputStream::Stdout, command_output.clone())
                    } else {
                        assert_eq!(execution_id, terminal_execution_id);
                        (ExecutionOutputStream::Pty, b"prompt".to_vec())
                    };
                    let next = data.len() as u64;
                    ExecutionResponse::Output {
                        output: ExecutionOutput {
                            execution_id,
                            after: 0,
                            next,
                            chunks: vec![ExecutionOutputChunk { stream, data }],
                            eof: execution_id == command_id,
                            truncated: false,
                        },
                    }
                }
                command => panic!("unexpected execution command: {command:?}"),
            };
            let mut response = serde_json::to_vec(&ExecutionProtocolResponse::success(
                request.request_id,
                response,
            ))
            .unwrap();
            response.push(b'\n');
            writer.write_all(&response).await.unwrap();
        }
    }

    async fn mock_exited_terminal_server(
        listener: tokio::net::UnixListener,
        conversation_id: agl_core::ConversationId,
        run_id: AgentRunId,
        terminal_id: TerminalId,
        race_after_inspect: bool,
    ) {
        let execution_id = agl_execution_api::ExecutionId::generate();
        let mut inspections = 0_u8;
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut line = Vec::new();
            BufReader::new(reader)
                .read_until(b'\n', &mut line)
                .await
                .unwrap();
            let request: ExecutionProtocolRequest = serde_json::from_slice(&line).unwrap();
            let request_id = request.request_id.clone();
            let owner = ExecutionOwner::Agent {
                conversation_id,
                run_id,
            };
            let exited = ExecutionStatus {
                execution_id,
                terminal_id: Some(terminal_id),
                owner,
                state: ExecutionState::Exited,
                outcome: Some(ExecutionOutcome::Exit { code: 7 }),
                output_bytes: 12,
                output_truncated: false,
            };
            let response = match request.command {
                ExecutionCommand::InspectTerminal {
                    terminal_id: requested,
                } => {
                    assert_eq!(requested, terminal_id);
                    inspections += 1;
                    let mut status = exited.clone();
                    if race_after_inspect && inspections == 1 {
                        status.state = ExecutionState::Running;
                        status.outcome = None;
                    }
                    ExecutionProtocolResponse::success(
                        request_id,
                        ExecutionResponse::Status { status },
                    )
                }
                ExecutionCommand::Read { .. } => ExecutionProtocolResponse::success(
                    request_id,
                    ExecutionResponse::Output {
                        output: ExecutionOutput {
                            execution_id,
                            after: 0,
                            next: 12,
                            chunks: vec![ExecutionOutputChunk {
                                stream: ExecutionOutputStream::Pty,
                                data: b"final output".to_vec(),
                            }],
                            eof: true,
                            truncated: false,
                        },
                    },
                ),
                ExecutionCommand::Write { .. } if race_after_inspect => {
                    ExecutionProtocolResponse::error(
                        request_id,
                        agl_execution_api::ExecutionProtocolError::new(
                            agl_execution_api::ExecutionProtocolErrorCode::Conflict,
                            "terminal is not active",
                            false,
                        ),
                    )
                }
                command => panic!("unexpected terminal lifecycle command: {command:?}"),
            };
            let mut response = serde_json::to_vec(&response).unwrap();
            response.push(b'\n');
            writer.write_all(&response).await.unwrap();
        }
    }
}
