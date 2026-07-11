use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use agl_config::{
    BackendKind, InferenceBackendConfig, InferenceRuntimeConfig, LocalInferenceConfig, ModelConfig,
    ModelDialect, MtpRuntimeConfig, PromptConfig, ToolCallFormat,
};
use agl_ids::{AttemptId, RunId, TurnId};
use agl_oven::{RenderedMessage, RenderedMessageRole, RenderedModelRequest};

use crate::evidence::InferenceArtifactRoot;
use crate::{InferenceFinishReason, InferenceRequest};

use super::*;

const RUN_ID: &str = "run_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b31";
const TURN_ID: &str = "turn_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b32";
static ROOT_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
struct FakeState {
    operations: Vec<String>,
    block_generation: bool,
    started_generations: usize,
    panic_on_generate: bool,
}

#[derive(Default)]
struct FakeControl {
    state: Mutex<FakeState>,
    changed: Condvar,
}

impl FakeControl {
    fn set_blocked(&self, blocked: bool) {
        self.state.lock().unwrap().block_generation = blocked;
        self.changed.notify_all();
    }

    fn wait_for_started(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut state = self.state.lock().unwrap();
        while state.started_generations < count {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "fake generation did not start");
            let (next, timeout) = self.changed.wait_timeout(state, remaining).unwrap();
            state = next;
            assert!(!timeout.timed_out() || state.started_generations >= count);
        }
    }

    fn operations(&self) -> Vec<String> {
        self.state.lock().unwrap().operations.clone()
    }
}

struct FakeRuntime {
    control: Arc<FakeControl>,
}

struct FakeModel {
    digest: String,
    control: Arc<FakeControl>,
}

impl Drop for FakeModel {
    fn drop(&mut self) {
        self.control
            .state
            .lock()
            .unwrap()
            .operations
            .push(format!("drop_model:{}", self.digest));
    }
}

struct FakeContext {
    model_digest: String,
    digest: String,
    control: Arc<FakeControl>,
}

impl Drop for FakeContext {
    fn drop(&mut self) {
        self.control.state.lock().unwrap().operations.push(format!(
            "drop_context:{}:{}",
            self.model_digest, self.digest
        ));
    }
}

impl ModelRuntime for FakeRuntime {
    type Model = FakeModel;
    type Context = FakeContext;

    fn load_model(
        &mut self,
        key: &ModelKey,
        _config: &LocalInferenceConfig,
    ) -> Result<RuntimeOperation<Self::Model>, RuntimeFailure> {
        self.control
            .state
            .lock()
            .unwrap()
            .operations
            .push(format!("load_model:{}", key.digest()));
        Ok(RuntimeOperation::new(
            FakeModel {
                digest: key.digest().to_string(),
                control: Arc::clone(&self.control),
            },
            format!("fake model load {}", key.digest()),
        ))
    }

    fn create_context(
        &mut self,
        model: &mut Self::Model,
        job: &InferenceJob,
    ) -> Result<RuntimeOperation<Self::Context>, RuntimeFailure> {
        self.control
            .state
            .lock()
            .unwrap()
            .operations
            .push(format!("create_context:{}", job.context_key().digest()));
        Ok(RuntimeOperation::new(
            FakeContext {
                model_digest: model.digest.clone(),
                digest: job.context_key().digest().to_string(),
                control: Arc::clone(&self.control),
            },
            format!("fake context create {}", job.context_key().digest()),
        ))
    }

    fn generate(
        &mut self,
        _model: &mut Self::Model,
        _context: &mut Self::Context,
        job: &InferenceJob,
    ) -> Result<RuntimeOperation<ModelGeneration>, RuntimeFailure> {
        let attempt = job.request().attempt_id.as_str().to_string();
        let mut state = self.control.state.lock().unwrap();
        state.operations.push(format!("generate:{attempt}"));
        state.started_generations += 1;
        self.control.changed.notify_all();
        while state.block_generation && !job.should_abort() {
            state = self
                .control
                .changed
                .wait_timeout(state, Duration::from_millis(5))
                .unwrap()
                .0;
        }
        let panic_on_generate = state.panic_on_generate;
        drop(state);
        if panic_on_generate {
            panic!("injected fake worker panic");
        }
        if job.should_abort() {
            return Err(RuntimeFailure::new(
                "fake generation aborted",
                format!("fake aborted {attempt}"),
            ));
        }
        Ok(RuntimeOperation::new(
            ModelGeneration {
                content: format!("answer:{attempt}"),
                finish_reason: InferenceFinishReason::Stop,
                selected_device: Some("fake:0".to_string()),
                input_tokens: 4,
                output_tokens: 1,
            },
            format!("fake generate {attempt}"),
        ))
    }

