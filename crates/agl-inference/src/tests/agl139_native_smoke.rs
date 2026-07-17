use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use agl_config::{ResolvedInferenceConfig, load_local_inference_config};
use agl_content::Content;
use agl_ids::{AttemptId, RequestId, RunId, SessionId, TurnId};
use agl_oven::{RenderedMessage, RenderedMessageRole, RenderedModelRequest};
use anyhow::{Context as _, Result, anyhow, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::evidence::InferenceArtifactRoot;
use crate::llama_cpp::NativeAbortTestProbe;
use crate::{
    ContextKey, InferenceCancellation, InferenceJob, InferenceRequest, InferenceResponse,
    LlamaCppModelRuntime, ModelGeneration, ModelKey, ModelManager, ModelManagerError,
    ModelManagerHandle, ModelManagerOptions, ModelManagerStatus, ModelRuntime, RuntimeFailure,
    RuntimeOperation,
};

const SMOKE_SCHEMA: &str = "agentlibre.smoke.agl139.v1";
const SMOKE_TASK_ID: &str = "AGL-139";
const SMOKE_SUMMARY_FILE: &str = "native-manager.json";
const WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const SUCCESS_OUTPUT_TOKENS: u32 = 8;

type NativeModel = <LlamaCppModelRuntime as ModelRuntime>::Model;
type NativeContext = <LlamaCppModelRuntime as ModelRuntime>::Context;

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct GenerationObservation {
    attempt_id: String,
    context_key_digest: String,
    rendered_message_count: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(deny_unknown_fields)]
struct ResourceDropObservation {
    kind: String,
    digest: String,
}

#[derive(Clone, Debug, Default)]
struct NativeObservations {
    model_load_digests: Vec<String>,
    context_create_digests: Vec<String>,
    generations: Vec<GenerationObservation>,
    resource_drops: Vec<ResourceDropObservation>,
}

struct ObservedRuntime {
    inner: LlamaCppModelRuntime,
    observations: Arc<Mutex<NativeObservations>>,
}

struct ObservedModel {
    inner: Option<NativeModel>,
    digest: String,
    observations: Arc<Mutex<NativeObservations>>,
}

struct ObservedContext {
    inner: Option<NativeContext>,
    digest: String,
    observations: Arc<Mutex<NativeObservations>>,
}

impl ModelRuntime for ObservedRuntime {
    type Model = ObservedModel;
    type Context = ObservedContext;

    fn load_model(
        &mut self,
        key: &ModelKey,
        config: &ResolvedInferenceConfig,
    ) -> std::result::Result<RuntimeOperation<Self::Model>, RuntimeFailure> {
        let RuntimeOperation { value, log } = self.inner.load_model(key, config)?;
        lock_observations(&self.observations)
            .model_load_digests
            .push(key.digest().to_string());
        Ok(RuntimeOperation::new(
            ObservedModel {
                inner: Some(value),
                digest: key.digest().to_string(),
                observations: Arc::clone(&self.observations),
            },
            log,
        ))
    }

    fn create_context(
        &mut self,
        model: &mut Self::Model,
        job: &InferenceJob,
    ) -> std::result::Result<RuntimeOperation<Self::Context>, RuntimeFailure> {
        let Some(native_model) = model.inner.as_mut() else {
            return Err(RuntimeFailure::new(
                "observed native model was already dropped",
                "",
            ));
        };
        let RuntimeOperation { value, log } = self.inner.create_context(native_model, job)?;
        let digest = job.context_key().digest().to_string();
        lock_observations(&self.observations)
            .context_create_digests
            .push(digest.clone());
        Ok(RuntimeOperation::new(
            ObservedContext {
                inner: Some(value),
                digest,
                observations: Arc::clone(&self.observations),
            },
            log,
        ))
    }

    fn generate(
        &mut self,
        model: &mut Self::Model,
        context: &mut Self::Context,
        job: &InferenceJob,
    ) -> std::result::Result<RuntimeOperation<ModelGeneration>, RuntimeFailure> {
        lock_observations(&self.observations)
            .generations
            .push(GenerationObservation {
                attempt_id: job.request().attempt_id.to_string(),
                context_key_digest: job.context_key().digest().to_string(),
                rendered_message_count: job.request().rendered.messages.len(),
            });
        let Some(native_model) = model.inner.as_mut() else {
            return Err(RuntimeFailure::new(
                "observed native model was already dropped",
                "",
            ));
        };
        let Some(native_context) = context.inner.as_mut() else {
            return Err(RuntimeFailure::new(
                "observed native context was already dropped",
                "",
            ));
        };
        self.inner.generate(native_model, native_context, job)
    }

    fn clear_context(
        &mut self,
        model: &mut Self::Model,
        context: &mut Self::Context,
    ) -> std::result::Result<RuntimeOperation<()>, RuntimeFailure> {
        let Some(native_model) = model.inner.as_mut() else {
            return Err(RuntimeFailure::new(
                "observed native model was already dropped",
                "",
            ));
        };
        let Some(native_context) = context.inner.as_mut() else {
            return Err(RuntimeFailure::new(
                "observed native context was already dropped",
                "",
            ));
        };
        self.inner.clear_context(native_model, native_context)
    }
}

impl Drop for ObservedContext {
    fn drop(&mut self) {
        drop(self.inner.take());
        lock_observations(&self.observations)
            .resource_drops
            .push(ResourceDropObservation {
                kind: "context".to_string(),
                digest: self.digest.clone(),
            });
    }
}

impl Drop for ObservedModel {
    fn drop(&mut self) {
        drop(self.inner.take());
        lock_observations(&self.observations)
            .resource_drops
            .push(ResourceDropObservation {
                kind: "model".to_string(),
                digest: self.digest.clone(),
            });
    }
}

fn lock_observations(
    observations: &Mutex<NativeObservations>,
) -> std::sync::MutexGuard<'_, NativeObservations> {
    observations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone)]
