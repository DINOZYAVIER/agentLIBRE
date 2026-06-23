use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agl_tools::{
    HookBatchRequest, HookBatchResult, HookEvent, HookId, HookInput, HookMessage, HookResult,
    HookStatus,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const SCRIPT_HOOK_INPUT_SCHEMA: &str = "agentlibre.script_hook_input.v1";
pub const SCRIPT_HOOK_RESULT_SCHEMA: &str = "agentlibre.script_hook_result.v1";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptHookRuntime {
    hooks: BTreeMap<HookId, ScriptHook>,
}

impl ScriptHookRuntime {
    pub fn new(hooks: Vec<ScriptHook>) -> Result<Self> {
        let mut indexed = BTreeMap::new();
        for hook in hooks {
            if indexed.insert(hook.hook_id.clone(), hook).is_some() {
                bail!("duplicate script hook id");
            }
        }
        Ok(Self { hooks: indexed })
    }

    pub fn run_batch(&self, request: HookBatchRequest) -> HookBatchResult {
        let results = request
            .hooks
            .iter()
            .map(|hook_id| {
                let input = HookInput {
                    hook_id: hook_id.clone(),
                    event: request.event,
                    payload: request.payload.clone(),
                };
                self.run_hook(input)
            })
            .collect();
        HookBatchResult {
            event: request.event,
            results,
        }
    }

    pub fn run_hook(&self, input: HookInput) -> HookResult {
        let Some(hook) = self.hooks.get(&input.hook_id) else {
            return fail(
                input.hook_id,
                "script_hook.missing",
                "script hook is not registered",
                None,
            );
        };
        hook.run(input)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptHook {
    pub hook_id: HookId,
    pub event: HookEvent,
    pub command: PathBuf,
    pub args: Vec<String>,
    pub timeout: Duration,
    pub trust: ScriptHookTrust,
}

impl ScriptHook {
    pub fn trusted_hash(
        hook_id: HookId,
        event: HookEvent,
        command: impl Into<PathBuf>,
        sha256: impl Into<String>,
    ) -> Self {
        Self {
            hook_id,
            event,
            command: command.into(),
            args: Vec::new(),
            timeout: Duration::from_secs(2),
            trust: ScriptHookTrust::TrustedHash {
                sha256: sha256.into(),
            },
        }
    }

    pub fn with_arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn unsupported(hook_id: HookId, event: HookEvent, command: impl Into<PathBuf>) -> Self {
        Self {
            hook_id,
            event,
            command: command.into(),
            args: Vec::new(),
            timeout: Duration::from_secs(2),
            trust: ScriptHookTrust::Unsupported,
        }
    }

    fn run(&self, input: HookInput) -> HookResult {
        if input.event != self.event {
            return fail(
                input.hook_id,
                "script_hook.event_mismatch",
                "script hook was invoked for the wrong hook event",
                Some(format!(
                    "expected {}, got {}",
                    self.event.as_str(),
                    input.event.as_str()
                )),
            );
        }
        if let Err(err) = self.verify_trust() {
            return fail(
                input.hook_id,
                err.code(),
                err.message(),
                Some(err.to_string()),
            );
        }

        let request = ScriptHookInput {
            schema: SCRIPT_HOOK_INPUT_SCHEMA,
            hook_id: input.hook_id.clone(),
            event: input.event,
            payload: input.payload,
        };
        let stdin = match serde_json::to_vec(&request) {
            Ok(mut bytes) => {
                bytes.push(b'\n');
                bytes
            }
            Err(err) => {
                return fail(
                    input.hook_id,
                    "script_hook.input_serialize_failed",
                    "script hook input could not be serialized",
                    Some(err.to_string()),
                );
            }
        };

        match run_process(&self.command, &self.args, &stdin, self.timeout) {
            Ok(output) => decode_script_output(input.hook_id, output),
            Err(err) => fail(
                input.hook_id,
                err.code(),
                err.message(),
                Some(err.to_string()),
            ),
        }
    }

    fn verify_trust(&self) -> std::result::Result<(), ScriptHookTrustError> {
        match &self.trust {
            ScriptHookTrust::Unsupported => Err(ScriptHookTrustError::Unsupported),
            ScriptHookTrust::TrustedHash { sha256 } => {
                let actual = sha256_file(&self.command).map_err(|err| {
                    ScriptHookTrustError::HashReadFailed {
                        message: err.to_string(),
                    }
                })?;
                if actual == *sha256 {
                    Ok(())
                } else {
                    Err(ScriptHookTrustError::HashMismatch {
                        expected: sha256.clone(),
                        actual,
                    })
                }
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScriptHookTrust {
    TrustedHash { sha256: String },
    Unsupported,
}

#[derive(Debug, Eq, PartialEq)]
enum ScriptHookTrustError {
    Unsupported,
    HashReadFailed { message: String },
    HashMismatch { expected: String, actual: String },
}

impl ScriptHookTrustError {
    fn code(&self) -> &'static str {
        match self {
            Self::Unsupported => "script_hook.untrusted",
            Self::HashReadFailed { .. } => "script_hook.hash_read_failed",
            Self::HashMismatch { .. } => "script_hook.hash_mismatch",
        }
    }

    fn message(&self) -> &'static str {
        match self {
            Self::Unsupported => "script hook is not trusted for execution",
            Self::HashReadFailed { .. } => "script hook hash could not be verified",
            Self::HashMismatch { .. } => "script hook hash does not match trusted value",
        }
    }
}

impl std::fmt::Display for ScriptHookTrustError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported => write!(f, "script hook trust state is unsupported"),
            Self::HashReadFailed { message } => write!(f, "{message}"),
            Self::HashMismatch { expected, actual } => {
                write!(f, "expected sha256 {expected}, got {actual}")
            }
        }
    }
}

#[derive(Debug)]
enum ScriptHookProcessError {
    Spawn { message: String },
    Stdin { message: String },
    Timeout { timeout_ms: u128 },
    NonZeroExit { code: Option<i32>, stderr: String },
    MalformedOutput { message: String },
    SchemaMismatch { schema: String },
}

impl ScriptHookProcessError {
    fn code(&self) -> &'static str {
        match self {
            Self::Spawn { .. } => "script_hook.spawn_failed",
            Self::Stdin { .. } => "script_hook.stdin_failed",
            Self::Timeout { .. } => "script_hook.timeout",
            Self::NonZeroExit { .. } => "script_hook.nonzero_exit",
            Self::MalformedOutput { .. } => "script_hook.malformed_output",
            Self::SchemaMismatch { .. } => "script_hook.schema_mismatch",
        }
    }

    fn message(&self) -> &'static str {
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

#[derive(Clone, Debug, Serialize)]
struct ScriptHookInput {
    schema: &'static str,
    hook_id: HookId,
    event: HookEvent,
    payload: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize)]
struct ScriptHookOutput {
    schema: String,
    status: HookStatus,
    #[serde(default)]
    messages: Vec<HookMessage>,
}

struct ProcessOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: std::process::ExitStatus,
}