    fn clear_context(
        &mut self,
        _model: &mut Self::Model,
        context: &mut Self::Context,
    ) -> Result<RuntimeOperation<()>, RuntimeFailure> {
        self.control
            .state
            .lock()
            .unwrap()
            .operations
            .push(format!("clear_context:{}", context.digest));
        Ok(RuntimeOperation::without_log(()))
    }
}

#[test]
fn options_and_resource_keys_are_strict_and_load_aware() {
    assert_eq!(ModelManagerOptions::default().max_loaded_models, 1);
    assert_eq!(ModelManagerOptions::default().max_contexts_per_model, 2);
    assert_eq!(ModelManagerOptions::default().queue_capacity, 32);
    assert!(
        ModelManagerOptions {
            queue_capacity: 0,
            ..ModelManagerOptions::default()
        }
        .validate()
        .is_err()
    );

    let first = config("one.gguf");
    let mut context_variant = first.clone();
    context_variant.runtime.context_tokens = 2048;
    context_variant.runtime.threads = 2;
    context_variant.prompt.skills = vec!["different".to_string()];
    assert_eq!(
        ModelKey::from_config(&first).unwrap(),
        ModelKey::from_config(&context_variant).unwrap()
    );
    assert_ne!(
        ContextKey::for_conversation(&first, "session-a").unwrap(),
        ContextKey::for_conversation(&context_variant, "session-a").unwrap()
    );
    let mut prompt_variant = first.clone();
    prompt_variant.prompt.skills = vec!["different".to_string()];
    assert_eq!(
        ContextKey::for_conversation(&first, "session-a").unwrap(),
        ContextKey::for_conversation(&prompt_variant, "session-a").unwrap()
    );

    let second = config("two.gguf");
    assert_ne!(
        ModelKey::from_config(&first).unwrap(),
        ModelKey::from_config(&second).unwrap()
    );
    assert!(ContextKey::for_conversation(&first, " ").is_err());
}

