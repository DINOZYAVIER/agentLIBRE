use std::fs::{self, File};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agl_model::ModelExecutionPlan;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::host::descriptors::VerifiedDescriptorSet;
use crate::host::{EngineInventory, ResourcePools};
use crate::{
    InferenceFailure, InferenceFinishReason, InferenceRequest, InferenceResponse,
    InferenceResponseMetadata, ResolvedMediaAttachment,
};

use super::request::{
    descriptor_mappings, launch_args, request_body, speculative_readiness_matches,
};
use super::transport::{
    HttpConnection, MAX_HTTP_RESPONSE_BYTES, bounded_body, bounded_json, create_private_directory,
    drain_diagnostics, duplicate, mark_cloexec_range, protocol, protocol_io, protocol_io_context,
};

const LISTEN_FD: RawFd = 190;
pub(super) const ARTIFACT_FD_BASE: RawFd = 200;
const EXECUTABLE_FD: RawFd = 189;

fn dri_prime_selector(native_device_id: &str) -> Result<String, InferenceFailure> {
    let value = native_device_id
        .strip_prefix("pci:")
        .unwrap_or(native_device_id);
    let bytes = value.as_bytes();
    if bytes.len() != 12
        || bytes[4] != b':'
        || bytes[7] != b':'
        || bytes[10] != b'.'
        || bytes
            .iter()
            .enumerate()
            .any(|(index, byte)| !matches!(index, 4 | 7 | 10) && !byte.is_ascii_hexdigit())
    {
        return Err(protocol("selected accelerator has no canonical PCI BDF"));
    }
    Ok(format!("pci-{}!", value.replace([':', '.'], "_")))
}

