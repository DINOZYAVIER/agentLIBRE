use std::path::{Path, PathBuf};
use std::sync::Arc;

use agl_core::ToolId;
use agl_core::agent::{EffectReceipt, ToolFailure, ToolFailureKind, ToolResult};
use agl_core::execution::*;
use agl_core::package::{InMemoryPackageView, PackageRelativePath};
use agl_tool_api::{
    ExecutionHost, ExtensionBindings, ToolBinding, ToolContext, ToolFuture, ToolHandler,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

const EXEC_EFFECT: &str = "agentlibre.execution:process.execute";
const TERMINAL_EFFECT: &str = "agentlibre.execution:terminal.control";
const COMMAND_TOOL: &str = "agentlibre.execution:command.exec";
const TERMINAL_TOOL: &str = "agentlibre.execution:terminal.session";

pub fn bindings(
    version: agl_core::package::PackageVersion,
    content_digest: agl_core::package::PackageTreeDigest,
    definition: agl_core::ExtensionDefinition,
    client: ExecutionHost,
) -> anyhow::Result<ExtensionBindings> {
    crate::validate_declaration(
        &declaration(),
        &content_digest,
        &definition,
        "agentlibre.execution",
        &[COMMAND_TOOL, TERMINAL_TOOL],
    )?;
    let tools = definition
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
    Ok(ExtensionBindings {
        version,
        content_digest,
        definition,
        tools,
        allows_authority: Arc::new(valid_execution_scope),
    })
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
    client: ExecutionHost,
}

impl ToolHandler for ExecutionTool {
    fn call(&self, context: ToolContext, input: Value) -> ToolFuture {
        let tool_id = self.tool_id.clone();
        let client = self.client.clone();
        Box::pin(async move {
            execution_action(&client, context, input, tool_id.as_str() == TERMINAL_TOOL).await
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Input {
    Start {
        argv: Vec<String>,
        cwd: Option<String>,
        timeout_ms: Option<u64>,
        max_output_bytes: Option<u64>,
    },
    Open {
        argv: Vec<String>,
        cwd: Option<String>,
        timeout_ms: Option<u64>,
        max_output_bytes: Option<u64>,
        rows: Option<u16>,
        columns: Option<u16>,
    },
    Inspect {
        execution_id: ExecutionId,
    },
    Read {
        execution_id: ExecutionId,
        stream: ExecutionOutputStream,
        offset: Option<u64>,
        max_bytes: Option<u32>,
    },
    Wait {
        execution_id: ExecutionId,
        wait_timeout_ms: u64,
    },
    Write {
        execution_id: ExecutionId,
        input: String,
    },
    Resize {
        execution_id: ExecutionId,
        rows: u16,
        columns: u16,
    },
    Interrupt {
        execution_id: ExecutionId,
    },
    Stop {
        execution_id: ExecutionId,
    },
    Cleanup {
        execution_id: ExecutionId,
    },
}
async fn execution_action(
    host: &ExecutionHost,
    context: ToolContext,
    input: Value,
    terminal: bool,
) -> Result<ToolResult, ToolFailure> {
    if context.is_cancelled() {
        return Err(rejection(ToolFailureKind::Cancelled, "input"));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    if now >= context.deadline_at_ms {
        return Err(rejection(ToolFailureKind::Deadline, "input"));
    }
    let input: Input = serde_json::from_value(input)
        .map_err(|_| rejection(ToolFailureKind::InvalidInput, "input"))?;
    let effect = if terminal {
        TERMINAL_EFFECT
    } else {
        EXEC_EFFECT
    };
    let action = match input {
        Input::Start {
            argv,
            cwd,
            timeout_ms,
            max_output_bytes,
        } if !terminal => start(
            &context,
            effect,
            argv,
            cwd,
            timeout_ms,
            max_output_bytes,
            None,
        )?,
        Input::Open {
            argv,
            cwd,
            timeout_ms,
            max_output_bytes,
            rows,
            columns,
        } if terminal => start(
            &context,
            effect,
            argv,
            cwd,
            timeout_ms,
            max_output_bytes,
            Some(TerminalSize {
                rows: rows.unwrap_or(24),
                columns: columns.unwrap_or(80),
            }),
        )?,
        Input::Inspect { execution_id } => ExecutionAction::Inspect(execution_id),
        Input::Read {
            execution_id,
            stream,
            offset,
            max_bytes,
        } => {
            if terminal != (stream == ExecutionOutputStream::Pty) {
                return Err(rejection(ToolFailureKind::InvalidInput, "stream"));
            }
            ExecutionAction::Read {
                execution_id,
                stream,
                offset: offset.unwrap_or(0),
                max_bytes: max_bytes.unwrap_or(65536),
            }
        }
        Input::Wait {
            execution_id,
            wait_timeout_ms,
        } => ExecutionAction::Wait {
            execution_id,
            wait_timeout_ms,
        },
        Input::Write {
            execution_id,
            input,
        } if terminal => {
            if input.is_empty() || input.len() > 65536 {
                return Err(rejection(ToolFailureKind::InvalidInput, "input"));
            }
            ExecutionAction::Write {
                execution_id,
                data: input.into_bytes(),
            }
        }
        Input::Resize {
            execution_id,
            rows,
            columns,
        } if terminal => ExecutionAction::Resize {
            execution_id,
            size: TerminalSize { rows, columns },
        },
        Input::Interrupt { execution_id } if terminal => ExecutionAction::Interrupt(execution_id),
        Input::Stop { execution_id } => ExecutionAction::Stop(execution_id),
        Input::Cleanup { execution_id } => ExecutionAction::Cleanup(execution_id),
        _ => return Err(rejection(ToolFailureKind::InvalidInput, "action")),
    };
    let receipt = receipt(&context, effect)?;
    let limit = context.result_bytes;
    let result = host.call(context, action).await?;
    let value = match result {
        ExecutionResult::Status(status) => {
            serde_json::to_value(status).map_err(|_| failure(ToolFailureKind::InvalidResult))?
        }
        ExecutionResult::Output(output) => {
            json!({"execution_id":output.execution_id,"stream":output.stream,"offset":output.offset,"next":output.next,"output":String::from_utf8_lossy(&output.data),"eof":output.eof,"truncated":output.truncated})
        }
        ExecutionResult::Acknowledged => json!({"acknowledged":true}),
    };
    tool_result(value, receipt, limit)
}
#[allow(clippy::too_many_arguments)]
fn start(
    context: &ToolContext,
    effect: &str,
    mut argv: Vec<String>,
    cwd: Option<String>,
    timeout_ms: Option<u64>,
    max_output_bytes: Option<u64>,
    size: Option<TerminalSize>,
) -> Result<ExecutionAction, ToolFailure> {
    argv[0] = admitted_executable(context, effect, &argv)?
        .to_string_lossy()
        .into_owned();
    let cwd = contained_cwd(context, cwd.as_deref())?;
    let request = ExecutionStartRequest {
        owner: owner(context)?,
        argv,
        cwd: cwd.to_string_lossy().into_owned(),
        // The managed unit inherits current graphical-session variables from
        // the user manager. Keep only the daemon's executable lookup PATH here.
        environment: std::env::var("PATH")
            .ok()
            .map(|path| std::collections::BTreeMap::from([("PATH".into(), path)]))
            .unwrap_or_default(),
        clear_environment: false,
        io: if size.is_some() {
            ExecutionIo::Pty
        } else {
            ExecutionIo::Pipes
        },
        timeout_ms: timeout_ms.unwrap_or(0),
        max_output_bytes: max_output_bytes.unwrap_or(16 * 1024 * 1024),
        terminal_size: size,
        isolation: if context.read_only_workspace {
            ExecutionIsolation::ReadOnlyWorkspace {
                workspace_root: context
                    .workspace
                    .root
                    .as_path()
                    .to_string_lossy()
                    .into_owned(),
                output_directories: context.output_directories.clone(),
            }
        } else {
            ExecutionIsolation::Standard
        },
    };
    request
        .validate()
        .map_err(|_| rejection(ToolFailureKind::InvalidInput, "input"))?;
    Ok(ExecutionAction::Start(request))
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
                .get("root")
                .and_then(Value::as_str)
                .is_some_and(|root| Path::new(root) == context.workspace.root.as_path())
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
        .find(|grant| {
            grant.effect.as_str() == effect
                && grant
                    .scope
                    .as_value()
                    .get("root")
                    .and_then(Value::as_str)
                    .is_some_and(|root| Path::new(root) == context.workspace.root.as_path())
        })
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

pub fn declaration() -> InMemoryPackageView {
    InMemoryPackageView::new([
        embedded(
            "EXTENSION.toml",
            include_bytes!("../../../extensions/agentlibre-execution/EXTENSION.toml"),
        ),
        embedded(
            "schemas/execution-scope.json",
            include_bytes!("../../../extensions/agentlibre-execution/schemas/execution-scope.json"),
        ),
        embedded(
            "schemas/command-exec.json",
            include_bytes!("../../../extensions/agentlibre-execution/schemas/command-exec.json"),
        ),
        embedded(
            "schemas/terminal-session.json",
            include_bytes!(
                "../../../extensions/agentlibre-execution/schemas/terminal-session.json"
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
    use super::*;
    use agl_core::agent::{
        AbsolutePath, AgentOperationKey, AuthorityGrantSet, RelativePath, WorkspaceScope,
    };
    use agl_core::{AgentRunId, AuthorityGrant, CanonicalJson, ConversationId, EffectId};
    use agl_tool_api::ToolCancellation;
    use std::num::NonZeroU32;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn context(root: &Path) -> ToolContext {
        ToolContext::new(
            AgentOperationKey {
                run_id: AgentRunId::generate(),
                ordinal: NonZeroU32::MIN,
            },
            Some(ConversationId::generate()),
            WorkspaceScope {
                root: AbsolutePath::try_from(root.to_string_lossy().into_owned()).unwrap(),
                working_directory: RelativePath::try_from(".".to_owned()).unwrap(),
            },
            AuthorityGrantSet(vec![AuthorityGrant {
                effect: EffectId::new(EXEC_EFFECT).unwrap(),
                scope: CanonicalJson::new(json!({"root":root,"executables":["sh"]})).unwrap(),
            }]),
            i64::MAX,
            65536,
            ToolCancellation::new(),
        )
    }
    #[tokio::test]
    async fn rejected_execution_requests_never_call_the_host() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let host = ExecutionHost::new(move |_, _| {
            count.fetch_add(1, Ordering::Relaxed);
            Box::pin(async { Ok(ExecutionResult::Acknowledged) })
        });
        let start = json!({"action":"start","argv":["sh","-c","true"]});
        let mut expired = context(Path::new("/tmp"));
        expired.deadline_at_ms = 0;
        assert_eq!(
            execution_action(&host, expired, start.clone(), false)
                .await
                .unwrap_err()
                .kind,
            ToolFailureKind::Deadline
        );
        let cancelled = context(Path::new("/tmp"));
        cancelled.cancellation().cancel();
        assert_eq!(
            execution_action(&host, cancelled, start.clone(), false)
                .await
                .unwrap_err()
                .kind,
            ToolFailureKind::Cancelled
        );
        let mut wrong_root = context(Path::new("/tmp"));
        wrong_root.authority.0[0].scope =
            CanonicalJson::new(json!({"root":"/","executables":["sh"]})).unwrap();
        assert_eq!(
            execution_action(&host, wrong_root, start.clone(), false)
                .await
                .unwrap_err()
                .kind,
            ToolFailureKind::Unauthorized
        );
        for input in [
            json!({"action":"start","argv":[]}),
            json!({"action":"start","argv":["/bin/sh"]}),
            json!({"action":"start","argv":["sh"],"cwd":"../"}),
            json!({"action":"open","argv":["sh"]}),
        ] {
            assert!(
                execution_action(&host, context(Path::new("/tmp")), input, false)
                    .await
                    .is_err()
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
    #[tokio::test]
    async fn command_start_preserves_arguments_and_async_defaults() {
        let host = ExecutionHost::new(|_, action| {
            let ExecutionAction::Start(request) = action else {
                panic!("expected Start");
            };
            assert_eq!(request.timeout_ms, 0);
            assert_eq!(request.max_output_bytes, 16 * 1024 * 1024);
            assert!(Path::new(&request.argv[0]).is_absolute());
            assert_eq!(
                &request.argv[1..],
                ["-c", "printf '%s' \"$1\"", "sh", "$HOME ; *"]
            );
            Box::pin(async { Ok(ExecutionResult::Acknowledged) })
        });
        let result = execution_action(
            &host,
            context(Path::new("/tmp")),
            json!({"action":"start","argv":["sh","-c","printf '%s' \"$1\"","sh","$HOME ; *"]}),
            false,
        )
        .await
        .unwrap();
        assert_eq!(result.effect_receipts.len(), 1);
    }
}
