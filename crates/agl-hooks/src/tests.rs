use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use agl_tools::{HookBatchRequest, HookStatus};
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
    let script = write_script(
        "nonzero",
        "#!/bin/sh\ncat >/dev/null\necho nope >&2\nexit 7\n",
    );
    let runtime = runtime_for_script("local.nonzero", &script);

    let result = runtime.run_hook(input("local.nonzero", HookEvent::ModelRequest, json!({})));

    assert_eq!(result.status, HookStatus::Fail, "{result:?}");
    assert_eq!(
        result.messages[0].code, "script_hook.nonzero_exit",
        "{result:?}"
    );
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

    assert_eq!(result.status, HookStatus::Fail, "{result:?}");
    assert_eq!(result.messages[0].code, "script_hook.timeout", "{result:?}");
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