#[test]
fn manager_reuses_weights_and_keeps_conversation_evidence_isolated() {
    let root = temp_root("reuse");
    let control = Arc::new(FakeControl::default());
    let mut manager = manager(ModelManagerOptions::default(), Arc::clone(&control));
    let handle = manager.handle();
    let config = config("shared.gguf");

    let first = handle
        .generate(job(&root, &config, "session-a", 1))
        .unwrap();
    let second = handle
        .generate(job(&root, &config, "session-b", 2))
        .unwrap();
    let third = handle
        .generate(job(&root, &config, "session-a", 3))
        .unwrap();

    assert_eq!(first.metadata.model_state.as_deref(), Some("loaded"));
    assert_eq!(second.metadata.model_state.as_deref(), Some("reused"));
    assert_eq!(third.metadata.model_state.as_deref(), Some("reused"));
    let status = handle.status().unwrap();
    assert_eq!(status.model_loads, 1);
    assert_eq!(status.context_loads, 2);
    assert_eq!(status.cached_contexts, 2);
    assert_eq!(status.completed_jobs, 3);

    let first_log = runtime_log(&root, 1);
    let second_log = runtime_log(&root, 2);
    assert!(first_log.contains(attempt_id(1).as_str()));
    assert!(!first_log.contains(attempt_id(2).as_str()));
    assert!(second_log.contains(attempt_id(2).as_str()));
    assert!(!second_log.contains(attempt_id(1).as_str()));

    manager.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn context_and_model_lru_evict_idle_resources_in_drop_order() {
    let root = temp_root("lru");
    let control = Arc::new(FakeControl::default());
    let options = ModelManagerOptions {
        max_contexts_per_model: 2,
        ..ModelManagerOptions::default()
    };
    let mut manager = manager(options, Arc::clone(&control));
    let handle = manager.handle();
    let first_config = config("first.gguf");

    handle.generate(job(&root, &first_config, "a", 1)).unwrap();
    handle.generate(job(&root, &first_config, "b", 2)).unwrap();
    handle.generate(job(&root, &first_config, "a", 3)).unwrap();
    handle.generate(job(&root, &first_config, "c", 4)).unwrap();
    handle.generate(job(&root, &first_config, "b", 5)).unwrap();
    let status = handle.status().unwrap();
    assert_eq!(status.context_loads, 4);
    assert_eq!(status.context_evictions, 2);

    let first_model = ModelKey::from_config(&first_config).unwrap();
    let second_config = config("second.gguf");
    handle.generate(job(&root, &second_config, "d", 6)).unwrap();
    let status = handle.status().unwrap();
    assert_eq!(status.model_loads, 2);
    assert_eq!(status.model_evictions, 1);
    manager.shutdown().unwrap();

    let operations = control.operations();
    assert_contexts_drop_before_model(&operations, first_model.digest());
    let second_model = ModelKey::from_config(&second_config).unwrap();
    assert_contexts_drop_before_model(&operations, second_model.digest());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn bounded_fifo_queue_rejects_overflow_and_skips_cancelled_jobs() {
    let root = temp_root("queue");
    let control = Arc::new(FakeControl::default());
    control.set_blocked(true);
    let options = ModelManagerOptions {
        queue_capacity: 2,
        ..ModelManagerOptions::default()
    };
    let mut manager = manager(options, Arc::clone(&control));
    let handle = manager.handle();
    let config = config("queue.gguf");

    let first_handle = handle.clone();
    let first_job = job(&root, &config, "a", 1);
    let first = thread::spawn(move || first_handle.generate(first_job));
    control.wait_for_started(1);

    let cancellation = InferenceCancellation::new();
    let second_handle = handle.clone();
    let second_job = job(&root, &config, "b", 2).with_cancellation(cancellation.clone());
    let second = thread::spawn(move || second_handle.generate(second_job));
    wait_for_queue_depth(&handle, 1);

    let third_handle = handle.clone();
    let third_job = job(&root, &config, "c", 3);
    let third = thread::spawn(move || third_handle.generate(third_job));
    wait_for_queue_depth(&handle, 2);

    let overflow = handle.generate(job(&root, &config, "d", 4)).unwrap_err();
    assert_eq!(overflow, ModelManagerError::QueueFull { capacity: 2 });
    assert!(overflow.retryable());

    cancellation.cancel();
    assert_eq!(
        second.join().unwrap().unwrap_err(),
        ModelManagerError::Cancelled
    );
    control.set_blocked(false);
    first.join().unwrap().unwrap();
    third.join().unwrap().unwrap();
    wait_until_idle(&handle);

    let generated: Vec<_> = control
        .operations()
        .into_iter()
        .filter(|operation| operation.starts_with("generate:"))
        .collect();
    assert_eq!(
        generated,
        vec![
            format!("generate:{}", attempt_id(1)),
            format!("generate:{}", attempt_id(3)),
        ]
    );
    assert_eq!(handle.status().unwrap().cancellations, 1);
    manager.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn active_cancellation_and_queued_deadline_are_typed() {
    let root = temp_root("cancel-deadline");
    let control = Arc::new(FakeControl::default());
    control.set_blocked(true);
    let mut manager = manager(ModelManagerOptions::default(), Arc::clone(&control));
    let handle = manager.handle();
    let config = config("cancel.gguf");

    let cancellation = InferenceCancellation::new();
    let active_handle = handle.clone();
    let active_job = job(&root, &config, "active", 1).with_cancellation(cancellation.clone());
    let active = thread::spawn(move || active_handle.generate(active_job));
    control.wait_for_started(1);

    let deadline_handle = handle.clone();
    let deadline_job =
        job(&root, &config, "queued", 2).with_deadline(Instant::now() + Duration::from_millis(30));
    let deadline = thread::spawn(move || deadline_handle.generate(deadline_job));
    wait_for_queue_depth(&handle, 1);
    assert_eq!(
        deadline.join().unwrap().unwrap_err(),
        ModelManagerError::DeadlineExceeded
    );

    cancellation.cancel();
    assert_eq!(
        active.join().unwrap().unwrap_err(),
        ModelManagerError::Cancelled
    );
    control.set_blocked(false);
    wait_until_idle(&handle);
    let operations = control.operations();
    assert!(
        !operations
            .iter()
            .any(|operation| operation == &format!("generate:{}", attempt_id(2)))
    );
    let status = handle.status().unwrap();
    assert_eq!(status.cancellations, 1);
    assert_eq!(status.deadline_exceeded, 1);
    manager.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn clear_release_idle_retention_and_shutdown_are_observable() {
    let root = temp_root("lifecycle");
    let control = Arc::new(FakeControl::default());
    let options = ModelManagerOptions {
        idle_context_retention: Duration::from_millis(15),
        ..ModelManagerOptions::default()
    };
    let mut manager = manager(options, Arc::clone(&control));
    let handle = manager.handle();
    let config = config("lifecycle.gguf");
    let key_a = ContextKey::for_conversation(&config, "a").unwrap();

    handle.generate(job(&root, &config, "a", 1)).unwrap();
    handle.clear_context(&key_a).unwrap();
    assert!(
        control
            .operations()
            .iter()
            .any(|operation| operation == &format!("clear_context:{}", key_a.digest()))
    );
    handle.release_context(&key_a).unwrap();
    assert_eq!(handle.status().unwrap().cached_contexts, 0);

    handle.generate(job(&root, &config, "a", 2)).unwrap();
    thread::sleep(Duration::from_millis(25));
    handle.generate(job(&root, &config, "b", 3)).unwrap();
    let status = handle.status().unwrap();
    assert_eq!(status.cached_contexts, 1);
    assert!(status.context_evictions >= 1);

    manager.shutdown().unwrap();
    wait_for_unavailable(&handle);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn worker_panic_becomes_manager_unavailable() {
    let root = temp_root("panic");
    let control = Arc::new(FakeControl::default());
    control.state.lock().unwrap().panic_on_generate = true;
    let mut manager = manager(ModelManagerOptions::default(), Arc::clone(&control));
    let handle = manager.handle();

    assert_eq!(
        handle
            .generate(job(&root, &config("panic.gguf"), "a", 1))
            .unwrap_err(),
        ModelManagerError::ManagerUnavailable
    );
    wait_for_unavailable(&handle);
    assert_eq!(
        manager.shutdown().unwrap_err(),
        ModelManagerError::ManagerUnavailable
    );
    let _ = std::fs::remove_dir_all(root);
}

fn manager(options: ModelManagerOptions, control: Arc<FakeControl>) -> ModelManager {
    ModelManager::spawn(options, FakeRuntime { control }).unwrap()
}

fn config(model: &str) -> LocalInferenceConfig {
    LocalInferenceConfig {
        backend: InferenceBackendConfig {
            kind: BackendKind::LlamaCpp,
            model: PathBuf::from("/models").join(model),
        },
        runtime: InferenceRuntimeConfig {
            gpu_layers: 0,
            context_tokens: 4096,
            threads: 4,
            device: None,
            batch_size: None,
            ubatch_size: None,
            flash_attention: None,
            cache_type_k: None,
            cache_type_v: None,
            mmap: Some(true),
            kv_unified: None,
            mtp: MtpRuntimeConfig::default(),
        },
        model: ModelConfig {
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
        },
        prompt: PromptConfig::default(),
    }
}

fn job(
    root: &Path,
    config: &LocalInferenceConfig,
    conversation: &str,
    attempt: u64,
) -> InferenceJob {
    let request = InferenceRequest {
        run_id: RunId::parse(RUN_ID).unwrap(),
        turn_id: TurnId::parse(TURN_ID).unwrap(),
        attempt_id: attempt_id(attempt),
        session_id: None,
        request_id: None,
        rendered: RenderedModelRequest {
            run_id: RunId::parse(RUN_ID).unwrap(),
            turn_id: TurnId::parse(TURN_ID).unwrap(),
            request_index: usize::try_from(attempt).unwrap(),
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
            messages: vec![RenderedMessage {
                role: RenderedMessageRole::User,
                content: format!("message {attempt}"),
                name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        },
    };
    InferenceJob::new(
        config.clone(),
        request,
        ContextKey::for_conversation(config, conversation).unwrap(),
        InferenceArtifactRoot::new(root),
        32,
    )
    .unwrap()
}

fn attempt_id(attempt: u64) -> AttemptId {
    AttemptId::parse(&format!("attempt_01890f3b-6d7a-7c1f-b4b5-{attempt:012x}")).unwrap()
}

fn temp_root(name: &str) -> PathBuf {
    let sequence = ROOT_COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "agl-model-manager-{name}-{}-{sequence}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    root
}

fn runtime_log(root: &Path, attempt: u64) -> String {
    std::fs::read_to_string(
        root.join("runs")
            .join(RUN_ID)
            .join("attempts")
            .join(attempt_id(attempt).as_str())
            .join("runtime.log"),
    )
    .unwrap()
}

fn wait_for_queue_depth(handle: &ModelManagerHandle, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if handle.status().unwrap().queue_depth == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "queue depth did not reach {expected}"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn wait_until_idle(handle: &ModelManagerHandle) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let status = handle.status().unwrap();
        if status.queue_depth == 0 && status.active_scope.is_none() {
            return;
        }
        assert!(Instant::now() < deadline, "manager did not become idle");
        thread::sleep(Duration::from_millis(2));
    }
}

fn wait_for_unavailable(handle: &ModelManagerHandle) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if handle.status() == Err(ModelManagerError::ManagerUnavailable) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "manager remained available after worker exit"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn assert_contexts_drop_before_model(operations: &[String], model_digest: &str) {
    let model_drop = operations
        .iter()
        .position(|operation| operation == &format!("drop_model:{model_digest}"))
        .expect("model drop was not observed");
    let context_prefix = format!("drop_context:{model_digest}:");
    assert!(
        operations[..model_drop]
            .iter()
            .any(|operation| operation.starts_with(&context_prefix))
    );
    assert!(
        !operations[model_drop + 1..]
            .iter()
            .any(|operation| operation.starts_with(&context_prefix))
    );
}
