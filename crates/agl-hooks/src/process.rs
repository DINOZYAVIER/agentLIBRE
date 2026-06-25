use std::io::{Error, ErrorKind, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const TEXT_FILE_BUSY_OS_ERROR: i32 = 26;

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
    let mut child = spawn_process(command, args)?;

    if let Some(mut child_stdin) = child.stdin.take() {
        child_stdin
            .write_all(stdin)
            .map_err(|err| ScriptHookProcessError::Stdin {
                message: err.to_string(),
            })?;
    }

    let started = Instant::now();
    loop {
        match try_wait(&mut child)? {
            Some(status) => {
                let stdout = read_pipe(child.stdout.take())?;
                let stderr = read_pipe(child.stderr.take())?;
                return Ok(ProcessOutput {
                    stdout,
                    stderr,
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

fn spawn_process(
    command: &Path,
    args: &[String],
) -> std::result::Result<Child, ScriptHookProcessError> {
    let started = Instant::now();
    loop {
        match Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => return Ok(child),
            Err(err)
                if is_retryable_spawn_error(&err)
                    && started.elapsed() < Duration::from_millis(100) =>
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(err) => {
                return Err(ScriptHookProcessError::Spawn {
                    message: err.to_string(),
                });
            }
        }
    }
}

fn is_retryable_spawn_error(err: &Error) -> bool {
    err.kind() == ErrorKind::Interrupted || err.raw_os_error() == Some(TEXT_FILE_BUSY_OS_ERROR)
}

fn try_wait(child: &mut Child) -> std::result::Result<Option<ExitStatus>, ScriptHookProcessError> {
    loop {
        match child.try_wait() {
            Ok(status) => return Ok(status),
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) => {
                return Err(ScriptHookProcessError::Spawn {
                    message: err.to_string(),
                });
            }
        }
    }
}

fn read_pipe<R: Read>(pipe: Option<R>) -> std::result::Result<Vec<u8>, ScriptHookProcessError> {
    let mut output = Vec::new();
    if let Some(mut pipe) = pipe {
        loop {
            match pipe.read_to_end(&mut output) {
                Ok(_) => break,
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) => {
                    return Err(ScriptHookProcessError::Spawn {
                        message: err.to_string(),
                    });
                }
            }
        }
    }
    Ok(output)
}
