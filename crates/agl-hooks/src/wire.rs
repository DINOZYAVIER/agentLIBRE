use agl_capabilities::{HookEvent, HookId, HookInput, HookMessage, HookResult, HookStatus};
use serde::{Deserialize, Serialize};

use crate::process::{ProcessOutput, ScriptHookProcessError};
use crate::result::fail;

pub const SCRIPT_HOOK_INPUT_SCHEMA: &str = "agentlibre.script_hook_input.v1";
pub const SCRIPT_HOOK_RESULT_SCHEMA: &str = "agentlibre.script_hook_result.v1";

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ScriptHookInput {
    pub(crate) schema: &'static str,
    pub(crate) hook_id: HookId,
    pub(crate) event: HookEvent,
    pub(crate) payload: serde_json::Value,
}

impl From<HookInput> for ScriptHookInput {
    fn from(input: HookInput) -> Self {
        Self {
            schema: SCRIPT_HOOK_INPUT_SCHEMA,
            hook_id: input.hook_id,
            event: input.event,
            payload: input.payload,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct ScriptHookOutput {
    schema: String,
    status: HookStatus,
    #[serde(default)]
    messages: Vec<HookMessage>,
}

pub(crate) fn decode_script_output(hook_id: HookId, output: ProcessOutput) -> HookResult {
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