fn run_process(
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

fn decode_script_output(hook_id: HookId, output: ProcessOutput) -> HookResult {
    if !output.status.success() {
        return fail(
            hook_id,
            "script_hook.nonzero_exit",
            "script hook process exited unsuccessfully",
            Some(
                ScriptHookProcessError::NonZeroExit {
                    code: output.status.code(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                }
                .to_string(),
            ),
        );
    }

    let decoded = match serde_json::from_slice::<ScriptHookOutput>(&output.stdout) {
        Ok(decoded) => decoded,
        Err(err) => {
            let process_error = ScriptHookProcessError::MalformedOutput {
                message: err.to_string(),
            };
            return fail(
                hook_id,
                process_error.code(),
                process_error.message(),
                Some(process_error.to_string()),
            );
        }
    };
    if decoded.schema != SCRIPT_HOOK_RESULT_SCHEMA {
        let process_error = ScriptHookProcessError::SchemaMismatch {
            schema: decoded.schema,
        };
        return fail(
            hook_id,
            process_error.code(),
            process_error.message(),
            Some(process_error.to_string()),
        );
    }

    HookResult {
        hook_id,
        status: decoded.status,
        messages: decoded.messages,
    }
}

fn fail(hook_id: HookId, code: &str, message: &str, fix: Option<String>) -> HookResult {
    HookResult {
        hook_id,
        status: HookStatus::Fail,
        messages: vec![HookMessage {
            code: code.to_string(),
            message: message.to_string(),
            fix,
        }],
    }
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let bytes =
        std::fs::read(path).with_context(|| format!("failed to read hook {}", path.display()))?;
    Ok(sha256_hex(&bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use agl_tools::HookBatchRequest;
    use serde_json::json;

    use super::*;

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn trusted_script_hook_executes_json_contract() {
        let script = write_script(
            "pass",
            r#"#!/bin/sh
cat >/dev/null
printf '{"schema":"agentlibre.script_hook_result.v1","status":"pass","messages":[]}\n'
"#,
        );
        let runtime = runtime_for_script("local.pass", &script);

        let result = runtime.run_hook(input(
            "local.pass",
            HookEvent::ModelRequest,
            json!({"a": 1}),
        ));

        assert_eq!(result.status, HookStatus::Pass, "{:?}", result.messages);
        assert!(result.messages.is_empty());
    }

    #[test]
    fn hash_mismatch_blocks_before_execution() {
        let marker = temp_path("marker");
        let script = write_script(
            "hash-mismatch",
            &format!(
                r#"#!/bin/sh
touch '{}'
printf '{{"schema":"agentlibre.script_hook_result.v1","status":"pass","messages":[]}}\n'
"#,
                marker.display()
            ),
        );
        let hook = ScriptHook::trusted_hash(
            HookId::new("local.hash").unwrap(),
            HookEvent::ModelRequest,
            script,
            "0000000000000000000000000000000000000000000000000000000000000000",
        );
        let runtime = ScriptHookRuntime::new(vec![hook]).unwrap();

        let result = runtime.run_hook(input("local.hash", HookEvent::ModelRequest, json!({})));

        assert_eq!(result.status, HookStatus::Fail);
        assert_eq!(result.messages[0].code, "script_hook.hash_mismatch");
        assert!(!marker.exists());
    }

    #[test]
    fn unsupported_trust_blocks_execution() {
        let script = write_script("unsupported", "#!/bin/sh\nexit 0\n");
        let hook = ScriptHook::unsupported(
            HookId::new("local.unsupported").unwrap(),
            HookEvent::ModelRequest,
            script,
        );
        let runtime = ScriptHookRuntime::new(vec![hook]).unwrap();

        let result = runtime.run_hook(input(
            "local.unsupported",
            HookEvent::ModelRequest,
            json!({}),
        ));

        assert_eq!(result.status, HookStatus::Fail);
        assert_eq!(result.messages[0].code, "script_hook.untrusted");
    }

    #[test]
    fn nonzero_exit_is_distinguishable() {
        let script = write_script("nonzero", "#!/bin/sh\necho nope >&2\nexit 7\n");
        let runtime = runtime_for_script("local.nonzero", &script);

        let result = runtime.run_hook(input("local.nonzero", HookEvent::ModelRequest, json!({})));

        assert_eq!(result.status, HookStatus::Fail);
        assert_eq!(result.messages[0].code, "script_hook.nonzero_exit");
    }

    #[test]
    fn malformed_output_is_distinguishable() {
        let script = write_script("malformed", "#!/bin/sh\nprintf 'not json\\n'\n");
        let runtime = runtime_for_script("local.malformed", &script);

        let result = runtime.run_hook(input("local.malformed", HookEvent::ModelRequest, json!({})));

        assert_eq!(result.status, HookStatus::Fail);
        assert_eq!(result.messages[0].code, "script_hook.malformed_output");
    }

    #[test]
    fn timeout_kills_child_and_returns_failure() {
        let script = write_script("timeout", "#!/bin/sh\nwhile true; do :; done\n");
        let sha256 = sha256_file(&script).unwrap();
        let hook = ScriptHook::trusted_hash(
            HookId::new("local.timeout").unwrap(),
            HookEvent::ModelRequest,
            script,
            sha256,
        )
        .with_timeout(Duration::from_millis(25));
        let runtime = ScriptHookRuntime::new(vec![hook]).unwrap();

        let result = runtime.run_hook(input("local.timeout", HookEvent::ModelRequest, json!({})));

        assert_eq!(result.status, HookStatus::Fail);
        assert_eq!(result.messages[0].code, "script_hook.timeout");
    }

    #[test]
    fn batch_uses_shared_hook_contract_shape() {
        let script = write_script(
            "batch",
            r#"#!/bin/sh
cat >/dev/null
printf '{"schema":"agentlibre.script_hook_result.v1","status":"warn","messages":[{"code":"local.warn","message":"warned","fix":null}]}\n'
"#,
        );
        let runtime = runtime_for_script("local.batch", &script);

        let result = runtime.run_batch(HookBatchRequest {
            event: HookEvent::ModelRequest,
            hooks: vec![HookId::new("local.batch").unwrap()],
            payload: json!({"ok": true}),
        });

        assert_eq!(result.event, HookEvent::ModelRequest);
        assert_eq!(result.results[0].status, HookStatus::Warn);
        assert_eq!(result.results[0].messages[0].code, "local.warn");
    }

    fn runtime_for_script(id: &str, script: &Path) -> ScriptHookRuntime {
        let sha256 = sha256_file(script).unwrap();
        let hook = ScriptHook::trusted_hash(
            HookId::new(id).unwrap(),
            HookEvent::ModelRequest,
            script,
            sha256,
        );
        ScriptHookRuntime::new(vec![hook]).unwrap()
    }

    fn input(hook_id: &str, event: HookEvent, payload: serde_json::Value) -> HookInput {
        HookInput {
            hook_id: HookId::new(hook_id).unwrap(),
            event,
            payload,
        }
    }

    fn write_script(name: &str, content: &str) -> PathBuf {
        let path = temp_path(name);
        {
            let mut file = std::fs::File::create(&path).unwrap();
            file.write_all(content.as_bytes()).unwrap();
            file.sync_all().unwrap();
        }
        make_executable(&path);
        path
    }

    fn temp_path(name: &str) -> PathBuf {
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("agl-hook-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(not(unix))]
    fn make_executable(_path: &Path) {}
}
