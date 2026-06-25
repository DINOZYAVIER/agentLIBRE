use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub(crate) struct ProcessOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) status: std::process::ExitStatus,
}

#[derive(Debug)]
pub(crate) enum ScriptHookProcessError {
    Spawn { message: String },
    Stdin { message: String },
    Timeout { timeout_ms: u128 },
    NonZeroExit { code: Option<i32>, stderr: String },
    MalformedOutput { message: String },
    SchemaMismatch { schema: String },
}

impl ScriptHookProcessError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Spawn { .. } => "script_hook.spawn_failed",
            Self::Stdin { .. } => "script_hook.stdin_failed",
            Self::Timeout { .. } => "script_hook.timeout",
            Self::NonZeroExit { .. } => "script_hook.nonzero_exit",
            Self::MalformedOutput { .. } => "script_hook.malformed_output",
            Self::SchemaMismatch { .. } => "script_hook.schema_mismatch",
        }
    }

    pub(crate) fn message(&self) -> &'static str {
        match self {
            Self::Spawn { .. } => "script hook process could not be spawned",
            Self::Stdin { .. } => "script hook stdin could not be written",
            Self::Timeout { .. } => "script hook process timed out",
            Self::NonZeroExit { .. } => "script hook process exited unsuccessfully",
            Self::MalformedOutput { .. } => "script hook returned malformed output",
            Self::SchemaMismatch { .. } => "script hook returned an unsupported schema",
        }
    }
}

impl std::fmt::Display for ScriptHookProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Spawn { message } | Self::Stdin { message } => write!(f, "{message}"),
            Self::Timeout { timeout_ms } => write!(f, "timeout after {timeout_ms}ms"),
            Self::NonZeroExit { code, stderr } => {
                write!(f, "exit code {code:?}; stderr bytes={}", stderr.len())
            }
            Self::MalformedOutput { message } => write!(f, "{message}"),
            Self::SchemaMismatch { schema } => write!(f, "unsupported schema `{schema}`"),
        }
    }
}

pub(crate) fn run_process(
    command: &Path,
    args: &[String],
    stdin: &[u8],
    timeout: Duration,
) -> std::result::Result<ProcessOutput, ScriptHookProcessError> {
    let mut child = Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| ScriptHookProcessError::Spawn {
            message: err.to_string(),
        })?;

    if let Some(mut child_stdin) = child.stdin.take() {
        child_stdin
            .write_all(stdin)
            .map_err(|err| ScriptHookProcessError::Stdin {
                message: err.to_string(),
            })?;
    }

    let started = Instant::now();
    loop {
        match child
            .try_wait()
            .map_err(|err| ScriptHookProcessError::Spawn {
                message: err.to_string(),
            })? {
            Some(status) => {
                let output =
                    child
                        .wait_with_output()
                        .map_err(|err| ScriptHookProcessError::Spawn {
                            message: err.to_string(),
                        })?;
                return Ok(ProcessOutput {
                    stdout: output.stdout,
                    stderr: output.stderr,
                    status,
                });
            }
            None if started.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ScriptHookProcessError::Timeout {
                    timeout_ms: timeout.as_millis(),
                });
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
}
