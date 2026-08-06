use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use agl_config::{
    BackendKind, InferenceBackendConfig, InferenceRuntimeConfig, ModelConfig, ModelDialect,
    MtpRuntimeConfig, PromptConfig, ResolvedInferenceConfig, ToolCallFormat,
};
use agl_content::{
    ArtifactRetention, ArtifactSensitivity, ArtifactSource, ArtifactSourceKind, Content,
    ContentPart, ImageDimensions, MediaType,
};
use agl_ids::{AttemptId, RunId, TurnId};
use agl_oven::{RenderedMessage, RenderedMessageRole, RenderedModelRequest};

use crate::evidence::InferenceArtifactRoot;
use crate::{
    InferenceDeviceInfo, InferenceDeviceKind, InferenceFinishReason, InferenceOutputEvent,
    InferenceOutputSink, InferenceProductStage, InferenceRequest, OutputDelivery,
};

use super::*;

const RUN_ID: &str = "run_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b31";
const TURN_ID: &str = "turn_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b32";
static ROOT_COUNTER: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone)]
struct ManualManagerClock(Arc<Mutex<Instant>>);

impl ManualManagerClock {
    fn new(now: Instant) -> Self {
        Self(Arc::new(Mutex::new(now)))
    }

    fn advance(&self, duration: Duration) {
        let mut now = self.0.lock().unwrap();
        *now = now.checked_add(duration).unwrap();
    }
}

