use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use agl_capabilities::{
    HookBatchRequest, HookBatchResult, HookEvent, HookId, HookInput, HookResult,
};
use anyhow::{Result, bail};

mod hash;
mod process;
mod result;
mod trust;
mod wire;

pub use hash::sha256_file;
pub use trust::ScriptHookTrust;
pub use wire::{SCRIPT_HOOK_INPUT_SCHEMA, SCRIPT_HOOK_RESULT_SCHEMA};

use process::run_process;
use result::fail;
use trust::{ScriptHookTrustError, verify_trust};
use wire::{ScriptHookInput, decode_script_output};

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

        let hook_id = input.hook_id.clone();
        let request = ScriptHookInput::from(input);
        let stdin = match serde_json::to_vec(&request) {
            Ok(mut bytes) => {
                bytes.push(b'\n');
                bytes
            }
            Err(err) => {
                return fail(
                    hook_id,
                    "script_hook.input_serialize_failed",
                    "script hook input could not be serialized",
                    Some(err.to_string()),
                );
            }
        };

        match run_process(&self.command, &self.args, &stdin, self.timeout) {
            Ok(output) => decode_script_output(hook_id.clone(), output),
            Err(err) => fail(hook_id, err.code(), err.message(), Some(err.to_string())),
        }
    }

    fn verify_trust(&self) -> std::result::Result<(), ScriptHookTrustError> {
        verify_trust(&self.command, &self.trust)
    }
}

#[cfg(test)]
mod tests;