struct SmokeAttemptRecord {
    label: &'static str,
    run_id: RunId,
    turn_id: TurnId,
    attempt_id: AttemptId,
    context_key_digest: String,
}

struct PreparedSmokeJob {
    job: InferenceJob,
    record: SmokeAttemptRecord,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct SmokeSummary {
    schema: &'static str,
    task_id: &'static str,
    outcome: &'static str,
    model_key_digest: String,
    config_digest: String,
    context_key_digests: Vec<String>,
    attempts: Vec<SmokeAttemptSummary>,
    counters: SmokeCounterSummary,
    admission: SmokeAdmissionSummary,
    native_abort: SmokeNativeAbortSummary,
    runtime_observations: SmokeRuntimeObservationSummary,
    resource_lifecycle: SmokeResourceLifecycleSummary,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct SmokeAttemptSummary {
    label: &'static str,
    run_id: RunId,
    turn_id: TurnId,
    attempt_id: AttemptId,
    context_key_digest: String,
    evidence_started: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    events_ref: Option<String>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct SmokeCounterSummary {
    model_loads: u64,
    context_loads: u64,
    cached_contexts_before_shutdown: usize,
    completed_jobs: u64,
    cancellations: u64,
    deadline_exceeded: u64,
    failures: u64,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct SmokeAdmissionSummary {
    queue_capacity: usize,
    queued_depth_observed: usize,
    depth_after_queued_cancel: usize,
    replacement_depth_before_active_release: usize,
    queued_cancel_reclaimed_capacity: bool,
    replacement_admitted_while_active: bool,
    queued_attempt_never_started: bool,
    queued_attempt_has_no_evidence: bool,
    replacement_succeeded: bool,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct SmokeNativeAbortSummary {
    callback_installations: usize,
    callback_calls: usize,
    aborting_callback_calls: usize,
    install_wait_timed_out: bool,
    active_attempt_cancelled: bool,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct SmokeRuntimeObservationSummary {
    model_load_digests: Vec<String>,
    context_create_digests: Vec<String>,
    generation_attempt_ids: Vec<String>,
    rendered_message_counts_by_context: BTreeMap<String, Vec<usize>>,
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct SmokeResourceLifecycleSummary {
    drops: Vec<ResourceDropObservation>,
    all_contexts_dropped_before_model: bool,
}

#[test]
#[ignore = "requires AGL_LOCAL_INFERENCE_CONFIG, AGL_INFERENCE_ARTIFACT_ROOT, and AGL_STORE_ROOT"]
fn manual_llama_cpp_smoke_from_env() -> Result<()> {
    let config_path = PathBuf::from(std::env::var("AGL_LOCAL_INFERENCE_CONFIG")?);
    let artifact_root_path = PathBuf::from(std::env::var("AGL_INFERENCE_ARTIFACT_ROOT")?);
    let store_root = PathBuf::from(std::env::var("AGL_STORE_ROOT")?);
    let config = load_local_inference_config(&config_path)?;
    let summary_path = smoke_summary_path();
    remove_stale_summary(&summary_path)?;

    let artifact_root = InferenceArtifactRoot::new(&artifact_root_path);
    let model_key = ModelKey::from_config(&config)?;
    let session_a = SessionId::generate();
    let session_b = SessionId::generate();
    let context_a = ContextKey::for_conversation(&config, session_a.as_str())?;
    let context_b = ContextKey::for_conversation(&config, session_b.as_str())?;
    ensure!(context_a != context_b, "native smoke context keys collided");

    let observations = Arc::new(Mutex::new(NativeObservations::default()));
    let runtime = ObservedRuntime {
        inner: LlamaCppModelRuntime::new(),
        observations: Arc::clone(&observations),
    };
    let options = ModelManagerOptions {
        queue_capacity: 1,
        max_contexts_per_model: 2,
        ..ModelManagerOptions::default()
    };
    let mut manager = ModelManager::spawn(options, runtime)?;
    let handle = manager.handle();

    let initial_a = vec![text_message(
        RenderedMessageRole::User,
        "Give one short sentence about a red circle.",
    )?];
    let initial_b = vec![text_message(
        RenderedMessageRole::User,
        "Give one short sentence about a blue square.",
    )?];
    let warm_a = prepare_job(
        "warm_a",
        &config,
        context_a.clone(),
        session_a.clone(),
        initial_a.clone(),
        &artifact_root,
        &store_root,
        0,
        SUCCESS_OUTPUT_TOKENS,
        InferenceCancellation::new(),
    )?;
    let warm_a_record = warm_a.record.clone();
    let warm_a_response = handle.generate(warm_a.job)?;
    ensure!(
        !warm_a_response.content.trim().is_empty(),
        "first native context returned empty text"
    );

    let warm_b = prepare_job(
        "warm_b",
        &config,
        context_b.clone(),
        session_b.clone(),
        initial_b.clone(),
        &artifact_root,
        &store_root,
        0,
        SUCCESS_OUTPUT_TOKENS,
        InferenceCancellation::new(),
    )?;
    let warm_b_record = warm_b.record.clone();
    let warm_b_response = handle.generate(warm_b.job)?;
    ensure!(
        !warm_b_response.content.trim().is_empty(),
        "second native context returned empty text"
    );

    let warm_status = handle.status()?;
    ensure!(warm_status.model_loads == 1, "native model was not reused");
    ensure!(
        warm_status.context_loads == 2 && warm_status.cached_contexts == 2,
        "native smoke did not retain two independent contexts"
    );

    let active_history = followup_history(
        &initial_a,
        &warm_a_response,
        "Continue that same context with one more sentence.",
    )?;
    let queued_history = followup_history(
        &initial_b,
        &warm_b_response,
        "Continue that different context with one more sentence.",
    )?;
    let active_cancellation = InferenceCancellation::new();
    let active = prepare_job(
        "active_cancel",
        &config,
        context_a.clone(),
        session_a,
        active_history,
        &artifact_root,
        &store_root,
        1,
        4096,
        active_cancellation.clone(),
    )?;
    let active_record = active.record.clone();
    let queued_cancellation = InferenceCancellation::new();
    let queued = prepare_job(
        "queued_cancel",
        &config,
        context_b.clone(),
        session_b.clone(),
        queued_history.clone(),
        &artifact_root,
        &store_root,
        1,
        SUCCESS_OUTPUT_TOKENS,
        queued_cancellation.clone(),
    )?;
    let queued_record = queued.record.clone();
    let replacement = prepare_job(
        "replacement",
        &config,
        context_b.clone(),
        session_b,
        queued_history,
        &artifact_root,
        &store_root,
        1,
        SUCCESS_OUTPUT_TOKENS,
        InferenceCancellation::new(),
    )?;
    let replacement_record = replacement.record.clone();

    let (probe, _probe_registration) =
        NativeAbortTestProbe::register().map_err(|message| anyhow!(message))?;
    let active_thread = spawn_generation(handle.clone(), active.job);
    if !probe.wait_for_install(WAIT_TIMEOUT) {
        active_cancellation.cancel();
        let _ = join_generation(active_thread);
        let _ = manager.shutdown();
        return Err(anyhow!(
            "native abort callback was not installed before the smoke timeout"
        ));
    }

    let queued_thread = spawn_generation(handle.clone(), queued.job);
    let queued_status = wait_for_queue_depth(&handle, 1, WAIT_TIMEOUT);
    queued_cancellation.cancel();
    let reclaimed_status = wait_for_queue_depth(&handle, 0, WAIT_TIMEOUT);
    let queued_result = join_generation(queued_thread)?;

    let replacement_thread = spawn_generation(handle.clone(), replacement.job);
    let replacement_queued_status = wait_for_queue_depth(&handle, 1, WAIT_TIMEOUT);
    active_cancellation.cancel();
    let active_result = join_generation(active_thread)?;
    let replacement_result = join_generation(replacement_thread)?;
    let final_status = handle.status()?;
    manager.shutdown()?;
    let observed = lock_observations(&observations).clone();

    let queued_cancel_reclaimed_capacity = queued_status.is_some()
        && reclaimed_status.is_some()
        && matches!(queued_result, Err(ModelManagerError::Cancelled));
    let replacement_admitted_while_active = replacement_queued_status.is_some();
    let active_attempt_cancelled = matches!(active_result, Err(ModelManagerError::Cancelled));
    let replacement_succeeded = replacement_result.is_ok();
    let queued_attempt_never_started = observed
        .generations
        .iter()
        .all(|entry| entry.attempt_id != queued_record.attempt_id.as_str());
    let queued_attempt_has_no_evidence = !artifact_root
        .paths(&queued_record.run_id, &queued_record.attempt_id)
        .attempt_dir()
        .exists();

    validate_smoke_outcomes(
        &final_status,
        &model_key,
        &context_a,
        &context_b,
        &observed,
        queued_cancel_reclaimed_capacity,
        replacement_admitted_while_active,
        active_attempt_cancelled,
        replacement_succeeded,
        queued_attempt_never_started,
        queued_attempt_has_no_evidence,
        &probe,
    )?;

    let attempts = vec![
        attempt_summary(&warm_a_record, true),
        attempt_summary(&warm_b_record, true),
        attempt_summary(&active_record, true),
        attempt_summary(&queued_record, false),
        attempt_summary(&replacement_record, true),
    ];
    validate_attempt_evidence(&artifact_root, &attempts)?;
    let summary = build_summary(
        &config,
        &model_key,
        &context_a,
        &context_b,
        attempts,
        &final_status,
        &observed,
        &probe,
        queued_cancel_reclaimed_capacity,
        replacement_admitted_while_active,
        active_attempt_cancelled,
        replacement_succeeded,
        queued_attempt_never_started,
        queued_attempt_has_no_evidence,
    )?;
    let bytes = serde_json::to_vec_pretty(&summary)?;
    validate_safe_summary(
        &bytes,
        &config,
        &artifact_root_path,
        &store_root,
        [
            "Give one short sentence about a red circle.",
            "Give one short sentence about a blue square.",
            "Continue that same context with one more sentence.",
            "Continue that different context with one more sentence.",
            warm_a_response.content.as_str(),
            warm_b_response.content.as_str(),
        ],
    )?;
    write_summary(&summary_path, &bytes)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn prepare_job(
    label: &'static str,
    config: &ResolvedInferenceConfig,
    context_key: ContextKey,
    session_id: SessionId,
    messages: Vec<RenderedMessage>,
    artifact_root: &InferenceArtifactRoot,
    store_root: &Path,
    request_index: usize,
    max_output_tokens: u32,
    cancellation: InferenceCancellation,
) -> Result<PreparedSmokeJob> {
    let run_id = RunId::generate();
    let turn_id = TurnId::generate();
    let attempt_id = AttemptId::generate();
    let request = InferenceRequest {
        run_id: run_id.clone(),
        turn_id: turn_id.clone(),
        attempt_id: attempt_id.clone(),
        session_id: Some(session_id),
        request_id: Some(RequestId::generate()),
        rendered: RenderedModelRequest {
            run_id: run_id.clone(),
            turn_id: turn_id.clone(),
            request_index,
            dialect: config.model.dialect,
            tool_call_format: config.model.tool_call_format,
            messages,
            tools: Vec::new(),
        },
    };
    let job = InferenceJob::new(
        config.clone(),
        request,
        context_key.clone(),
        artifact_root.clone(),
        store_root.to_path_buf(),
        max_output_tokens,
    )?
    .with_cancellation(cancellation);
    Ok(PreparedSmokeJob {
        job,
        record: SmokeAttemptRecord {
            label,
            run_id,
            turn_id,
            attempt_id,
            context_key_digest: context_key.digest().to_string(),
        },
    })
}

fn text_message(role: RenderedMessageRole, value: impl Into<String>) -> Result<RenderedMessage> {
    Ok(RenderedMessage {
        role,
        content: Some(Content::text(value.into())?),
        name: None,
        tool_calls: Vec::new(),
    })
}

fn followup_history(
    initial: &[RenderedMessage],
    response: &InferenceResponse,
    followup: &str,
) -> Result<Vec<RenderedMessage>> {
    let mut history = initial.to_vec();
    history.push(text_message(
        RenderedMessageRole::Assistant,
        response.content.clone(),
    )?);
    history.push(text_message(RenderedMessageRole::User, followup)?);
    Ok(history)
}

fn spawn_generation(
    handle: ModelManagerHandle,
    job: InferenceJob,
) -> JoinHandle<std::result::Result<InferenceResponse, ModelManagerError>> {
    thread::spawn(move || handle.generate(job))
}

fn join_generation(
    worker: JoinHandle<std::result::Result<InferenceResponse, ModelManagerError>>,
) -> Result<std::result::Result<InferenceResponse, ModelManagerError>> {
    worker
        .join()
        .map_err(|_| anyhow!("native smoke generation thread panicked"))
}

fn wait_for_queue_depth(
    handle: &ModelManagerHandle,
    expected: usize,
    timeout: Duration,
) -> Option<ModelManagerStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match handle.status() {
            Ok(status) if status.queue_depth == expected => return Some(status),
            Ok(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(_) | Err(_) => return None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_smoke_outcomes(
    status: &ModelManagerStatus,
    model_key: &ModelKey,
    context_a: &ContextKey,
    context_b: &ContextKey,
    observed: &NativeObservations,
    queued_cancel_reclaimed_capacity: bool,
    replacement_admitted_while_active: bool,
    active_attempt_cancelled: bool,
    replacement_succeeded: bool,
    queued_attempt_never_started: bool,
    queued_attempt_has_no_evidence: bool,
    probe: &NativeAbortTestProbe,
) -> Result<()> {
    ensure!(
        queued_cancel_reclaimed_capacity,
        "queued cancellation did not reclaim capacity"
    );
    ensure!(
        replacement_admitted_while_active,
        "replacement was not admitted while the cancelled generation remained active"
    );
    ensure!(
        active_attempt_cancelled,
        "active generation was not cancelled"
    );
    ensure!(
        replacement_succeeded,
        "replacement generation did not succeed"
    );
    ensure!(
        queued_attempt_never_started,
        "queued cancelled work reached the native runtime"
    );
    ensure!(
        queued_attempt_has_no_evidence,
        "queued cancelled work started attempt evidence"
    );
    ensure!(
        probe.installed() == 1,
        "unexpected native abort callback installation count"
    );
    ensure!(
        probe.callback_calls() > 0,
        "native abort callback was never invoked"
    );
    ensure!(
        probe.aborting_callback_calls() > 0,
        "native abort callback never observed cancellation"
    );
    ensure!(
        !probe.install_wait_timed_out(),
        "native abort probe timed out"
    );
    ensure!(
        status.queue_depth == 0 && status.active_scope.is_none(),
        "manager did not become idle"
    );
    ensure!(
        status.model_loads == 1,
        "manager loaded the configured model more than once"
    );
    ensure!(
        status.context_loads == 2,
        "manager did not create exactly two contexts"
    );
    ensure!(
        status.cached_contexts == 1,
        "active cancellation did not invalidate only its context"
    );
    ensure!(
        status.completed_jobs == 3,
        "unexpected successful job count"
    );
    ensure!(status.cancellations == 2, "cancellation accounting drifted");
    ensure!(
        status.deadline_exceeded == 0 && status.failures == 0,
        "unexpected failure accounting"
    );
    ensure!(
        status.loaded_model_digests == [model_key.digest().to_string()],
        "loaded-model status does not match the configured model"
    );
    ensure!(
        observed.model_load_digests == [model_key.digest().to_string()],
        "native runtime observed an unexpected model load sequence"
    );
    ensure!(
        observed.context_create_digests.len() == 2
            && observed
                .context_create_digests
                .contains(&context_a.digest().to_string())
            && observed
                .context_create_digests
                .contains(&context_b.digest().to_string()),
        "native runtime did not observe both independent contexts"
    );
    ensure!(
        all_contexts_dropped_before_model(&observed.resource_drops),
        "native resources did not drop contexts before the model"
    );
    Ok(())
}

fn attempt_summary(record: &SmokeAttemptRecord, evidence_started: bool) -> SmokeAttemptSummary {
    SmokeAttemptSummary {
        label: record.label,
        run_id: record.run_id.clone(),
        turn_id: record.turn_id.clone(),
        attempt_id: record.attempt_id.clone(),
        context_key_digest: record.context_key_digest.clone(),
        evidence_started,
        events_ref: evidence_started
            .then(|| format!("runs/{}/events.jsonl", record.run_id.as_str())),
    }
}

fn validate_attempt_evidence(
    artifact_root: &InferenceArtifactRoot,
    attempts: &[SmokeAttemptSummary],
) -> Result<()> {
    for attempt in attempts {
        let paths = artifact_root.paths(&attempt.run_id, &attempt.attempt_id);
        if attempt.evidence_started {
            ensure!(
                paths.events_jsonl().is_file(),
                "started smoke attempt has no event stream"
            );
            ensure!(
                paths.request_json().is_file(),
                "started smoke attempt has no request evidence"
            );
            ensure!(
                paths.runtime_log().is_file(),
                "started smoke attempt has no runtime evidence"
            );
        } else {
            ensure!(
                !paths.attempt_dir().exists(),
                "non-started smoke attempt wrote evidence"
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_summary(
    config: &ResolvedInferenceConfig,
    model_key: &ModelKey,
    context_a: &ContextKey,
    context_b: &ContextKey,
    attempts: Vec<SmokeAttemptSummary>,
    status: &ModelManagerStatus,
    observed: &NativeObservations,
    probe: &NativeAbortTestProbe,
    queued_cancel_reclaimed_capacity: bool,
    replacement_admitted_while_active: bool,
    active_attempt_cancelled: bool,
    replacement_succeeded: bool,
    queued_attempt_never_started: bool,
    queued_attempt_has_no_evidence: bool,
) -> Result<SmokeSummary> {
    let mut history_counts = BTreeMap::<String, Vec<usize>>::new();
    for generation in &observed.generations {
        history_counts
            .entry(generation.context_key_digest.clone())
            .or_default()
            .push(generation.rendered_message_count);
    }
    Ok(SmokeSummary {
        schema: SMOKE_SCHEMA,
        task_id: SMOKE_TASK_ID,
        outcome: "passed",
        model_key_digest: model_key.digest().to_string(),
        config_digest: config_digest(config)?,
        context_key_digests: vec![
            context_a.digest().to_string(),
            context_b.digest().to_string(),
        ],
        attempts,
        counters: SmokeCounterSummary {
            model_loads: status.model_loads,
            context_loads: status.context_loads,
            cached_contexts_before_shutdown: status.cached_contexts,
            completed_jobs: status.completed_jobs,
            cancellations: status.cancellations,
            deadline_exceeded: status.deadline_exceeded,
            failures: status.failures,
        },
        admission: SmokeAdmissionSummary {
            queue_capacity: 1,
            queued_depth_observed: 1,
            depth_after_queued_cancel: 0,
            replacement_depth_before_active_release: 1,
            queued_cancel_reclaimed_capacity,
            replacement_admitted_while_active,
            queued_attempt_never_started,
            queued_attempt_has_no_evidence,
            replacement_succeeded,
        },
        native_abort: SmokeNativeAbortSummary {
            callback_installations: probe.installed(),
            callback_calls: probe.callback_calls(),
            aborting_callback_calls: probe.aborting_callback_calls(),
            install_wait_timed_out: probe.install_wait_timed_out(),
            active_attempt_cancelled,
        },
        runtime_observations: SmokeRuntimeObservationSummary {
            model_load_digests: observed.model_load_digests.clone(),
            context_create_digests: observed.context_create_digests.clone(),
            generation_attempt_ids: observed
                .generations
                .iter()
                .map(|entry| entry.attempt_id.clone())
                .collect(),
            rendered_message_counts_by_context: history_counts,
        },
        resource_lifecycle: SmokeResourceLifecycleSummary {
            drops: observed.resource_drops.clone(),
            all_contexts_dropped_before_model: all_contexts_dropped_before_model(
                &observed.resource_drops,
            ),
        },
    })
}

fn all_contexts_dropped_before_model(drops: &[ResourceDropObservation]) -> bool {
    let Some(model_index) = drops.iter().position(|entry| entry.kind == "model") else {
        return false;
    };
    drops.iter().filter(|entry| entry.kind == "context").count() == 2
        && drops
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.kind == "context")
            .all(|(index, _)| index < model_index)
        && model_index + 1 == drops.len()
}

fn config_digest(config: &ResolvedInferenceConfig) -> Result<String> {
    let bytes = serde_json::to_vec(config)?;
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(7 + digest.len() * 2);
    encoded.push_str("sha256:");
    for byte in digest {
        write!(&mut encoded, "{byte:02x}")?;
    }
    Ok(encoded)
}

fn validate_safe_summary<'a>(
    bytes: &[u8],
    config: &ResolvedInferenceConfig,
    artifact_root: &Path,
    store_root: &Path,
    sensitive_text: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    validate_safe_value(&value, None)?;
    let encoded = String::from_utf8(bytes.to_vec())?;
    for sensitive in [
        config.backend.model.to_string_lossy().as_ref(),
        artifact_root.to_string_lossy().as_ref(),
        store_root.to_string_lossy().as_ref(),
    ] {
        ensure!(
            sensitive.is_empty() || !encoded.contains(sensitive),
            "safe smoke summary contains a private path"
        );
    }
    for sensitive in sensitive_text {
        ensure!(
            sensitive.is_empty() || !encoded.contains(sensitive),
            "safe smoke summary contains request or response text"
        );
    }
    Ok(())
}

fn validate_safe_value(value: &serde_json::Value, key: Option<&str>) -> Result<()> {
    const FORBIDDEN_KEYS: [&str; 7] = [
        "prompt",
        "content",
        "output",
        "model_path",
        "config_path",
        "native_log",
        "runtime_log",
    ];
    if let Some(key) = key {
        ensure!(
            FORBIDDEN_KEYS
                .iter()
                .all(|forbidden| !key.contains(forbidden)),
            "safe smoke summary contains forbidden field {key}"
        );
    }
    match value {
        serde_json::Value::Object(fields) => {
            for (field, value) in fields {
                validate_safe_value(value, Some(field))?;
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                validate_safe_value(value, key)?;
            }
        }
        serde_json::Value::String(value) => {
            ensure!(
                !value.starts_with('/') && !value.starts_with("file:") && !value.contains(".."),
                "safe smoke summary contains an unsafe path-like value"
            );
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
    Ok(())
}

fn smoke_summary_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(".agl/smoke/AGL-139")
        .join(SMOKE_SUMMARY_FILE)
}

fn remove_stale_summary(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn write_summary(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("AGL-139 smoke summary path has no parent")?;
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    let mut terminated = bytes.to_vec();
    terminated.push(b'\n');
    std::fs::write(&temporary, terminated)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

#[test]
fn safe_summary_shape_rejects_sensitive_fields_and_absolute_paths() {
    assert!(
        validate_safe_value(
            &serde_json::json!({
                "schema": SMOKE_SCHEMA,
                "events_ref": "runs/run_01890f3b-6d7a-7c1f-b4b5-8f7e0c1a2b31/events.jsonl",
                "passed": true
            }),
            None,
        )
        .is_ok()
    );
    assert!(validate_safe_value(&serde_json::json!({"prompt": "private request"}), None,).is_err());
    assert!(
        validate_safe_value(
            &serde_json::json!({"events_ref": "/private/events.jsonl"}),
            None,
        )
        .is_err()
    );
}