fn latest_whole_second_instant() -> Instant {
    let now = Instant::now();
    let mut low = 0_u64;
    let mut high = u64::MAX;
    while low < high {
        let middle = low + (high - low) / 2 + 1;
        if now.checked_add(Duration::from_secs(middle)).is_some() {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    now.checked_add(Duration::from_secs(low))
        .expect("the zero duration is always representable")
}

impl super::queue::ManagerClock for ManualManagerClock {
    fn now(&self) -> Instant {
        *self.0.lock().unwrap()
    }
}

#[derive(Default)]
struct FakeState {
    operations: Vec<String>,
    block_generation: bool,
    started_generations: usize,
    panic_on_generate: bool,
    resolved_images: Vec<Vec<u8>>,
    finish_reason: Option<InferenceFinishReason>,
    fail_context_release: bool,
    fail_model_release: bool,
    backend_lost_on_context_release: bool,
    resource_failure_on_load: Option<(String, String)>,
    resource_failure_details_on_load: Option<ResourceAdmissionDetails>,
    resource_admission_on_generate: Option<ResourceAdmissionDetails>,
    reaped_resource_failure_on_generate: Option<(String, String)>,
    inventory_calls: usize,
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

struct RecordingOutputSink {
    events: Mutex<Vec<InferenceOutputEvent>>,
    delivery: OutputDelivery,
}

impl InferenceOutputSink for RecordingOutputSink {
    fn try_emit(&self, event: InferenceOutputEvent) -> OutputDelivery {
        self.events.lock().unwrap().push(event);
        self.delivery
    }
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

    fn device_inventory(&mut self) -> Result<Vec<InferenceDeviceInfo>, RuntimeFailure> {
        let mut state = self.control.state.lock().unwrap();
        state.inventory_calls += 1;
        state.operations.push("device_inventory".to_string());
        Ok(vec![test_device_info()])
    }

    fn load_model(
        &mut self,
        job: &InferenceJob,
    ) -> Result<RuntimeOperation<Self::Model>, RuntimeFailure> {
        let key = job.model_key();
        self.control
            .state
            .lock()
            .unwrap()
            .operations
            .push(format!("load_model:{}", key.digest()));
        let resource_failure = {
            let state = self.control.state.lock().unwrap();
            state
                .resource_failure_on_load
                .clone()
                .map(|failure| (failure, state.resource_failure_details_on_load.clone()))
        };
        if let Some(((code, message), details)) = resource_failure {
            return Err(match details {
                Some(details) => {
                    RuntimeFailure::resource_admission_with_details(code, message, "", details)
                }
                None => RuntimeFailure::resource_admission(code, message, ""),
            });
        }
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
        if let Some(content) = job.resolved_content() {
            for message in content.messages() {
                for part in message.parts() {
                    if let Some((_, bytes)) = part.image() {
                        state.resolved_images.push(bytes.to_vec());
                    }
                }
            }
        }
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
        let finish_reason = state.finish_reason.unwrap_or(InferenceFinishReason::Stop);
        let reaped_resource_failure = state.reaped_resource_failure_on_generate.clone();
        let resource_admission = state.resource_admission_on_generate.clone();
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
        if let Some((code, message)) = reaped_resource_failure {
            return Err(RuntimeFailure::reaped_resource_generation(
                code,
                message,
                "fake worker generation reaped",
            ));
        }
        Ok(RuntimeOperation::new(
            ModelGeneration {
                content: format!("answer:{attempt}"),
                finish_reason,
                selected_device: Some("fake:0".to_string()),
                input_tokens: 4,
                output_tokens: 1,
                resource_admission,
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

    fn release_context(
        &mut self,
        _model: &mut Self::Model,
        context: &mut Self::Context,
    ) -> Result<RuntimeOperation<()>, RuntimeFailure> {
        let mut state = self.control.state.lock().unwrap();
        state
            .operations
            .push(format!("release_context:{}", context.digest));
        if state.backend_lost_on_context_release {
            return Err(RuntimeFailure::backend_lost(
                "fake backend lost during context release",
                "fake backend loss log",
            ));
        }
        if state.fail_context_release {
            return Err(RuntimeFailure::new(
                "fake context release failed",
                "fake context release log",
            ));
        }
        Ok(RuntimeOperation::new((), "fake context release log"))
    }

    fn release_model(
        &mut self,
        model: &mut Self::Model,
    ) -> Result<RuntimeOperation<()>, RuntimeFailure> {
        let mut state = self.control.state.lock().unwrap();
        state
            .operations
            .push(format!("release_model:{}", model.digest));
        if state.fail_model_release {
            return Err(RuntimeFailure::new(
                "fake model release failed",
                "fake model release log",
            ));
        }
        Ok(RuntimeOperation::new((), "fake model release log"))
    }
}

#[test]
fn options_and_resource_keys_are_strict_and_load_aware() {
    assert_eq!(ModelManagerOptions::default().max_loaded_models, 1);
    assert_eq!(ModelManagerOptions::default().max_contexts_per_model, 2);
    assert_eq!(ModelManagerOptions::default().queue_capacity, 32);
    assert_eq!(
        ModelManagerOptions::default().context_idle_duration,
        Duration::from_secs(900)
    );
    assert_eq!(
        ModelManagerOptions::default().model_idle_duration,
        Duration::from_secs(300)
    );
    assert!(ModelManagerOptions::default().model_lease_root.is_none());
    assert!(
        ModelManagerOptions {
            queue_capacity: 0,
            ..ModelManagerOptions::default()
        }
        .validate()
        .is_err()
    );
    for duration in [Duration::ZERO, Duration::from_secs(86_401)] {
        assert!(
            ModelManagerOptions {
                context_idle_duration: duration,
                ..ModelManagerOptions::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            ModelManagerOptions {
                model_idle_duration: duration,
                ..ModelManagerOptions::default()
            }
            .validate()
            .is_err()
        );
    }
    for duration in [Duration::from_secs(1), Duration::from_secs(86_400)] {
        ModelManagerOptions {
            context_idle_duration: duration,
            model_idle_duration: duration,
            ..ModelManagerOptions::default()
        }
        .validate()
        .unwrap();
    }

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
fn checked_residency_deadline_overflow_is_typed_before_native_context_allocation() {
    let root = temp_root("residency-deadline-overflow");
    let control = Arc::new(FakeControl::default());
    let clock = Arc::new(ManualManagerClock::new(latest_whole_second_instant()));
    let mut manager = ModelManager::spawn_for_test(
        ModelManagerOptions {
            context_idle_duration: Duration::from_secs(1),
            model_idle_duration: Duration::from_secs(1),
            ..ModelManagerOptions::default()
        },
        FakeRuntime {
            control: Arc::clone(&control),
        },
        clock,
    )
    .unwrap();
    let handle = manager.handle();
    let config = config("residency-deadline-overflow.gguf");
    let model_key = ModelKey::from_config(&config).unwrap();

    let error = handle
        .generate(job(&root, &config, "overflow", 1))
        .unwrap_err();
    assert!(matches!(
        error,
        ModelManagerError::InvalidOptions { ref message }
            if message == "manager residency deadline overflowed the monotonic clock"
    ));
    wait_for_residency(&handle, 0, 0);
    let operations = control.operations();
    assert!(operations.contains(&format!("load_model:{}", model_key.digest())));
    assert!(
        !operations
            .iter()
            .any(|operation| operation.starts_with("create_context:"))
    );
    assert!(operations.contains(&format!("release_model:{}", model_key.digest())));

    manager.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn worker_job_payload_round_trips_only_after_host_content_resolution() {
    let root = temp_root("worker-payload");
    std::fs::create_dir_all(&root).unwrap();
    let config = config("worker.gguf");
    let mut original = job(&root, &config, "worker-session", 98);

    assert!(original.worker_payload(Instant::now()).is_err());
    original.resolve_content().unwrap();
    let payload = original.worker_payload(Instant::now()).unwrap();
    let bytes = serde_json::to_vec(&payload).unwrap();
    let decoded: WorkerJobPayload = serde_json::from_slice(&bytes).unwrap();
    let restored = InferenceJob::from_worker_payload(
        decoded,
        InferenceCancellation::new(),
        Arc::new(crate::NoopInferenceOutputSink),
        Instant::now(),
    )
    .unwrap();

    assert_eq!(restored.config(), original.config());
    assert_eq!(restored.request(), original.request());
    assert_eq!(restored.model_key(), original.model_key());
    assert_eq!(restored.context_key(), original.context_key());
    assert_eq!(restored.resolved_content(), original.resolved_content());
    assert_eq!(restored.max_output_tokens(), original.max_output_tokens());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn device_inventory_is_serialized_through_the_manager_fifo_and_propagated() {
    let root = temp_root("device-inventory");
    let control = Arc::new(FakeControl::default());
    control.set_blocked(true);
    let mut manager = manager(ModelManagerOptions::default(), Arc::clone(&control));
    let handle = manager.handle();
    let config = config("inventory.gguf");

    let generation_handle = handle.clone();
    let generation_root = root.clone();
    let generation = thread::spawn(move || {
        generation_handle.generate(job(&generation_root, &config, "active", 1))
    });
    control.wait_for_started(1);

    let inventory_handle = handle.clone();
    let inventory = thread::spawn(move || inventory_handle.device_inventory());
    wait_for_queue_depth(&handle, 1);
    assert_eq!(control.state.lock().unwrap().inventory_calls, 0);

    control.set_blocked(false);
    generation.join().unwrap().unwrap();
    assert_eq!(inventory.join().unwrap().unwrap(), [test_device_info()]);
    let state = control.state.lock().unwrap();
    assert_eq!(state.inventory_calls, 1);
    let generation_position = state
        .operations
        .iter()
        .position(|operation| operation.starts_with("generate:"))
        .unwrap();
    let inventory_position = state
        .operations
        .iter()
        .position(|operation| operation == "device_inventory")
        .unwrap();
    assert!(generation_position < inventory_position);
    drop(state);

    manager.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn manager_publishes_host_queue_admission_and_failure_in_order() {
    let root = temp_root("host-stages");
    let control = Arc::new(FakeControl::default());
    control.state.lock().unwrap().resource_failure_on_load = Some((
        "gpu_admission_capacity_exceeded".to_string(),
        "injected admission failure".to_string(),
    ));
    let mut manager = manager(ModelManagerOptions::default(), Arc::clone(&control));
    let sink = Arc::new(RecordingOutputSink {
        events: Mutex::new(Vec::new()),
        delivery: OutputDelivery::Delivered,
    });
    let mut inference_job = job(&root, &config("host-stages.gguf"), "stages", 111);
    let output_sink: Arc<dyn InferenceOutputSink> = sink.clone();
    inference_job.replace_output_sink(output_sink);

    assert!(matches!(
        manager.handle().generate(inference_job),
        Err(ModelManagerError::ResourceAdmission { .. })
    ));
    let stages = sink
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            InferenceOutputEvent::Stage(event) => Some((event.stage_sequence, event.stage)),
            InferenceOutputEvent::TextDelta { .. } => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        stages,
        [
            (1, InferenceProductStage::Queued),
            (2, InferenceProductStage::Admission),
            (3, InferenceProductStage::Failed),
        ]
    );

    manager.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn lagged_presentation_consumer_does_not_fail_generation() {
    let root = temp_root("lagged-presentation");
    let control = Arc::new(FakeControl::default());
    let mut manager = manager(ModelManagerOptions::default(), control);
    let sink = Arc::new(RecordingOutputSink {
        events: Mutex::new(Vec::new()),
        delivery: OutputDelivery::Lagged,
    });
    let mut inference_job = job(&root, &config("lagged.gguf"), "lagged", 112);
    let output_sink: Arc<dyn InferenceOutputSink> = sink.clone();
    inference_job.replace_output_sink(output_sink);

    assert!(manager.handle().generate(inference_job).is_ok());
    assert_eq!(sink.events.lock().unwrap().len(), 1);

    manager.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn loaded_model_holds_a_path_lease_until_shutdown() {
    let root = temp_root("model-lease");
    let lease_root = root.join("leases");
    let control = Arc::new(FakeControl::default());
    let options = ModelManagerOptions::default().with_model_lease_root(&lease_root);
    let mut manager = manager(options, control);
    let model = config("leased.gguf");
    manager
        .handle()
        .generate(job(&root, &model, "lease", 1))
        .unwrap();

    let leases = std::fs::read_dir(&lease_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(leases.len(), 1);
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&leases[0]).unwrap()).unwrap();
    assert_eq!(record["version"], 1);
    assert_eq!(record["paths"][0], "/models/leased.gguf");
    let competing = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&leases[0])
        .unwrap();
    assert!(matches!(
        competing.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));

    manager.shutdown().unwrap();
    assert!(!leases[0].exists());
    let _ = std::fs::remove_dir_all(root);
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
    assert_eq!(status.resident_contexts, 2);
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
fn incomplete_response_has_incomplete_evidence_and_status_accounting() {
    let root = temp_root("incomplete-evidence");
    let control = Arc::new(FakeControl::default());
    control.state.lock().unwrap().finish_reason = Some(InferenceFinishReason::Length);
    let mut manager = manager(ModelManagerOptions::default(), Arc::clone(&control));
    let handle = manager.handle();

    let response = handle
        .generate(job(&root, &config("length.gguf"), "session-a", 1))
        .unwrap();
    assert_eq!(response.finish_reason, InferenceFinishReason::Length);
    let status = handle.status().unwrap();
    assert_eq!(status.completed_jobs, 0);
    assert_eq!(status.incomplete_jobs, 1);
    let status_wire = serde_json::to_value(&status).unwrap();
    assert_eq!(status_wire["completed_jobs"], 0);
    assert_eq!(status_wire["incomplete_jobs"], 1);

    let events = std::fs::read_to_string(root.join("runs").join(RUN_ID).join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<agl_events::SafeRuntimeEventEnvelope>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(matches!(
        events.last().map(|event| &event.payload),
        Some(agl_events::SafeRuntimeEvent::InferenceAttemptFinished {
            finish_status: agl_events::InferenceFinishStatus::IncompleteOutput,
        })
    ));
    assert!(!events.iter().any(|event| matches!(
        event.payload,
        agl_events::SafeRuntimeEvent::InferenceAttemptFinished {
            finish_status: agl_events::InferenceFinishStatus::Succeeded,
        }
    )));

    manager.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn manager_resolves_vision_artifacts_only_for_the_worker_runtime() {
    let root = temp_root("vision-resolution");
    let store = agl_store::AglStore::open_at(&root).unwrap();
    let run_id = RunId::parse(RUN_ID).unwrap();
    store
        .admit_run(&agl_store::DurableRunDraft {
            run_id: run_id.clone(),
            session_id: None,
            turn_id: None,
            kind: agl_store::RunKind::Cron,
            priority: 0,
            concurrency_key: None,
            input: serde_json::json!({}),
            checkpoint: None,
            effective_policy_hash: None,
            execution_context: test_execution_context(),
            budget: agl_store::RunBudget::default(),
            not_before_ms: None,
        })
        .unwrap();
    let private_bytes = b"private fake image bytes";
    let stored = store
        .write_artifact(
            &run_id,
            MediaType::ImagePng,
            private_bytes,
            Some(ImageDimensions::new(2, 2).unwrap()),
            ArtifactSensitivity::Sensitive,
            ArtifactSource {
                kind: ArtifactSourceKind::ScreenCapture,
                extension: Some("fake-portal".to_string()),
            },
            ArtifactRetention::RunScoped,
        )
        .unwrap();
    let mut config = config("vision.gguf");
    config.backend.multimodal_projector = Some(PathBuf::from("/models/mmproj.gguf"));
    let request = InferenceRequest {
        run_id: run_id.clone(),
        turn_id: TurnId::parse(TURN_ID).unwrap(),
        attempt_id: attempt_id(90),
        session_id: None,
        request_id: None,
        rendered: RenderedModelRequest {
            run_id,
            turn_id: TurnId::parse(TURN_ID).unwrap(),
            request_index: 0,
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
            messages: vec![RenderedMessage {
                role: RenderedMessageRole::User,
                content: Some(
                    Content::new([
                        ContentPart::text("what is shown?").unwrap(),
                        ContentPart::artifact(stored.reference),
                    ])
                    .unwrap(),
                ),
                name: None,
                tool_calls: Vec::new(),
            }],
            tools: Vec::new(),
        },
    };
    let inference_job = InferenceJob::new(
        config.clone(),
        request,
        ContextKey::for_conversation(&config, "vision").unwrap(),
        InferenceArtifactRoot::new(&root),
        root.clone(),
        32,
        Arc::new(crate::NoopInferenceOutputSink),
    )
    .unwrap();
    let control = Arc::new(FakeControl::default());
    let mut manager = manager(ModelManagerOptions::default(), Arc::clone(&control));

    manager.handle().generate(inference_job).unwrap();

    assert_eq!(
        control.state.lock().unwrap().resolved_images,
        [private_bytes.to_vec()]
    );
    manager.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

fn test_execution_context() -> agl_process::ExecutionContextSnapshot {
    let workspace = std::env::temp_dir().canonicalize().unwrap();
    agl_process::ExecutionContextSnapshot {
        workspace_root: workspace.clone(),
        working_directory: workspace,
        private_execution_roots: Vec::new(),
        shell: agl_process::ShellProfileSnapshot {
            program: PathBuf::from("/bin/sh"),
            command_args: vec!["-c".to_owned()],
            login_command_args: Some(vec!["-l".to_owned(), "-c".to_owned()]),
            environment_names: vec!["PATH".to_owned()],
            executable_digest: "sha256:test-shell".to_owned(),
            config_digest: "sha256:test-config".to_owned(),
        },
        revision: 1,
        profile_metadata: "workspace".to_owned(),
    }
}

#[test]
fn text_only_profile_rejects_artifact_content_before_queue_admission() {
    let mut request = job(
        &temp_root("unsupported-content"),
        &config("text.gguf"),
        "text",
        91,
    )
    .request()
    .clone();
    let artifact = agl_content::ArtifactRef::new(
        agl_content::ArtifactId::generate(),
        agl_content::BlobDigest::from_bytes(b"image"),
        MediaType::ImagePng,
        5,
        Some(ImageDimensions::new(1, 1).unwrap()),
        ArtifactSensitivity::Sensitive,
        ArtifactSource {
            kind: ArtifactSourceKind::ScreenCapture,
            extension: None,
        },
    )
    .unwrap();
    request.rendered.messages[0].content =
        Some(Content::new([ContentPart::artifact(artifact)]).unwrap());
    let config = config("text.gguf");
    let error = InferenceJob::new(
        config.clone(),
        request,
        ContextKey::for_conversation(&config, "text").unwrap(),
        InferenceArtifactRoot::new("/tmp/unused"),
        PathBuf::from("/tmp/unused-store"),
        32,
        Arc::new(crate::NoopInferenceOutputSink),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        ModelManagerError::UnsupportedContent { .. }
    ));
    assert_eq!(error.code(), "unsupported_content");
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
    wait_for_queue_depth(&handle, 1);

    let replacement_handle = handle.clone();
    let replacement_job = job(&root, &config, "d", 4);
    let replacement = thread::spawn(move || replacement_handle.generate(replacement_job));
    wait_for_queue_depth(&handle, 2);

    control.set_blocked(false);
    first.join().unwrap().unwrap();
    third.join().unwrap().unwrap();
    replacement.join().unwrap().unwrap();
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
            format!("generate:{}", attempt_id(4)),
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
    wait_for_queue_depth(&handle, 0);

    let replacement_handle = handle.clone();
    let replacement_job = job(&root, &config, "replacement", 3);
    let replacement = thread::spawn(move || replacement_handle.generate(replacement_job));
    wait_for_queue_depth(&handle, 1);

    cancellation.cancel();
    assert_eq!(
        active.join().unwrap().unwrap_err(),
        ModelManagerError::Cancelled
    );
    control.set_blocked(false);
    replacement.join().unwrap().unwrap();
    wait_until_idle(&handle);
    let operations = control.operations();
    assert!(
        !operations
            .iter()
            .any(|operation| operation == &format!("generate:{}", attempt_id(2)))
    );
    assert!(
        operations
            .iter()
            .any(|operation| operation == &format!("generate:{}", attempt_id(3)))
    );
    let status = handle.status().unwrap();
    assert_eq!(status.cancellations, 1);
    assert_eq!(status.deadline_exceeded, 1);
    manager.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn shutdown_closes_full_queue_out_of_band_and_releases_all_waiters() {
    let root = temp_root("shutdown-full");
    let control = Arc::new(FakeControl::default());
    control.set_blocked(true);
    let options = ModelManagerOptions {
        queue_capacity: 2,
        ..ModelManagerOptions::default()
    };
    let mut manager = manager(options, Arc::clone(&control));
    let handle = manager.handle();
    let config = config("shutdown.gguf");

    let active_handle = handle.clone();
    let active_job = job(&root, &config, "active", 1);
    let active = thread::spawn(move || active_handle.generate(active_job));
    control.wait_for_started(1);

    let first_pending_handle = handle.clone();
    let first_pending_job = job(&root, &config, "pending-a", 2);
    let first_pending = thread::spawn(move || first_pending_handle.generate(first_pending_job));
    let second_pending_handle = handle.clone();
    let second_pending_job = job(&root, &config, "pending-b", 3);
    let second_pending = thread::spawn(move || second_pending_handle.generate(second_pending_job));
    wait_for_queue_depth(&handle, 2);

    let shutdown_handle = handle.clone();
    let shutdown = thread::spawn(move || shutdown_handle.shutdown());

    assert_eq!(
        first_pending.join().unwrap().unwrap_err(),
        ModelManagerError::Cancelled
    );
    assert_eq!(
        second_pending.join().unwrap().unwrap_err(),
        ModelManagerError::Cancelled
    );
    assert_eq!(
        active.join().unwrap().unwrap_err(),
        ModelManagerError::Cancelled
    );
    shutdown.join().unwrap().unwrap();
    assert_eq!(
        handle.status().unwrap_err(),
        ModelManagerError::ManagerUnavailable
    );
    manager.shutdown().unwrap();

    let generated = control
        .operations()
        .into_iter()
        .filter(|operation| operation.starts_with("generate:"))
        .collect::<Vec<_>>();
    assert_eq!(generated, [format!("generate:{}", attempt_id(1))]);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn clear_release_idle_retention_and_shutdown_are_observable() {
    let root = temp_root("lifecycle");
    let control = Arc::new(FakeControl::default());
    let clock = Arc::new(ManualManagerClock::new(Instant::now()));
    let options = ModelManagerOptions {
        context_idle_duration: Duration::from_secs(1),
        model_idle_duration: Duration::from_secs(1),
        ..ModelManagerOptions::default()
    };
    let mut manager = ModelManager::spawn_for_test(
        options,
        FakeRuntime {
            control: Arc::clone(&control),
        },
        clock.clone(),
    )
    .unwrap();
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
    assert_eq!(handle.status().unwrap().resident_contexts, 0);

    handle.generate(job(&root, &config, "a", 2)).unwrap();
    clock.advance(Duration::from_secs(1));
    handle.wake_for_test();
    wait_for_residency(&handle, 1, 0);
    let status = handle.status().unwrap();
    assert_eq!(status.automatic_context_unloads, 1);
    assert_eq!(status.automatic_model_unloads, 0);

    clock.advance(Duration::from_secs(1));
    handle.wake_for_test();
    wait_for_residency(&handle, 0, 0);
    let status = handle.status().unwrap();
    assert_eq!(status.automatic_model_unloads, 1);

    manager.shutdown().unwrap();
    wait_for_unavailable(&handle);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn manual_unload_is_idempotent_ordered_and_bounded_in_status() {
    let root = temp_root("manual-unload");
    let control = Arc::new(FakeControl::default());
    let options = ModelManagerOptions {
        max_contexts_per_model: 3,
        ..ModelManagerOptions::default()
    };
    let mut manager = manager(options, Arc::clone(&control));
    let handle = manager.handle();
    let config = config("manual-unload.gguf");
    let model_key = ModelKey::from_config(&config).unwrap();
    let context_a = ContextKey::for_conversation(&config, "a").unwrap();
    let context_b = ContextKey::for_conversation(&config, "b").unwrap();

    handle.generate(job(&root, &config, "b", 1)).unwrap();
    handle.generate(job(&root, &config, "a", 2)).unwrap();

    let aggregate = handle.status().unwrap();
    assert_eq!(aggregate.resident_models, 1);
    assert_eq!(aggregate.resident_contexts, 2);
    assert!(aggregate.resident_model_digests.is_empty());
    let detail = handle
        .status_with_detail(ModelManagerStatusDetail::ModelDigests)
        .unwrap();
    assert_eq!(detail.resident_model_digests, [model_key.digest()]);
    assert!(!detail.resident_model_digests_truncated);

    let result = handle
        .unload(ModelUnloadTarget::digest(model_key.digest()).unwrap())
        .unwrap();
    assert_eq!(
        result,
        ModelUnloadResult {
            matched_models: 1,
            released_models: 1,
            released_contexts: 2,
            outcome: ModelUnloadOutcome::Released,
        }
    );
    let operations = control.operations();
    let release_a = operations
        .iter()
        .position(|operation| operation == &format!("release_context:{}", context_a.digest()))
        .unwrap();
    let release_b = operations
        .iter()
        .position(|operation| operation == &format!("release_context:{}", context_b.digest()))
        .unwrap();
    let release_model = operations
        .iter()
        .position(|operation| operation == &format!("release_model:{}", model_key.digest()))
        .unwrap();
    let expected_context_order = if context_a.digest() < context_b.digest() {
        (release_a, release_b)
    } else {
        (release_b, release_a)
    };
    assert!(expected_context_order.0 < expected_context_order.1);
    assert!(expected_context_order.1 < release_model);

    let status = handle.status().unwrap();
    assert_eq!(status.resident_models, 0);
    assert_eq!(status.resident_contexts, 0);
    assert_eq!(status.manual_unloads, 1);
    assert_eq!(status.last_release_reason, Some(ModelReleaseReason::Manual));
    assert_eq!(
        status.last_release_outcome,
        Some(ModelReleaseOutcome::Released)
    );
    assert_eq!(
        handle.unload(ModelUnloadTarget::All).unwrap(),
        ModelUnloadResult::not_resident()
    );

    manager.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn matching_manual_unload_is_busy_while_generation_owns_the_model() {
    let root = temp_root("manual-unload-busy");
    let control = Arc::new(FakeControl::default());
    control.set_blocked(true);
    let mut manager = manager(ModelManagerOptions::default(), Arc::clone(&control));
    let handle = manager.handle();
    let config = config("manual-unload-busy.gguf");
    let model_key = ModelKey::from_config(&config).unwrap();
    let generation_handle = handle.clone();
    let generation_root = root.clone();
    let generation_config = config.clone();
    let generation = thread::spawn(move || {
        generation_handle.generate(job(&generation_root, &generation_config, "active", 1))
    });
    control.wait_for_started(1);

    assert_eq!(
        handle
            .unload(ModelUnloadTarget::digest(model_key.digest()).unwrap())
            .unwrap(),
        ModelUnloadResult::busy()
    );
    assert_eq!(
        handle.unload(ModelUnloadTarget::All).unwrap(),
        ModelUnloadResult::busy()
    );
    assert_eq!(handle.status().unwrap().queue_depth, 0);

    control.set_blocked(false);
    generation.join().unwrap().unwrap();
    assert_eq!(
        handle
            .unload(ModelUnloadTarget::digest(model_key.digest()).unwrap())
            .unwrap()
            .outcome,
        ModelUnloadOutcome::Released
    );

    manager.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn equal_context_deadlines_release_by_digest_before_model_deadline_starts() {
    let root = temp_root("equal-idle-deadlines");
    let control = Arc::new(FakeControl::default());
    let clock = Arc::new(ManualManagerClock::new(Instant::now()));
    let options = ModelManagerOptions {
        max_contexts_per_model: 3,
        context_idle_duration: Duration::from_secs(1),
        model_idle_duration: Duration::from_secs(2),
        ..ModelManagerOptions::default()
    };
    let mut manager = ModelManager::spawn_for_test(
        options,
        FakeRuntime {
            control: Arc::clone(&control),
        },
        clock.clone(),
    )
    .unwrap();
    let handle = manager.handle();
    let config = config("equal-idle-deadlines.gguf");
    let model_key = ModelKey::from_config(&config).unwrap();
    let context_a = ContextKey::for_conversation(&config, "a").unwrap();
    let context_b = ContextKey::for_conversation(&config, "b").unwrap();
    handle.generate(job(&root, &config, "b", 1)).unwrap();
    handle.generate(job(&root, &config, "a", 2)).unwrap();

    clock.advance(Duration::from_secs(1));
    handle.wake_for_test();
    wait_for_residency(&handle, 1, 0);
    let operations = control.operations();
    let release_a = operations
        .iter()
        .position(|operation| operation == &format!("release_context:{}", context_a.digest()))
        .unwrap();
    let release_b = operations
        .iter()
        .position(|operation| operation == &format!("release_context:{}", context_b.digest()))
        .unwrap();
    if context_a.digest() < context_b.digest() {
        assert!(release_a < release_b);
    } else {
        assert!(release_b < release_a);
    }
    assert!(
        !operations
            .iter()
            .any(|operation| operation == &format!("release_model:{}", model_key.digest()))
    );
    assert_eq!(
        handle.status().unwrap().next_residency_deadline_after_ms,
        Some(2_000)
    );

    clock.advance(Duration::from_secs(2));
    handle.wake_for_test();
    wait_for_residency(&handle, 0, 0);
    let operations = control.operations();
    let model_release = operations
        .iter()
        .position(|operation| operation == &format!("release_model:{}", model_key.digest()))
        .unwrap();
    assert!(release_a < model_release);
    assert!(release_b < model_release);

    manager.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn failed_explicit_context_release_keeps_host_resource_and_accounting() {
    let root = temp_root("release-ack");
    let control = Arc::new(FakeControl::default());
    let mut manager = manager(ModelManagerOptions::default(), Arc::clone(&control));
    let handle = manager.handle();
    let config = config("release-ack.gguf");
    let context_key = ContextKey::for_conversation(&config, "kept").unwrap();

    handle.generate(job(&root, &config, "kept", 1)).unwrap();
    control.state.lock().unwrap().fail_context_release = true;
    let error = handle.release_context(&context_key).unwrap_err();

    assert!(matches!(error, ModelManagerError::ContextFailed { .. }));
    let status = handle
        .status_with_detail(ModelManagerStatusDetail::ModelDigests)
        .unwrap();
    assert_eq!(status.resident_contexts, 1);
    assert_eq!(status.context_evictions, 0);
    assert!(
        status
            .resident_model_digests
            .contains(&context_key.model_key().digest().to_string())
    );
    assert!(
        !control
            .operations()
            .iter()
            .any(|operation| operation.starts_with("drop_context:"))
    );

    control.state.lock().unwrap().fail_context_release = false;
    handle.release_context(&context_key).unwrap();
    assert_eq!(handle.status().unwrap().resident_contexts, 0);
    let operations = control.operations();
    let releases = operations
        .iter()
        .filter(|operation| *operation == &format!("release_context:{}", context_key.digest()))
        .count();
    assert_eq!(releases, 2);
    assert!(
        operations
            .iter()
            .any(|operation| operation.ends_with(context_key.digest())
                && operation.starts_with("drop_context:"))
    );

    manager.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn failed_lru_releases_do_not_claim_context_or_model_eviction() {
    let root = temp_root("lru-release-ack");
    let control = Arc::new(FakeControl::default());
    let options = ModelManagerOptions {
        max_contexts_per_model: 1,
        ..ModelManagerOptions::default()
    };
    let mut manager = manager(options, Arc::clone(&control));
    let handle = manager.handle();
    let first_config = config("first-release-ack.gguf");

    handle.generate(job(&root, &first_config, "a", 1)).unwrap();
    control.state.lock().unwrap().fail_context_release = true;
    assert!(matches!(
        handle
            .generate(job(&root, &first_config, "b", 2))
            .unwrap_err(),
        ModelManagerError::ContextFailed { .. }
    ));
    let status = handle.status().unwrap();
    assert_eq!(status.resident_contexts, 1);
    assert_eq!(status.context_loads, 1);
    assert_eq!(status.context_evictions, 0);

    control.state.lock().unwrap().fail_context_release = false;
    handle.generate(job(&root, &first_config, "b", 3)).unwrap();
    assert_eq!(handle.status().unwrap().context_evictions, 1);

    control.state.lock().unwrap().fail_model_release = true;
    let second_config = config("second-release-ack.gguf");
    assert!(matches!(
        handle
            .generate(job(&root, &second_config, "c", 4))
            .unwrap_err(),
        ModelManagerError::LoadFailed { .. }
    ));
    let first_model = ModelKey::from_config(&first_config).unwrap();
    let status = handle
        .status_with_detail(ModelManagerStatusDetail::ModelDigests)
        .unwrap();
    assert_eq!(status.resident_contexts, 0);
    assert_eq!(status.model_evictions, 0);
    assert_eq!(status.context_evictions, 2);
    assert_eq!(status.resident_model_digests, [first_model.digest()]);
    assert!(
        !control
            .operations()
            .iter()
            .any(|operation| operation == &format!("drop_model:{}", first_model.digest()))
    );

    control.state.lock().unwrap().fail_model_release = false;
    handle.generate(job(&root, &second_config, "c", 5)).unwrap();
    assert_eq!(handle.status().unwrap().model_evictions, 1);

    manager.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn backend_loss_during_release_discards_the_whole_generation_without_ack() {
    let root = temp_root("release-backend-loss");
    let control = Arc::new(FakeControl::default());
    let mut manager = manager(ModelManagerOptions::default(), Arc::clone(&control));
    let handle = manager.handle();
    let config = config("backend-loss.gguf");
    let context_key = ContextKey::for_conversation(&config, "lost").unwrap();

    handle.generate(job(&root, &config, "lost", 1)).unwrap();
    control
        .state
        .lock()
        .unwrap()
        .backend_lost_on_context_release = true;
    let error = handle.release_context(&context_key).unwrap_err();

    assert!(matches!(error, ModelManagerError::BackendLost { .. }));
    assert_eq!(error.code(), "manager.backend_lost");
    assert!(!error.retryable());
    let status = handle
        .status_with_detail(ModelManagerStatusDetail::ModelDigests)
        .unwrap();
    assert_eq!(status.resident_contexts, 0);
    assert!(status.resident_model_digests.is_empty());
    assert_eq!(status.context_evictions, 0);
    assert_eq!(status.model_evictions, 0);
    let operations = control.operations();
    assert!(
        operations
            .iter()
            .any(|operation| operation == &format!("release_context:{}", context_key.digest()))
    );
    assert!(
        !operations
            .iter()
            .any(|operation| operation.starts_with("release_model:"))
    );

    manager.shutdown().unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn resource_admission_codes_survive_manager_mapping_and_reaped_generations() {
    let root = temp_root("typed-resource-admission");
    let control = Arc::new(FakeControl::default());
    let mut manager = manager(ModelManagerOptions::default(), Arc::clone(&control));
    let handle = manager.handle();
    let config = config("resource-admission.gguf");

    control.state.lock().unwrap().resource_failure_on_load = Some((
        "accelerator_capacity_exceeded".to_string(),
        "requested profile does not fit".to_string(),
    ));
    let details = ResourceAdmissionDetails {
        selected_profile_id: "gemma4-31b-64k-reviewed".to_string(),
        context_tokens: 65_536,
        model_key: "11".repeat(32),
        context_key: "22".repeat(32),
        snapshot: crate::admission::DeviceMemorySnapshot {
            physical_device_id: "drm-render-128".to_string(),
            driver_id: "amdgpu-test".to_string(),
            total_bytes: 24_000,
            available_bytes: 21_473,
            observed_at_unix_ms: 1_000,
        },
        estimate: crate::admission::AllocationEstimate {
            model_bytes: 16_950,
            context_bytes: 3_850,
            transient_bytes: 320,
            uncertainty_bytes: 256,
        },
        required_bytes: 22_400,
        available_bytes: 21_473,
        reserved_bytes: 0,
        pressure_bytes: 2_527,
        reserve_bytes: 1_024,
        fallback_allowed: false,
        model_load_started: false,
        tool_effect_started: false,
    };
    control
        .state
        .lock()
        .unwrap()
        .resource_failure_details_on_load = Some(details.clone());
    let error = handle
        .generate(job(&root, &config, "capacity", 1))
        .unwrap_err();
    assert!(matches!(error, ModelManagerError::ResourceAdmission { .. }));
    assert_eq!(error.code(), "accelerator_capacity_exceeded");
    assert!(!error.retryable());
    assert_eq!(error.resource_admission_details(), Some(&details));

    control.state.lock().unwrap().resource_failure_on_load = None;
    control
        .state
        .lock()
        .unwrap()
        .resource_failure_details_on_load = None;
    control.state.lock().unwrap().resource_admission_on_generate = Some(details.clone());
    let response = handle.generate(job(&root, &config, "receipt", 2)).unwrap();
    assert_eq!(response.metadata.resource_admission, Some(details));
    control.state.lock().unwrap().resource_admission_on_generate = None;
    control
        .state
        .lock()
        .unwrap()
        .reaped_resource_failure_on_generate = Some((
        "resource_estimate_exceeded".to_string(),
        "worker receipt exceeded its envelope".to_string(),
    ));
    let error = handle
        .generate(job(&root, &config, "receipt", 3))
        .unwrap_err();
    assert!(matches!(error, ModelManagerError::ResourceAdmission { .. }));
    assert_eq!(error.code(), "resource_estimate_exceeded");
    let status = handle
        .status_with_detail(ModelManagerStatusDetail::ModelDigests)
        .unwrap();
    assert!(status.resident_model_digests.is_empty());
    assert_eq!(status.resident_contexts, 0);

    manager.shutdown().unwrap();
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

fn config(model: &str) -> ResolvedInferenceConfig {
    ResolvedInferenceConfig {
        backend: InferenceBackendConfig {
            kind: BackendKind::LlamaCpp,
            model: PathBuf::from("/models").join(model),
            multimodal_projector: None,
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
            structured_decoding: agl_config::StructuredDecodingMode::Auto,
            repair_malformed_tool_calls: true,
            mtp: MtpRuntimeConfig::default(),
        },
        model: ModelConfig {
            dialect: ModelDialect::Qwen3,
            tool_call_format: ToolCallFormat::HermesJson,
        },
        prompt: PromptConfig::default(),
    }
}

fn test_device_info() -> InferenceDeviceInfo {
    InferenceDeviceInfo {
        physical_device_id: "0000:03:00.0".to_string(),
        pci_device_id: Some("1002:744c".to_string()),
        pci_subsystem_id: Some("1da2:471e".to_string()),
        driver_build_id: "sha256:test-driver".to_string(),
        backend_name: "Vulkan0".to_string(),
        description: "Fake GPU".to_string(),
        kind: InferenceDeviceKind::DiscreteGpu,
        free_memory_bytes: 900,
        total_memory_bytes: 1_000,
        usable: true,
        supports_gpu_offload: true,
    }
}

fn job(
    root: &Path,
    config: &ResolvedInferenceConfig,
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
                content: Some(agl_content::Content::text(format!("message {attempt}")).unwrap()),
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
        root.to_path_buf(),
        32,
        Arc::new(crate::NoopInferenceOutputSink),
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

fn wait_for_residency(handle: &ModelManagerHandle, models: usize, contexts: usize) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let status = handle.status().unwrap();
        if status.resident_models == models && status.resident_contexts == contexts {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "manager residency did not converge"
        );
        thread::yield_now();
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