#[derive(Debug)]
pub(crate) struct EngineProcess {
    child: Child,
    connection: HttpConnection,
    control: HttpConnection,
    directory: PathBuf,
    descriptors: VerifiedDescriptorSet,
    generation: u64,
    receipt: EngineAllocationReceipt,
    diagnostics: Arc<Mutex<Vec<u8>>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EngineAllocationReceipt {
    pub receipt_id: String,
    pub plan_digest: String,
    pub reservation_id: u64,
    pub engine_generation: u64,
    pub selected_device: Option<String>,
    pub host_bytes: u64,
    pub device_bytes: u64,
    pub shared_bytes: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Readiness {
    schema: String,
    plan_digest: String,
    reservation_id: String,
    engine_generation: String,
    context_tokens: u32,
    batch_size: u32,
    ubatch_size: u32,
    slot_count: u32,
    speculative: SpeculativeReadiness,
    memory: Vec<MemoryBreakdown>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SpeculativeReadiness {
    pub(super) enabled: bool,
    pub(super) kind: String,
    pub(super) max_draft_tokens: u32,
    pub(super) min_draft_tokens: u32,
    pub(super) p_min_millionths: u32,
    pub(super) gpu_layers: i32,
    pub(super) key_cache_type: String,
    pub(super) value_cache_type: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemoryBreakdown {
    pool: String,
    device: String,
    model_bytes: u64,
    context_bytes: u64,
    compute_bytes: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveInventory {
    schema: String,
    llama_cpp_commit: String,
    devices: Vec<LiveInventoryDevice>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LiveInventoryDevice {
    identity: String,
    description: String,
    native_device_id: String,
    kind: String,
    available_pool_bytes: u64,
    physical_pool_bytes: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationStreamFrame {
    schema: String,
    attempt_id: String,
    sequence: u64,
    kind: GenerationStreamKind,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    raw_output: Option<String>,
    #[serde(default)]
    message: Option<Value>,
    #[serde(default)]
    usage: Option<GenerationStreamUsage>,
    #[serde(default)]
    prefill: Option<GenerationStreamPrefill>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum GenerationStreamKind {
    Delta,
    Final,
    Error,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationStreamUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GenerationStreamPrefill {
    tokens: u64,
    cached_tokens: u64,
    chunks: u64,
}

#[derive(Debug)]
struct GenerationStreamFinal {
    finish_reason: String,
    raw_output: String,
    message: Value,
    usage: GenerationStreamUsage,
    prefill: GenerationStreamPrefill,
}

impl EngineProcess {
    pub(crate) fn start(
        executable_path: &Path,
        executable: &File,
        plan: &ModelExecutionPlan,
        native_device_id: Option<&str>,
        descriptors: VerifiedDescriptorSet,
        generation: u64,
        reservation_id: u64,
    ) -> Result<Self, InferenceFailure> {
        let directory = create_private_directory()?;
        let socket_path = directory.join("bootstrap.sock");
        let listener = UnixListener::bind(&socket_path)
            .map_err(|error| protocol_io_context("bind private listener", error))?;
        let connection = UnixStream::connect(&socket_path)
            .map_err(|error| protocol_io_context("connect private listener", error))?;
        let control = UnixStream::connect(&socket_path)
            .map_err(|error| protocol_io_context("connect private control listener", error))?;
        fs::remove_file(&socket_path)
            .map_err(|error| protocol_io_context("unlink private listener", error))?;
        let mappings = descriptor_mappings(&directory, &descriptors)?;
        let slot_root = directory.join("slots");
        fs::create_dir(&slot_root)
            .map_err(|error| protocol_io_context("create private slot directory", error))?;
        let selected_accelerator = plan
            .selected_device()
            .filter(|device| !matches!(device.kind, agl_model::HostCapabilityDeviceKind::Cpu));
        // The private DRM namespace exposes exactly one render node, so the
        // engine-local Vulkan ordinal is always zero. The receipt and public
        // metadata retain the canonical host identity from the plan.
        let engine_device_identity = selected_accelerator.map(|_| "Vulkan0").or_else(|| {
            plan.selected_device()
                .map(|device| device.identity.as_str())
        });
        let args = launch_args(plan, &mappings, &slot_root, engine_device_identity)?;
        let dri_prime = selected_accelerator
            .map(|_| {
                native_device_id
                    .ok_or_else(|| protocol("selected accelerator has no runtime identity"))
                    .and_then(dri_prime_selector)
            })
            .transpose()?;
        let executable_fd = executable.as_raw_fd();
        let listener_fd = listener.as_raw_fd();
        let artifact_fds = descriptors
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| (file.file.as_raw_fd(), ARTIFACT_FD_BASE + index as RawFd))
            .collect::<Vec<_>>();
        let artifact_fd_end = ARTIFACT_FD_BASE + artifact_fds.len() as RawFd;
        let parent_pid = unsafe { libc::getpid() };
        #[cfg(target_os = "linux")]
        let sandbox = super::sandbox::PreparedSandbox::prepare(
            executable_path,
            executable,
            &descriptors,
            &directory,
            plan.selected_device(),
        )
        .map_err(|error| protocol_io_context("prepare sealed engine sandbox", error))?;

        let mut command = Command::new(format!("/proc/self/fd/{EXECUTABLE_FD}"));
        command
            .args(args)
            .env_clear()
            .env("AGL_LLAMA_SERVER_LISTEN_FD", LISTEN_FD.to_string())
            .env("AGL_INFERENCE_PLAN_DIGEST", plan.digest().as_str())
            .env("AGL_INFERENCE_RESERVATION_ID", reservation_id.to_string())
            .env("AGL_INFERENCE_ENGINE_GENERATION", generation.to_string())
            .current_dir(&directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(selector) = dri_prime {
            command.env("DRI_PRIME", selector);
        }
        if selected_accelerator.is_some() {
            command
                .env("MESA_SHADER_CACHE_DISABLE", "true")
                .env("VK_LOADER_LAYERS_DISABLE", "~implicit~");
        }
        // SAFETY: the callback performs only async-signal-safe descriptor
        // operations. All paths, arguments and allocations are prepared in
        // the parent before `fork`.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    return Err(std::io::Error::other(
                        "inference host exited during engine launch",
                    ));
                }
                duplicate(executable_fd, EXECUTABLE_FD)?;
                duplicate(listener_fd, LISTEN_FD)?;
                for (source, target) in &artifact_fds {
                    duplicate(*source, *target)?;
                }
                mark_cloexec_range(3, EXECUTABLE_FD - 1)?;
                mark_cloexec_range(LISTEN_FD + 1, ARTIFACT_FD_BASE - 1)?;
                mark_cloexec_range(artifact_fd_end, i32::MAX)?;
                #[cfg(target_os = "linux")]
                sandbox.enter()?;
                Ok(())
            });
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = fs::remove_dir_all(&directory);
                return Err(protocol_io_context("spawn sealed llama-server", error));
            }
        };
        let stderr = child.stderr.take().ok_or_else(|| {
            protocol("sealed llama-server did not expose its diagnostic descriptor")
        })?;
        let diagnostics = Arc::new(Mutex::new(Vec::new()));
        let diagnostic_sink = Arc::clone(&diagnostics);
        std::thread::Builder::new()
            .name(format!("agl-engine-diagnostics-{generation}"))
            .spawn(move || drain_diagnostics(stderr, &diagnostic_sink))
            .map_err(|error| protocol_io_context("start engine diagnostic drain", error))?;
        drop(listener);
        let mut process = Self {
            child,
            connection: HttpConnection::new(connection),
            control: HttpConnection::new(control),
            directory,
            descriptors,
            generation,
            receipt: EngineAllocationReceipt {
                receipt_id: String::new(),
                plan_digest: String::new(),
                reservation_id: 0,
                engine_generation: 0,
                selected_device: None,
                host_bytes: 0,
                device_bytes: 0,
                shared_bytes: 0,
            },
            diagnostics,
        };
        process.receipt = match process.verify_readiness(plan, reservation_id) {
            Ok(receipt) => receipt,
            Err(error) => {
                process.terminate();
                return Err(error);
            }
        };
        Ok(process)
    }

    pub(crate) fn receipt(&self) -> &EngineAllocationReceipt {
        &self.receipt
    }

    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    pub(crate) fn available_pools(
        &mut self,
        expected: &EngineInventory,
    ) -> Result<ResourcePools, InferenceFailure> {
        if let Some(status) = self.child.try_wait().map_err(protocol_io)? {
            return Err(protocol(&format!(
                "llama-server exited before live inventory: {status}"
            )));
        }
        let response = self.connection.request("GET", "/agl/v1/inventory", None)?;
        if response.status != 200 {
            return Err(protocol(&format!(
                "live inventory returned HTTP {}; body: {}",
                response.status,
                bounded_body(&response.body)
            )));
        }
        let inventory: LiveInventory = serde_json::from_slice(&response.body)
            .map_err(|error| protocol(&format!("invalid live inventory JSON: {error}")))?;
        let selected_accelerator = self.receipt.selected_device.as_deref().filter(|selected| {
            expected.devices.iter().any(|device| {
                device.identity == *selected
                    && !matches!(device.kind, agl_model::HostCapabilityDeviceKind::Cpu)
            })
        });
        let is_expected_live_device = |device: &agl_model::HostCapabilityDevice| {
            selected_accelerator.is_none_or(|selected| {
                device.identity == selected
                    || matches!(device.kind, agl_model::HostCapabilityDeviceKind::Cpu)
            })
        };
        let expected_device_count = expected
            .devices
            .iter()
            .filter(|device| is_expected_live_device(device))
            .count();
        if inventory.schema != "agentlibre.llama-inventory/v1"
            || inventory.llama_cpp_commit != expected.llama_cpp_commit
            || inventory.devices.len() != expected_device_count
        {
            return Err(protocol(&format!(
                "live inventory identity is stale or foreign: schema={}, commit={}, devices={:?}; expected schema=agentlibre.llama-inventory/v1, commit={}, devices={expected_device_count}",
                inventory.schema,
                inventory.llama_cpp_commit,
                inventory
                    .devices
                    .iter()
                    .map(|device| device.identity.as_str())
                    .collect::<Vec<_>>(),
                expected.llama_cpp_commit,
            )));
        }
        let mut device_bytes = 0_u64;
        let mut shared_bytes = 0_u64;
        for native in inventory.devices {
            let expected_identity = selected_accelerator
                .filter(|_| native.kind != "cpu")
                .unwrap_or(native.identity.as_str());
            let Some(device) = expected
                .devices
                .iter()
                .find(|device| device.identity == expected_identity)
            else {
                return Err(protocol("live inventory contains an unknown device"));
            };
            if !is_expected_live_device(device) {
                return Err(protocol("live inventory contains an unselected device"));
            }
            let runtime = expected
                .runtime_devices
                .iter()
                .find(|runtime| runtime.identity == expected_identity)
                .ok_or_else(|| protocol("live inventory omitted runtime device identity"))?;
            if native.description != runtime.description
                || native.native_device_id != runtime.native_device_id
                || (selected_accelerator.is_some()
                    && native.kind != "cpu"
                    && native.identity != "Vulkan0")
                || native.kind.is_empty()
                || native.physical_pool_bytes != device.physical_pool_bytes
                || native.available_pool_bytes > native.physical_pool_bytes
            {
                return Err(protocol("live inventory device fields are invalid"));
            }
            match device.kind {
                agl_model::HostCapabilityDeviceKind::DiscreteGpu
                | agl_model::HostCapabilityDeviceKind::Accelerator => {
                    device_bytes = device_bytes.max(native.available_pool_bytes);
                }
                agl_model::HostCapabilityDeviceKind::IntegratedGpu => {
                    shared_bytes = shared_bytes.max(native.available_pool_bytes);
                }
                _ => {}
            }
        }
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        let host_bytes = system.available_memory();
        if host_bytes == 0 {
            return Err(protocol("live host memory inventory is unavailable"));
        }
        Ok(ResourcePools {
            host_bytes,
            device_bytes,
            shared_bytes,
        })
    }

    pub(crate) fn generate(
        &mut self,
        plan: &ModelExecutionPlan,
        request: &InferenceRequest,
        media: &[ResolvedMediaAttachment],
        cancellation: &crate::InferenceCancellation,
        deadline: Option<Instant>,
        mut on_delta: impl FnMut(u64, String),
    ) -> Result<InferenceResponse, InferenceFailure> {
        if let Some(status) = self.child.try_wait().map_err(protocol_io)? {
            return Err(protocol(&format!(
                "llama-server exited before generation: {status}"
            )));
        }
        let body = request_body(plan, request, media)?;
        let started = Instant::now();
        if let Err(error) = self.connection.write_request(
            "POST",
            "/agl/v1/generate",
            Some(&body.bytes),
            &body.content_type,
        ) {
            let status = self.child.try_wait().map_err(protocol_io)?;
            return Err(protocol(&format!(
                "failed to send generation request ({error}); engine_status={status:?}; diagnostics={}",
                self.bounded_diagnostics()
            )));
        }
        let mut expected_sequence = 1_u64;
        let mut streamed_raw = String::new();
        let mut terminal = None;
        let response = match self.connection.read_generation_response(
            Some(cancellation),
            deadline,
            |line| {
                let frame: GenerationStreamFrame =
                    serde_json::from_slice(line).map_err(|error| {
                        protocol(&format!("invalid generation stream frame: {error}"))
                    })?;
                if frame.schema != "agentlibre.llama-stream/v1"
                    || frame.attempt_id != request.attempt_id.as_str()
                    || frame.sequence != expected_sequence
                    || terminal.is_some()
                {
                    return Err(protocol(
                        "generation stream identity, sequence or terminal order is invalid",
                    ));
                }
                expected_sequence = expected_sequence
                    .checked_add(1)
                    .ok_or_else(|| protocol("generation stream sequence overflow"))?;
                match frame.kind {
                    GenerationStreamKind::Delta => {
                        let content = frame
                            .content
                            .ok_or_else(|| protocol("generation delta omitted content"))?;
                        if frame.finish_reason.is_some()
                            || frame.raw_output.is_some()
                            || frame.message.is_some()
                            || frame.usage.is_some()
                            || frame.prefill.is_some()
                            || frame.error.is_some()
                        {
                            return Err(protocol("generation delta contains terminal fields"));
                        }
                        if content.is_empty() {
                            return Err(protocol("generation stream contains an empty delta"));
                        }
                        if streamed_raw
                            .len()
                            .checked_add(content.len())
                            .is_none_or(|size| size > MAX_HTTP_RESPONSE_BYTES)
                        {
                            return Err(protocol("raw generated output exceeds the AGL bound"));
                        }
                        streamed_raw.push_str(&content);
                        on_delta(frame.sequence, content);
                    }
                    GenerationStreamKind::Final => {
                        if frame.content.is_some() || frame.error.is_some() {
                            return Err(protocol("generation final contains nonterminal fields"));
                        }
                        let finish_reason = frame
                            .finish_reason
                            .ok_or_else(|| protocol("generation final omitted finish reason"))?;
                        let raw_output = frame
                            .raw_output
                            .ok_or_else(|| protocol("generation final omitted raw output"))?;
                        let message = frame
                            .message
                            .ok_or_else(|| protocol("generation final omitted parsed message"))?;
                        let usage = frame
                            .usage
                            .ok_or_else(|| protocol("generation final omitted usage"))?;
                        let prefill = frame
                            .prefill
                            .ok_or_else(|| protocol("generation final omitted prefill evidence"))?;
                        if raw_output.as_bytes() != streamed_raw.as_bytes() {
                            return Err(protocol(
                                "streamed raw output disagrees with the terminal raw output",
                            ));
                        }
                        terminal = Some(GenerationStreamFinal {
                            finish_reason,
                            raw_output,
                            message,
                            usage,
                            prefill,
                        });
                    }
                    GenerationStreamKind::Error => {
                        if frame.content.is_some()
                            || frame.finish_reason.is_some()
                            || frame.raw_output.is_some()
                            || frame.message.is_some()
                            || frame.usage.is_some()
                            || frame.prefill.is_some()
                        {
                            return Err(protocol("generation error contains result fields"));
                        }
                        let error = frame
                            .error
                            .ok_or_else(|| protocol("generation error omitted details"))?;
                        return Err(protocol(&format!(
                            "native generation stream failed: {}",
                            bounded_json(&error)
                        )));
                    }
                }
                Ok(())
            },
        ) {
            Ok(response) => response,
            Err(error @ (InferenceFailure::Cancelled | InferenceFailure::DeadlineExceeded)) => {
                self.cancel_attempt(request.attempt_id.as_str())?;
                self.terminate();
                return Err(error);
            }
            Err(error) => {
                let status = self.child.try_wait().map_err(protocol_io)?;
                return Err(protocol(&format!(
                    "generation response failed ({error}); engine_status={status:?}; diagnostics={}",
                    self.bounded_diagnostics()
                )));
            }
        };
        if response.status != 200 {
            if let Some(error) = typed_generation_error(&response.body) {
                return Err(error);
            }
            return Err(protocol(&format!(
                "generation returned HTTP {}; body: {}",
                response.status,
                bounded_body(&response.body)
            )));
        }
        if !response.body.is_empty() {
            return Err(protocol(
                "streaming generation returned an unexpected fixed response body",
            ));
        }
        let final_frame = if let Some(frame) = terminal {
            frame
        } else {
            let status = self.child.try_wait().map_err(protocol_io)?;
            return Err(protocol(&format!(
                "generation stream omitted its terminal frame after {} frames and {} raw bytes; engine_status={status:?}; diagnostics={}",
                expected_sequence.saturating_sub(1),
                streamed_raw.len(),
                self.bounded_diagnostics()
            )));
        };
        let raw = final_frame.raw_output;
        let finish_reason = match final_frame.finish_reason.as_str() {
            "stop" | "tool_calls" => InferenceFinishReason::Stop,
            "length" => InferenceFinishReason::Length,
            reason => return Err(protocol(&format!("unknown finish reason `{reason}`"))),
        };
        let choice = json!({"message": final_frame.message});
        validate_engine_output_projection(&raw, &choice)?;
        let input_tokens = final_frame.usage.prompt_tokens;
        let output_tokens = final_frame.usage.completion_tokens;
        if final_frame.prefill.tokens != input_tokens
            || final_frame.prefill.cached_tokens > input_tokens
        {
            return Err(protocol(
                "prefill token evidence disagrees with generation usage",
            ));
        }
        let prefill_chunks = final_frame.prefill.chunks;
        if prefill_chunks == 0 && input_tokens > 0 {
            return Err(protocol("generation omitted actual prefill chunk count"));
        }
        Ok(InferenceResponse {
            attempt_id: request.attempt_id.clone(),
            content: raw,
            finish_reason,
            metadata: InferenceResponseMetadata {
                model_state: Some(format!("generation:{}", self.generation)),
                selected_device: plan.selected_device().map(|device| device.identity.clone()),
                duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                input_tokens,
                output_tokens,
                configured_batch_size: plan.runtime().batch_size(),
                prefill_chunks,
                resource_admission: None,
            },
        })
    }

    fn cancel_attempt(&mut self, attempt_id: &str) -> Result<(), InferenceFailure> {
        let body = serde_json::to_vec(&json!({
            "attempt_id": attempt_id,
            "action": "cancel",
        }))
        .map_err(|error| protocol(&format!("failed to encode cancellation: {error}")))?;
        let cancellation = crate::InferenceCancellation::new();
        let response = self.control.request_with_control(
            "POST",
            "/agl/v1/control",
            Some(&body),
            "application/json",
            &cancellation,
            Some(Instant::now() + Duration::from_secs(5)),
        )?;
        if response.status != 200 {
            return Err(protocol(&format!(
                "cancellation returned HTTP {}; body: {}",
                response.status,
                bounded_body(&response.body)
            )));
        }
        let value: Value = serde_json::from_slice(&response.body)
            .map_err(|error| protocol(&format!("invalid cancellation JSON: {error}")))?;
        if value.get("schema").and_then(Value::as_str) != Some("agentlibre.llama-cancel/v1")
            || value.get("attempt_id").and_then(Value::as_str) != Some(attempt_id)
            || value.get("acknowledged").and_then(Value::as_bool) != Some(true)
        {
            return Err(protocol("cancellation acknowledgement identity is invalid"));
        }
        Ok(())
    }

    pub(crate) fn clear_slot(&mut self) -> Result<(), InferenceFailure> {
        if let Some(status) = self.child.try_wait().map_err(protocol_io)? {
            return Err(protocol(&format!(
                "llama-server exited before slot clear: {status}"
            )));
        }
        let response = self
            .connection
            .request("POST", "/agl/v1/slot/0?action=erase", None)?;
        if response.status != 200 {
            return Err(protocol(&format!(
                "slot clear returned HTTP {}; body: {}",
                response.status,
                bounded_body(&response.body)
            )));
        }
        Ok(())
    }

    fn verify_readiness(
        &mut self,
        plan: &ModelExecutionPlan,
        reservation_id: u64,
    ) -> Result<EngineAllocationReceipt, InferenceFailure> {
        let deadline = Instant::now() + Duration::from_secs(600);
        let response = loop {
            if let Some(status) = self.child.try_wait().map_err(protocol_io)? {
                return Err(protocol(&format!(
                    "llama-server exited while loading the model: {status}; diagnostics={}",
                    self.bounded_diagnostics()
                )));
            }
            let response = match self.connection.request("GET", "/agl/v1/readiness", None) {
                Ok(response) => response,
                Err(error) => {
                    let status = self.child.try_wait().map_err(protocol_io)?;
                    return Err(protocol(&format!(
                        "readiness response failed ({error}); engine_status={status:?}; diagnostics={}",
                        self.bounded_diagnostics()
                    )));
                }
            };
            if response.status == 200 {
                break response;
            }
            if response.status != 503 || Instant::now() >= deadline {
                return Err(protocol(&format!(
                    "readiness returned HTTP {}; body: {}",
                    response.status,
                    bounded_body(&response.body)
                )));
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let value: Value = serde_json::from_slice(&response.body)
            .map_err(|error| protocol(&format!("invalid readiness JSON: {error}")))?;
        let readiness: Readiness = serde_json::from_value(
            value
                .get("agl")
                .cloned()
                .ok_or_else(|| protocol("readiness omitted the typed AGL receipt"))?,
        )
        .map_err(|error| protocol(&format!("invalid AGL readiness receipt: {error}")))?;
        let runtime = plan.runtime();
        if readiness.schema != "agentlibre.llama-readiness/v1"
            || readiness.plan_digest != plan.digest().as_str()
            || readiness.reservation_id != reservation_id.to_string()
            || readiness.engine_generation != self.generation.to_string()
            || readiness.context_tokens != runtime.context_tokens()
            || readiness.batch_size != runtime.batch_size()
            || readiness.ubatch_size != runtime.ubatch_size()
            || readiness.slot_count != 1
            || runtime.slot_count() != 1
            || !speculative_readiness_matches(&readiness.speculative, runtime.mtp())
        {
            return Err(protocol(
                "native readiness does not match the exact runtime plan",
            ));
        }
        let mut host_bytes = 0_u64;
        let mut device_bytes = 0_u64;
        let selected_device = plan.selected_device().map(|device| device.identity.clone());
        for entry in readiness.memory {
            let total = entry
                .model_bytes
                .checked_add(entry.context_bytes)
                .and_then(|value| value.checked_add(entry.compute_bytes))
                .ok_or_else(|| protocol("native readiness memory arithmetic overflow"))?;
            match entry.pool.as_str() {
                "host" => {
                    host_bytes = host_bytes
                        .checked_add(total)
                        .ok_or_else(|| protocol("host receipt overflow"))?
                }
                "device" => {
                    if entry.device.is_empty()
                        || selected_device
                            .as_ref()
                            .is_none_or(|selected| selected != &entry.device)
                    {
                        return Err(protocol("device memory receipt omitted device identity"));
                    }
                    device_bytes = device_bytes
                        .checked_add(total)
                        .ok_or_else(|| protocol("device receipt overflow"))?;
                }
                _ => return Err(protocol("native readiness contains an unknown memory pool")),
            }
        }
        let resources = plan.resources();
        let admitted_host = resources
            .host_private_bytes()
            .checked_add(resources.decoder_scratch_bytes())
            .ok_or_else(|| protocol("planned host envelope overflow"))?;
        let (device_bytes, shared_bytes) = if resources.shared_bytes() > 0 {
            (0, device_bytes)
        } else {
            (device_bytes, 0)
        };
        if host_bytes > admitted_host
            || device_bytes > resources.device_private_bytes()
            || shared_bytes > resources.shared_bytes()
        {
            return Err(InferenceFailure::InvalidAllocationReceipt {
                reason: "native memory exceeds the admitted plan envelope".to_owned(),
                admitted: ResourcePools {
                    host_bytes: admitted_host,
                    device_bytes: resources.device_private_bytes(),
                    shared_bytes: resources.shared_bytes(),
                },
                reported: ResourcePools {
                    host_bytes,
                    device_bytes,
                    shared_bytes,
                },
            });
        }
        Ok(EngineAllocationReceipt {
            receipt_id: format!("engine:{}:reservation:{reservation_id}", self.generation),
            plan_digest: readiness.plan_digest,
            reservation_id,
            engine_generation: self.generation,
            selected_device,
            host_bytes,
            device_bytes,
            shared_bytes,
        })
    }

    fn bounded_diagnostics(&self) -> String {
        let bytes = self
            .diagnostics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let text = String::from_utf8_lossy(&bytes);
        text.chars()
            .take(4096)
            .map(|character| {
                if character == '\n'
                    || character == '\r'
                    || character == '\t'
                    || !character.is_control()
                {
                    character
                } else {
                    '�'
                }
            })
            .collect()
    }

    pub(crate) fn terminate(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn typed_generation_error(body: &[u8]) -> Option<InferenceFailure> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let error = value.get("error").unwrap_or(&value);
    if error.get("type").and_then(Value::as_str) != Some("exceed_context_size_error") {
        return None;
    }
    let required_tokens = error.get("n_prompt_tokens").and_then(Value::as_u64)?;
    let context_tokens = error.get("n_ctx").and_then(Value::as_u64)?;
    if required_tokens == 0 || context_tokens == 0 || required_tokens <= context_tokens {
        return None;
    }
    Some(InferenceFailure::ContextOverflow {
        required_tokens,
        context_tokens,
    })
}

fn validate_engine_output_projection(raw: &str, choice: &Value) -> Result<(), InferenceFailure> {
    use agl_actions::ParsedModelOutput;

    let projected = choice
        .pointer("/message/tool_calls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    match agl_actions::parse_model_output(raw) {
        ParsedModelOutput::Answer(answer) => {
            if answer != raw || !projected.is_empty() {
                return Err(protocol(
                    "engine Tool projection disagrees with AGL raw-output parsing",
                ));
            }
        }
        ParsedModelOutput::MalformedToolCall(_) => {
            return Err(protocol(
                "engine returned raw bytes that AGL classifies as a malformed Tool call",
            ));
        }
        ParsedModelOutput::ToolCall(parsed) => {
            if projected.len() != 1 {
                return Err(protocol(
                    "engine Tool projection must contain the one raw Tool call",
                ));
            }
            let function = projected[0]
                .get("function")
                .and_then(Value::as_object)
                .ok_or_else(|| protocol("engine Tool projection omitted function"))?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| protocol("engine Tool projection omitted function name"))?;
            let arguments = function
                .get("arguments")
                .and_then(Value::as_str)
                .ok_or_else(|| protocol("engine Tool projection omitted arguments"))?;
            let arguments: Value = serde_json::from_str(arguments)
                .map_err(|_| protocol("engine Tool projection arguments are invalid JSON"))?;
            if name != parsed.name || arguments != parsed.arguments {
                return Err(protocol(
                    "engine Tool projection disagrees with AGL raw-output parsing",
                ));
            }
        }
    }
    Ok(())
}

impl Drop for EngineProcess {
    fn drop(&mut self) {
        self.terminate();
        self.descriptors.files.clear();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream;

    use serde_json::json;

    use super::super::request::format_millionths;
    use super::{
        GenerationStreamFrame, HttpConnection, SpeculativeReadiness, typed_generation_error,
        validate_engine_output_projection,
    };
    use crate::InferenceFailure;

    // MIW-OUT-001 and MIW-CACHE-001.
    #[test]
    fn native_prompt_overflow_is_preserved_as_a_typed_terminal() {
        let body = br#"{"error":{"code":400,"message":"too large","type":"exceed_context_size_error","n_prompt_tokens":4097,"n_ctx":4096}}"#;
        assert!(matches!(
            typed_generation_error(body),
            Some(InferenceFailure::ContextOverflow {
                required_tokens: 4097,
                context_tokens: 4096,
            })
        ));
        assert!(typed_generation_error(br#"{"error":{"type":"server_error"}}"#).is_none());
    }

    #[test]
    fn mtp_probability_has_one_canonical_exact_argv_encoding() {
        assert_eq!(format_millionths(0), "0.000000");
        assert_eq!(format_millionths(250_000), "0.250000");
        assert_eq!(format_millionths(1_000_000), "1.000000");
        let disabled = SpeculativeReadiness {
            enabled: false,
            kind: "none".to_owned(),
            max_draft_tokens: 3,
            min_draft_tokens: 0,
            p_min_millionths: 0,
            gpu_layers: -1,
            key_cache_type: "f16".to_owned(),
            value_cache_type: "f16".to_owned(),
        };
        assert!(super::speculative_readiness_matches(&disabled, None));
    }

    // MIW-TOOL-002, MIW-TOOL-003 and MIW-PROTO-001.
    #[test]
    fn raw_output_and_engine_tool_projection_must_agree_exactly() {
        let answer = json!({"message": {"content": "hello", "tool_calls": []}});
        validate_engine_output_projection("hello", &answer).unwrap();

        let raw = r#"<tool_call>{"name":"read_file","arguments":{"path":"README.md"}}</tool_call>"#;
        let matching = json!({
            "message": {
                "content": raw,
                "tool_calls": [{
                    "type": "function",
                    "function": {
                        "name": "read_file",
                        "arguments": "{\"path\":\"README.md\"}"
                    }
                }]
            }
        });
        validate_engine_output_projection(raw, &matching).unwrap();

        let foreign = json!({
            "message": {
                "content": raw,
                "tool_calls": [{
                    "type": "function",
                    "function": {"name": "delete_file", "arguments": "{}"}
                }]
            }
        });
        assert!(validate_engine_output_projection(raw, &foreign).is_err());
        assert!(
            validate_engine_output_projection("plain", &matching).is_err(),
            "normalized Tool projection cannot authorize a plain raw answer"
        );
    }

    // MIW-PROTO-001, MIW-ENG-005 and MIW-ENG-008.
    #[test]
    fn bounded_chunked_generation_frames_preserve_native_order() {
        let first = b"{\"schema\":\"agentlibre.llama-stream/v1\",\"attempt_id\":\"attempt\",\"sequence\":1,\"kind\":\"delta\",\"content\":\"A\"}\n";
        let second = b"{\"schema\":\"agentlibre.llama-stream/v1\",\"attempt_id\":\"attempt\",\"sequence\":2,\"kind\":\"final\",\"finish_reason\":\"stop\",\"raw_output\":\"A\",\"message\":{\"content\":\"A\",\"tool_calls\":[]},\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1},\"prefill\":{\"tokens\":2,\"cached_tokens\":0,\"chunks\":1}}\n";
        let (client, mut server) = UnixStream::pair().unwrap();
        write!(
            server,
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            first.len()
        )
        .unwrap();
        server.write_all(first).unwrap();
        write!(server, "\r\n{:x}\r\n", second.len()).unwrap();
        server.write_all(second).unwrap();
        server.write_all(b"\r\n0\r\n\r\n").unwrap();
        drop(server);

        let mut connection = HttpConnection::new(client);
        let mut frames = Vec::new();
        let response = connection
            .read_generation_response(None, None, |frame| {
                frames.push(serde_json::from_slice::<GenerationStreamFrame>(frame).unwrap());
                Ok(())
            })
            .unwrap();
        assert_eq!(response.status, 200);
        assert!(response.body.is_empty());
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].sequence, 1);
        assert_eq!(frames[1].sequence, 2);
    }

    #[test]
    fn clean_chunk_boundary_eof_is_left_to_the_typed_terminal_validator() {
        let final_frame = b"{\"schema\":\"agentlibre.llama-stream/v1\",\"attempt_id\":\"attempt\",\"sequence\":1,\"kind\":\"final\",\"finish_reason\":\"stop\",\"raw_output\":\"A\",\"message\":{\"content\":\"A\",\"tool_calls\":[]},\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1},\"prefill\":{\"tokens\":2,\"cached_tokens\":0,\"chunks\":1}}\n";
        let (client, mut server) = UnixStream::pair().unwrap();
        write!(
            server,
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            final_frame.len()
        )
        .unwrap();
        server.write_all(final_frame).unwrap();
        server.write_all(b"\r\n").unwrap();
        drop(server);

        let mut connection = HttpConnection::new(client);
        let mut terminal = false;
        connection
            .read_generation_response(None, None, |frame| {
                terminal = serde_json::from_slice::<GenerationStreamFrame>(frame)
                    .is_ok_and(|frame| matches!(frame.kind, super::GenerationStreamKind::Final));
                Ok(())
            })
            .unwrap();
        assert!(terminal);
    }

    #[test]
    fn generation_stream_shape_and_chunk_termination_fail_closed() {
        assert!(
            serde_json::from_str::<GenerationStreamFrame>(
                r#"{"schema":"agentlibre.llama-stream/v1","attempt_id":"a","sequence":1,"kind":"delta","content":"x","foreign":true}"#,
            )
            .is_err()
        );

        let (client, mut server) = UnixStream::pair().unwrap();
        server
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\n\r\n4\r\ntestXX",
            )
            .unwrap();
        drop(server);
        let mut connection = HttpConnection::new(client);
        assert!(
            connection
                .read_generation_response(None, None, |_| Ok(()))
                .is_err()
        );
    }
}
