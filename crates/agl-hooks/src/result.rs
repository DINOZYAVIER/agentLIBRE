use agl_capabilities::{HookId, HookMessage, HookResult, HookStatus};

pub(crate) fn fail(hook_id: HookId, code: &str, message: &str, fix: Option<String>) -> HookResult {
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
