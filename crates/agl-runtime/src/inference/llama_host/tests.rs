
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};

use super::*;

fn digest(byte: char) -> String {
    format!("sha256:{}", byte.to_string().repeat(64))
}

fn profile() -> LlamaRuntimeProfile {
    LlamaRuntimeProfile {
        digest: RuntimeProfileDigest::parse(digest('1')).unwrap(),
        physical_device: PhysicalDeviceDigest::parse(digest('2')).unwrap(),
        driver_build: DriverBuildDigest::parse(digest('3')).unwrap(),
        required_host_bytes: 1,
        required_device_bytes: 0,
        required_shared_bytes: 0,
        context_tokens: 1024,
        batch_size: 32,
        ubatch_size: 16,
        threads: 1,
        gpu_layers: 0,
        speculative: false,
        speculative_max_draft_tokens: 0,
        speculative_type_k: None,
        speculative_type_v: None,
        slots: 1,
        continuous_batching: false,
        device: None,
        device_paths: vec![],
        selectors: vec![],
    }
}

#[test]
fn cold_registration_never_launches_and_failed_unload_retains_service() {
    let directory = private_directory().unwrap();
    let artifact_path = directory.join("model.gguf");
    fs::write(&artifact_path, b"GGUF-test").unwrap();
    let artifact_digest = PackageDigest::from_bytes(hash32(b"GGUF-test"));
    let artifact = crate::model::import_model(&crate::model::ModelImportRequest {
        path: artifact_path,
        expected_digest: model_artifact_digest(artifact_digest).unwrap(),
    })
    .unwrap();
    let mut runtime = crate::inference::service::test_runtime_selection();
    runtime.artifact.digest = artifact_digest;
    runtime.artifact.bytes = 9;
    let key = runtime.service.key;
    let execution = BlockingExecutionClient::new(directory.join("absent-execd.sock"));
    let host = LlamaHost {
        executable: directory.join("absent-llama-server"),
        execution: execution.clone(),
        engine_build: EngineBuildDigest::from_bytes(*runtime.load.engine_build_digest.as_bytes()),
        health: Mutex::new(RestoredInferenceHealth::default()),
        health_updates: Mutex::new(vec![]),
        service_changes: Mutex::new(()),
        services: Mutex::new(BTreeMap::new()),
    };
    host.register_service(RegisteredModelService {
        model: agl_core::agent::ModelDefinitionRef {
            id: crate::package::PackageId::new("test-model").unwrap(),
            version: crate::package::PackageVersion::new("1.0.0").unwrap(),
            digest: artifact_digest,
        },
        runtime,
        artifact,
        adapters: vec![],
    })
    .unwrap();
    let service = host.services.lock().unwrap().get(&key).unwrap().clone();
    assert!(service.engine.lock().unwrap().is_none());
    let engine_directory = directory.join("engine");
    fs::create_dir(&engine_directory).unwrap();
    *service.engine.lock().unwrap() = Some(Arc::new(ResidentEngine {
        execution,
        execution_id: ExecutionId::generate(),
        directory: engine_directory.clone(),
        socket_path: engine_directory.join("llama.sock"),
        profile: profile(),
        reasoning_supported: AtomicBool::new(false),
        device_lost: Arc::new(AtomicBool::new(false)),
        diagnostic_cursor: Mutex::new(0),
        terminated: AtomicBool::new(false),
        retiring: AtomicBool::new(false),
    }));
    assert!(matches!(
        host.unload_service(key),
        Err(InferenceServiceError::OutcomeUnknown)
    ));
    assert!(host.services.lock().unwrap().contains_key(&key));
    assert!(service.engine.lock().unwrap().is_some());
    assert!(engine_directory.exists());
    drop(service);
    drop(host);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn engine_release_requires_confirmed_reap_and_keeps_unknown_ownership() {
    use agl_execution_api::{
        ExecutionCommand, ExecutionOutput, ExecutionProtocolRequest, ExecutionProtocolResponse,
        ExecutionResponse, ExecutionStatus,
    };
    use std::io::BufRead as _;

    let directory = private_directory().unwrap();
    let socket = directory.join("execd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let execution_id = ExecutionId::generate();
    let server = std::thread::spawn(move || {
        for index in 0..5 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            std::io::BufReader::new(&mut stream)
                .read_line(&mut line)
                .unwrap();
            let request: ExecutionProtocolRequest = serde_json::from_str(&line).unwrap();
            let response = match request.command {
                ExecutionCommand::Signal {
                    execution_id: id,
                    signal: ExecutionSignal::Terminate,
                } => {
                    assert_eq!(id, execution_id);
                    assert!(matches!(index, 0 | 2));
                    ExecutionResponse::Acknowledged
                }
                ExecutionCommand::Inspect { execution_id: id } => {
                    assert_eq!(id, execution_id);
                    assert!(matches!(index, 1 | 3));
                    ExecutionResponse::Status {
                        status: ExecutionStatus {
                            execution_id,
                            terminal_id: None,
                            owner: ExecutionOwner::Runtime {
                                component: "inference".into(),
                            },
                            state: if index == 1 {
                                ExecutionState::OutcomeUnknown
                            } else {
                                ExecutionState::Exited
                            },
                            outcome: Some(if index == 1 {
                                ExecutionOutcome::UnknownAfterServiceRestart
                            } else {
                                ExecutionOutcome::Terminated
                            }),
                            output_bytes: 0,
                            output_truncated: false,
                        },
                    }
                }
                ExecutionCommand::Read {
                    execution_id: id,
                    after,
                    ..
                } => {
                    assert_eq!(id, execution_id);
                    assert_eq!(index, 4);
                    ExecutionResponse::Output {
                        output: ExecutionOutput {
                            execution_id,
                            after,
                            next: after,
                            chunks: vec![],
                            eof: true,
                            truncated: false,
                        },
                    }
                }
                other => panic!("unexpected release request: {other:?}"),
            };
            serde_json::to_writer(
                &mut stream,
                &ExecutionProtocolResponse::success(request.request_id, response),
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
        }
    });
    let engine = ResidentEngine {
        execution: BlockingExecutionClient::new(socket),
        execution_id,
        directory: directory.clone(),
        socket_path: directory.join("llama.sock"),
        profile: profile(),
        reasoning_supported: AtomicBool::new(false),
        device_lost: Arc::new(AtomicBool::new(false)),
        diagnostic_cursor: Mutex::new(0),
        terminated: AtomicBool::new(false),
        retiring: AtomicBool::new(false),
    };
    assert!(matches!(
        engine.terminate(),
        Err(InferenceServiceError::OutcomeUnknown)
    ));
    assert!(directory.exists());
    assert!(!engine.terminated.load(Ordering::Acquire));
    assert!(engine.retiring.load(Ordering::Acquire));
    engine.terminate().unwrap();
    assert!(engine.terminated.load(Ordering::Acquire));
    assert!(!directory.exists());
    server.join().unwrap();
    // Confirmed release is idempotent and needs no reachable execd socket.
    engine.terminate().unwrap();
}

#[test]
fn address_limit_covers_all_admitted_memory_domains() {
    let mut profile = profile();
    profile.required_host_bytes = 10;
    profile.required_device_bytes = 20;
    profile.required_shared_bytes = 30;
    assert_eq!(address_space_limit(&profile), 2 * 1024 * 1024 * 1024 + 120);

    profile.required_host_bytes = u64::MAX;
    assert_eq!(address_space_limit(&profile), u64::MAX);
}

#[test]
fn service_gate_enforces_slots_queue_and_request_local_cancellation() {
    let gate = Arc::new(ServiceGate::new(2, 1));
    let first = gate
        .acquire(&InferenceCancellation::new(), unix_ms() + 5_000)
        .unwrap();
    let second = gate
        .acquire(&InferenceCancellation::new(), unix_ms() + 5_000)
        .unwrap();
    let queued_cancellation = InferenceCancellation::new();
    let worker_cancellation = queued_cancellation.clone();
    let queued_gate = Arc::clone(&gate);
    let queued = std::thread::spawn(move || {
        queued_gate
            .acquire(&worker_cancellation, unix_ms() + 5_000)
            .map(|_| ())
    });
    for _ in 0..1_000 {
        if gate.state.lock().unwrap().queued == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(gate.state.lock().unwrap().queued, 1);
    assert!(matches!(
        gate.acquire(&InferenceCancellation::new(), unix_ms() + 5_000),
        Err(InferenceServiceError::Unavailable)
    ));

    queued_cancellation.cancel();
    assert_eq!(
        queued.join().unwrap(),
        Err(InferenceServiceError::Cancelled)
    );
    drop(first);
    drop(second);
    assert_eq!(gate.state.lock().unwrap().active, 0);
}

#[test]
fn launch_arguments_expose_selected_slots_and_continuous_batching() {
    let mut runtime = crate::inference::service::test_runtime_selection();
    runtime.service.slots = 2;
    runtime.service.continuous_batching = true;
    let mut profile = profile();
    profile.slots = 2;
    profile.continuous_batching = true;

    let arguments = launch_arguments(&runtime, &profile).unwrap();
    let parallel = arguments
        .iter()
        .position(|value| value == "--parallel")
        .unwrap();
    assert_eq!(arguments[parallel + 1], "2");
    let context = arguments
        .iter()
        .position(|value| value == "--ctx-size")
        .unwrap();
    assert_eq!(arguments[context + 1], "2048");
    assert!(!arguments.iter().any(|value| value == "--no-cont-batching"));

    runtime.service.continuous_batching = false;
    assert!(
        launch_arguments(&runtime, &profile)
            .unwrap()
            .iter()
            .any(|value| value == "--no-cont-batching")
    );
}

#[test]
fn launch_arguments_emit_exact_mtp_pairs() {
    let mut runtime = crate::inference::service::test_runtime_selection();
    runtime.load.gpu_layers = GpuLayerSelection::All;
    runtime.speculative = Some(agl_core::agent::SpeculativeSelection::Mtp {
        max_draft_tokens: 3,
        kv_cache_type_k: agl_core::agent::KvCacheType::Q4_0,
        kv_cache_type_v: agl_core::agent::KvCacheType::Q4_0,
    });
    let mut profile = profile();
    profile.gpu_layers = -2;
    profile.device = Some("Vulkan0".into());
    profile.speculative = true;
    let arguments = launch_arguments(&runtime, &profile).unwrap();
    let gpu_layers = arguments
        .iter()
        .position(|value| value == "--gpu-layers")
        .unwrap();
    assert_eq!(arguments[gpu_layers + 1], "-2");
    for pair in [
        ("--spec-type", "draft-mtp"),
        ("--spec-draft-device", "Vulkan0"),
        ("--spec-draft-ngl", "all"),
        ("--spec-draft-n-max", "3"),
        ("--spec-draft-type-k", "q4_0"),
        ("--spec-draft-type-v", "q4_0"),
    ] {
        let index = arguments.iter().position(|value| value == pair.0).unwrap();
        assert_eq!(arguments[index + 1], pair.1);
    }
}

#[test]
fn readiness_rejects_missing_or_mismatched_speculative_activation() {
    let mut profile = profile();
    profile.speculative = true;
    profile.speculative_max_draft_tokens = 3;
    profile.speculative_type_k = Some("q4_0".into());
    profile.speculative_type_v = Some("q4_0".into());
    let readiness = |enabled| Readiness {
        schema: "agentlibre.llama-readiness/v1".into(),
        plan_digest: profile.digest.to_string(),
        reservation_id: "1".into(),
        engine_generation: "1".into(),
        context_tokens: profile.context_tokens,
        batch_size: profile.batch_size,
        ubatch_size: profile.ubatch_size,
        slot_count: profile.slots,
        reasoning_supported: false,
        speculative: ReadinessSpeculative {
            enabled,
            kind: if enabled { "none,draft-mtp" } else { "none" }.into(),
            max_draft_tokens: if enabled { 3 } else { 0 },
            _min_draft_tokens: 0,
            _p_min_millionths: 0,
            gpu_layers: if enabled { -2 } else { 0 },
            key_cache_type: if enabled { "q4_0" } else { "f16" }.into(),
            value_cache_type: if enabled { "q4_0" } else { "f16" }.into(),
        },
        memory: vec![],
    };
    assert!(readiness_matches_profile(&readiness(true), &profile).unwrap());
    assert!(!readiness_matches_profile(&readiness(false), &profile).unwrap());
    profile.speculative = false;
    assert!(readiness_matches_profile(&readiness(false), &profile).unwrap());
}

#[test]
fn readiness_accepts_the_engine_speculative_object_shape() {
    let value = serde_json::json!({
        "enabled": true,
        "kind": "none,draft-mtp",
        "max_draft_tokens": 3,
        "min_draft_tokens": 0,
        "p_min_millionths": 0,
        "gpu_layers": -2,
        "key_cache_type": "q4_0",
        "value_cache_type": "q4_0"
    });
    let parsed: ReadinessSpeculative = serde_json::from_value(value).unwrap();
    assert!(parsed.enabled);
    assert_eq!(parsed.gpu_layers, -2);
    assert_eq!(parsed.max_draft_tokens, 3);
}

#[test]
fn mtp_reservation_scales_with_the_admitted_context() {
    let mut runtime = crate::inference::service::test_runtime_selection();
    runtime.load.gpu_layers = GpuLayerSelection::All;
    runtime.speculative = Some(agl_core::agent::SpeculativeSelection::Mtp {
        max_draft_tokens: 3,
        kv_cache_type_k: agl_core::agent::KvCacheType::Q4_0,
        kv_cache_type_v: agl_core::agent::KvCacheType::Q4_0,
    });
    runtime.load.context_tokens = 1024;
    let engine = EngineBuildDigest::parse(digest('4')).unwrap();
    let short = generated_profile(&runtime, engine).unwrap();
    runtime.load.context_tokens = 2048;
    let long = generated_profile(&runtime, engine).unwrap();
    assert!(long.required_device_bytes > short.required_device_bytes);
    assert!(long.required_host_bytes > short.required_host_bytes);
}

#[test]
fn reasoning_request_is_explicit_per_attempt() {
    let mut disabled = json!({});
    apply_reasoning_request(&mut disabled, agl_core::agent::ReasoningSelection::Disabled);
    assert_eq!(
        disabled,
        json!({
            "chat_template_kwargs": {"enable_thinking": false},
            "reasoning_format": "none"
        })
    );

    let mut enabled = json!({});
    apply_reasoning_request(
        &mut enabled,
        agl_core::agent::ReasoningSelection::Enabled {
            max_tokens: 4096,
            effort: Some(agl_core::agent::ReasoningEffort::Xhigh),
            preserve: true,
        },
    );
    assert_eq!(
        enabled,
        json!({
            "chat_template_kwargs": {
                "enable_thinking": true,
                "preserve_thinking": true,
                "reasoning_effort": "xhigh"
            },
            "reasoning_format": "deepseek",
            "thinking_budget_tokens": 4096
        })
    );
}

#[test]
fn oversized_schema_repetitions_are_relaxed_only_for_llama_grammar() {
    let admitted = json!({
        "type": "object",
        "properties": {
            "argv": {
                "type": "array",
                "maxItems": 256,
                "items": {"type": "string", "maxLength": 32768}
            },
            "input": {"type": "string", "maxLength": 1048576}
        }
    });

    assert_eq!(
        llama_tool_schema(admitted.clone()),
        json!({
            "type": "object",
            "properties": {
                "argv": {
                    "type": "array",
                    "maxItems": 256,
                    "items": {"type": "string"}
                },
                "input": {"type": "string"}
            }
        })
    );
    assert_eq!(admitted["properties"]["input"]["maxLength"], 1048576);
}

#[test]
fn root_object_union_is_projected_for_llama_tool_parameters() {
    let admitted = json!({
        "oneOf": [
            {
                "type": "object",
                "required": ["action", "argv"],
                "properties": {
                    "action": {"const": "open"},
                    "argv": {"type": "array"}
                }
            },
            {
                "type": "object",
                "required": ["action", "terminal_id"],
                "properties": {
                    "action": {"const": "read"},
                    "terminal_id": {"type": "string"}
                }
            }
        ]
    });

    let projected = llama_tool_schema(admitted.clone());
    assert_eq!(projected["type"], "object");
    assert_eq!(projected["required"], json!(["action"]));
    assert_eq!(
        projected["properties"]["action"],
        json!({"anyOf": [{"const": "open"}, {"const": "read"}]})
    );
    assert!(projected["properties"].get("argv").is_some());
    assert!(projected["properties"].get("terminal_id").is_some());
    assert_eq!(admitted.get("type"), None);
    assert!(admitted.get("oneOf").is_some());
}

#[test]
fn required_reasoning_fails_closed_without_template_support() {
    assert_eq!(
        validate_reasoning_capability(
            agl_core::agent::ReasoningSelection::Enabled {
                max_tokens: 1,
                effort: None,
                preserve: false,
            },
            false,
        ),
        Err(InferenceServiceError::InvalidRequest)
    );
    assert_eq!(
        validate_reasoning_capability(agl_core::agent::ReasoningSelection::Disabled, false,),
        Ok(false)
    );
}

#[test]
fn complete_prompt_reserves_the_full_generation_budget() {
    assert_eq!(ensure_context_capacity(48, 16, 64), Ok(()));
    assert_eq!(
        ensure_context_capacity(49, 16, 64),
        Err(InferenceServiceError::ContextExhausted(
            agl_core::agent::ContextCapacity::new(49, 16, 64)
        ))
    );
    assert_eq!(
        ensure_context_capacity(u64::MAX, 1, 64),
        Err(InferenceServiceError::ContextExhausted(
            agl_core::agent::ContextCapacity::new(u64::MAX, 1, 64)
        ))
    );
}

#[test]
fn prompt_capacity_is_per_slot_not_the_engines_total_context() {
    let mut profile = profile();
    profile.context_tokens = 64;
    profile.slots = 2;
    assert_eq!(engine_context_tokens(&profile).unwrap(), 128);
    assert!(
        matches!(ensure_context_capacity(49, 16, profile.context_tokens),
            Err(InferenceServiceError::ContextExhausted(capacity))
                if capacity.context_capacity_tokens == 64 && capacity.prompt_tokens == 49)
    );
}

#[test]
fn engine_context_rejects_slot_multiplication_overflow() {
    let mut profile = profile();
    profile.context_tokens = u32::MAX;
    profile.slots = 2;

    assert_eq!(
        engine_context_tokens(&profile),
        Err(InferenceServiceError::InvalidRequest)
    );
}

#[test]
fn generator_requires_the_exact_private_engine_bundle_digest() {
    let directory = private_directory().unwrap();
    let executable = directory.join("llama-server");
    let implementation = directory.join("libllama-server-impl.so");
    fs::write(&executable, b"launcher\n").unwrap();
    fs::write(&implementation, b"implementation\n").unwrap();
    let engine_build = crate::inference::private_engine_build_digest(&executable).unwrap();

    assert!(
        generator(
            LlamaServerConfig {
                executable: executable.clone(),
            },
            engine_build,
            BlockingExecutionClient::new("/test-does-not-use-execd"),
            &RestoredInferenceHealth::default(),
        )
        .is_ok()
    );

    fs::write(&implementation, b"changed implementation\n").unwrap();
    assert!(matches!(
        generator(
            LlamaServerConfig { executable },
            engine_build,
            BlockingExecutionClient::new("/test-does-not-use-execd"),
            &RestoredInferenceHealth::default(),
        ),
        Err(InferenceServiceError::InvalidRequest)
    ));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn request_appends_a_user_turn_after_an_assistant_summary() {
    let mut messages = vec![json!({"role": "assistant", "content": "summary"})];
    append_user_turn_after_assistant(&mut messages, false);
    assert_eq!(
        messages,
        vec![
            json!({"role": "assistant", "content": "summary"}),
            json!({"role": "user", "content": ""}),
        ]
    );

    let mut messages = vec![json!({"role": "user", "content": "tail"})];
    append_user_turn_after_assistant(&mut messages, true);
    assert_eq!(messages.len(), 1);
}

#[test]
fn request_instructs_continuation_after_compaction() {
    let mut messages = vec![json!({"role": "assistant", "content": "summary"})];
    append_user_turn_after_assistant(&mut messages, true);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(
        messages[1]["content"],
        "Continue the task preserved in the compaction summary, starting from the concrete next step in semantic.next_position. Use exact.inspected_files and semantic.completed as the work ledger; do not repeat completed investigation unless a file digest changed."
    );
}

#[test]
fn measurement_only_posts_the_exact_body_to_tokenizer_and_reports_oversize() {
    use std::io::BufRead as _;
    let directory = private_directory().unwrap();
    let socket = directory.join("measure.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let body = br#"{"messages":[{"role":"user","content":"fixture"}]}"#;
    let server = std::thread::spawn(move || {
        let (connection, _) = listener.accept().unwrap();
        connection
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = std::io::BufReader::new(connection);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "POST /agl/v1/input-tokens HTTP/1.1\r\n");
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            assert!(!line.is_empty());
        }
        let mut received = vec![0; body.len()];
        reader.read_exact(&mut received).unwrap();
        assert_eq!(received, body);
        let response = br#"{"input_tokens":257}"#;
        write!(
            reader.get_mut(),
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response.len()
        )
        .unwrap();
        reader.get_mut().write_all(response).unwrap();
    });
    let capacity = measure_context_capacity(&socket, body, 32, 256).unwrap();
    assert_eq!(capacity.prompt_tokens, 257);
    assert!(!capacity.fits());
    server.join().unwrap();
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn tokenizer_error_envelopes_have_typed_fail_closed_mapping() {
    let envelope = |status: u16, kind: &str, message: &str| {
        serde_json::to_vec(&json!({
            "error": {"code": status, "message": message, "type": kind}
        }))
        .unwrap()
    };

    let (error, code, _) = classify_tokenizer_error(
        400,
        &envelope(400, "exceed_context_size_error", "too large"),
        32,
        256,
    );
    assert_eq!(code, "context_exhausted");
    assert!(matches!(error, InferenceServiceError::ContextExhausted(_)));

    let (error, code, _) = classify_tokenizer_error(
        400,
        &envelope(400, "invalid_request_error", "bad template"),
        32,
        256,
    );
    assert_eq!(code, "invalid_request");
    assert_eq!(error, InferenceServiceError::InvalidRequest);

    let (error, code, diagnostic) = classify_tokenizer_error(
        503,
        &envelope(503, "unavailable_error", &"x".repeat(600)),
        32,
        256,
    );
    assert_eq!(code, "backend_unavailable");
    assert_eq!(
        error,
        InferenceServiceError::UnavailableWithReason(
            "llama-server reported an unavailable backend".to_owned()
        )
    );
    assert_eq!(diagnostic.chars().count(), 512);

    let (error, code, _) = classify_tokenizer_error(418, br#"{"error":"unknown"}"#, 32, 256);
    assert_eq!(code, "malformed_error_envelope");
    assert_eq!(error, InferenceServiceError::InvalidResult);
}

#[test]
fn private_http_stream_emits_progress_before_terminal_and_sends_identity_headers() {
    let directory = private_directory().unwrap();
    let socket = directory.join("stream.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (progress_sent, progress_seen) = std::sync::mpsc::sync_channel(1);
    let observed_before_terminal = Arc::new(AtomicBool::new(false));
    let server_observed = observed_before_terminal.clone();
    let server = std::thread::spawn(move || {
        let (mut connection, _) = listener.accept().unwrap();
        connection
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read = connection.read(&mut buffer).unwrap();
            request.extend_from_slice(&buffer[..read]);
            let Some(header_end) = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|position| position + 4)
            else {
                continue;
            };
            let header = std::str::from_utf8(&request[..header_end]).unwrap();
            let normalized_header = header.to_ascii_lowercase();
            let length = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap();
            if request.len() >= header_end + length {
                assert!(header.starts_with("POST /agl/v1/generate HTTP/1.1\r\n"));
                assert!(normalized_header.contains("\r\nx-agl-protocol: 1\r\n"));
                assert!(normalized_header.contains("\r\nx-agl-attempt-id: run:1:1\r\n"));
                break;
            }
        }
        connection
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
            .unwrap();
        let delta = frame_lines(&[json!({
            "schema": "agentlibre.llama-stream/v1",
            "attempt_id": "run:1:1",
            "sequence": 1,
            "kind": "delta",
            "content": "live"
        })]);
        write!(connection, "{:x}\r\n", delta.len()).unwrap();
        connection.write_all(&delta).unwrap();
        connection.write_all(b"\r\n").unwrap();
        connection.flush().unwrap();
        if progress_seen.recv_timeout(Duration::from_secs(1)).is_ok() {
            server_observed.store(true, Ordering::Release);
        }
        let terminal = frame_lines(&[final_frame(
            2,
            "live",
            json!({"role": "assistant", "content": "live"}),
        )]);
        write!(connection, "{:x}\r\n", terminal.len()).unwrap();
        connection.write_all(&terminal).unwrap();
        connection.write_all(b"\r\n0\r\n\r\n").unwrap();
    });
    let sink = InferenceProgressSink::new(move |_| {
        let _ = progress_sent.send(());
    });

    let response = generation_request(
        &socket,
        b"{}",
        &InferenceCancellation::new(),
        "run:1:1",
        Some(sink),
        false,
    )
    .unwrap();

    server.join().unwrap();
    fs::remove_dir_all(directory).unwrap();
    assert!(observed_before_terminal.load(Ordering::Acquire));
    assert_eq!(response.status, 200);
    assert!(response.generated.is_some());
}

#[test]
fn restored_cooldown_and_quarantine_remove_unsafe_profiles() {
    let profile = profile();
    let engine = EngineBuildDigest::parse(digest('4')).unwrap();
    let model = digest('5').parse().unwrap();
    let mut health = RestoredInferenceHealth {
        workers: vec![crate::inference::WorkerHealth {
            physical_device: profile.physical_device,
            driver_build: profile.driver_build,
            engine_build: engine,
            crash_streak: 1,
            retry_after_ms: i64::MAX,
            last_failure_kind: crate::inference::InferenceFailureKind::EngineCrash,
        }],
        quarantines: vec![],
    };
    assert!(!compatible_profile(&profile, &model, &engine, &health));
    health.workers.clear();
    health
        .quarantines
        .push(crate::inference::ResourceQuarantine {
            physical_device: profile.physical_device,
            driver_build: profile.driver_build,
            engine_build: engine,
            model,
            runtime_profile: profile.digest,
            admitted_host_bytes: 1,
            observed_host_bytes: 2,
            admitted_device_bytes: 0,
            observed_device_bytes: 0,
            admitted_shared_bytes: 0,
            observed_shared_bytes: 0,
            recorded_at_ms: 1,
        });
    assert!(!compatible_profile(
        &profile,
        &health.quarantines[0].model,
        &engine,
        &health
    ));
}

#[test]
fn allocation_overage_returns_exact_quarantine_evidence() {
    let profile = profile();
    let allocations = vec![MemoryAllocation {
        pool: "host".into(),
        device: "cpu".into(),
        model_bytes: 2,
        context_bytes: 3,
        compute_bytes: 4,
    }];
    let Err(EngineStartError::InvalidAllocation(observed)) =
        validate_allocation(&profile, &allocations)
    else {
        panic!("allocation above the admitted profile must be quarantined")
    };
    assert_eq!(
        observed,
        ObservedAllocation {
            host: 9,
            device: 0,
            shared: 0,
        }
    );
}

#[test]
fn device_loss_requires_an_exact_backend_symbol() {
    assert!(engine_diagnostic_is_device_lost(
        "ggml_vulkan: waitForFences error ErrorDeviceLost at backend.cpp:1"
    ));
    assert!(engine_diagnostic_is_device_lost(
        "vulkan returned VK_ERROR_DEVICE_LOST"
    ));
    assert!(!engine_diagnostic_is_device_lost(
        "the device lost contact with a remote worker"
    ));
    assert_eq!(
        worker_failure_kind(
            &InferenceServiceError::DeviceLost,
            Some(ExecutionOutcome::Signal {
                signal: libc::SIGKILL
            }),
        ),
        Some(InferenceFailureKind::DeviceLost)
    );
    assert_eq!(
        worker_failure_kind(
            &InferenceServiceError::Unavailable,
            Some(ExecutionOutcome::Signal {
                signal: libc::SIGKILL
            }),
        ),
        Some(InferenceFailureKind::UnattributedSignal {
            signal: libc::SIGKILL
        })
    );
    assert_eq!(
        worker_failure_kind(
            &InferenceServiceError::OutcomeUnknown,
            Some(ExecutionOutcome::UnknownAfterServiceRestart),
        ),
        None
    );
    assert_eq!(
        worker_failure_kind(
            &InferenceServiceError::OutcomeUnknown,
            Some(ExecutionOutcome::Signal {
                signal: libc::SIGTERM
            }),
        ),
        Some(InferenceFailureKind::UnattributedSignal {
            signal: libc::SIGTERM
        })
    );
    assert_ne!(
        worker_failure_kind(
            &InferenceServiceError::Unavailable,
            Some(ExecutionOutcome::Signal {
                signal: libc::SIGKILL
            }),
        ),
        Some(InferenceFailureKind::InvalidAllocation)
    );
}

fn frame_lines(frames: &[Value]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for frame in frames {
        serde_json::to_writer(&mut bytes, frame).unwrap();
        bytes.push(b'\n');
    }
    bytes
}

fn final_frame(sequence: u64, raw_output: &str, message: Value) -> Value {
    json!({
        "schema": "agentlibre.llama-stream/v1",
        "attempt_id": "run:1:1",
        "sequence": sequence,
        "kind": "final",
        "finish_reason": "stop",
        "raw_output": raw_output,
        "message": message,
        "usage": {"prompt_tokens": 3, "completion_tokens": 2},
        "prefill": {"tokens": 3, "cached_tokens": 0, "chunks": 1},
        "timings": {
            "cache_n": 1,
            "prompt_n": 2,
            "prompt_ms": 4.0,
            "prompt_per_token_ms": 2.0,
            "prompt_per_second": 500.0,
            "predicted_n": 2,
            "predicted_ms": 10.0,
            "predicted_per_token_ms": 5.0,
            "predicted_per_second": 200.0
        }
    })
}

#[test]
fn private_stream_preserves_exact_deltas_and_terminal_output() {
    let bytes = frame_lines(&[
        json!({
            "schema": "agentlibre.llama-stream/v1",
            "attempt_id": "run:1:1",
            "sequence": 1,
            "kind": "delta",
            "content": "hel"
        }),
        json!({
            "schema": "agentlibre.llama-stream/v1",
            "attempt_id": "run:1:1",
            "sequence": 2,
            "kind": "delta",
            "content": "lo"
        }),
        final_frame(3, "hello", json!({"role": "assistant", "content": "hello"})),
    ]);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let sink_observed = observed.clone();
    let sink = InferenceProgressSink::new(move |content| {
        sink_observed.lock().unwrap().push(content);
    });

    let generated = decode_generation(&bytes, "run:1:1", Some(&sink), false, false).unwrap();

    assert_eq!(
        generated.output,
        ModelGenerationOutput::Assistant(Content::text("hello").unwrap())
    );
    assert_eq!(
        *observed.lock().unwrap(),
        vec![Content::text("hel").unwrap(), Content::text("lo").unwrap()]
    );
    assert_eq!(generated.timings.prompt_per_second, 500.0);
    assert_eq!(generated.timings.predicted_per_second, 200.0);
}

#[test]
fn private_stream_requires_valid_native_timings() {
    let mut missing = final_frame(1, "hello", json!({"role": "assistant", "content": "hello"}));
    missing.as_object_mut().unwrap().remove("timings");
    assert!(matches!(
        decode_generation(&frame_lines(&[missing]), "run:1:1", None, false, false),
        Err(InferenceServiceError::InvalidRequest)
    ));

    let mut invalid = final_frame(1, "hello", json!({"role": "assistant", "content": "hello"}));
    invalid["timings"]["predicted_per_second"] = json!(-1.0);
    assert!(matches!(
        decode_generation(&frame_lines(&[invalid]), "run:1:1", None, false, false),
        Err(InferenceServiceError::InvalidResult)
    ));
}

#[test]
fn private_stream_strips_gemma_channel_wrappers_from_raw_output() {
    let raw = "<|channel>thought\n<channel|>visible answer";
    let bytes = frame_lines(&[
        json!({
            "schema": "agentlibre.llama-stream/v1",
            "attempt_id": "run:1:1",
            "sequence": 1,
            "kind": "delta",
            "content": raw
        }),
        final_frame(
            2,
            raw,
            json!({"role": "assistant", "content": "visible answer"}),
        ),
    ]);

    let generated = decode_generation(&bytes, "run:1:1", None, false, false).unwrap();

    assert_eq!(
        generated.output,
        ModelGenerationOutput::Assistant(Content::text("visible answer").unwrap())
    );
}

#[test]
fn reasoning_stream_buffers_private_content_for_internal_persistence() {
    let raw = "<think>private scratch</think>public answer";
    let bytes = frame_lines(&[
        json!({
            "schema": "agentlibre.llama-stream/v1",
            "attempt_id": "run:1:1",
            "sequence": 1,
            "kind": "delta",
            "content": raw
        }),
        final_frame(
            2,
            raw,
            json!({
                "role": "assistant",
                "reasoning_content": "private scratch",
                "content": "public answer"
            }),
        ),
    ]);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let sink_observed = observed.clone();
    let sink = InferenceProgressSink::new(move |content| {
        sink_observed.lock().unwrap().push(content);
    });

    let generated = decode_generation(&bytes, "run:1:1", Some(&sink), false, true).unwrap();

    assert_eq!(
        generated.output,
        ModelGenerationOutput::Assistant(Content::text("public answer").unwrap())
    );
    assert_eq!(
        generated.private_reasoning,
        Some(Content::text("private scratch").unwrap())
    );
    assert!(observed.lock().unwrap().is_empty());
}

#[test]
fn reasoning_projection_preserves_text_next_to_a_tool_call() {
    let message = json!({
        "role": "assistant",
        "reasoning_content": "private scratch",
        "content": "answer",
        "tool_calls": [{"function": {"name": "agentlibre.builtins:fs_read", "arguments": "{}"}}]
    });
    let (output, reasoning) = projected_reasoning_output(Some(&message)).unwrap();
    let ModelGenerationOutput::AssistantToolCall(mixed) = output else {
        panic!("expected mixed assistant and tool output")
    };
    assert_eq!(mixed.content.as_text(), "answer");
    assert_eq!(mixed.call.tool_id.as_str(), "agentlibre.builtins:fs_read");
    assert_eq!(reasoning.unwrap().as_text(), "private scratch");
    assert_eq!(
        projected_reasoning_output(Some(&json!({
            "role": "assistant",
            "reasoning_content": "private scratch",
            "content": ""
        })))
        .unwrap_err(),
        ModelMessageError::field("message.public_action.none")
    );
}

#[test]
fn reasoning_projection_reports_safe_message_shape_diagnostics() {
    let cases = [
        (
            json!({"role":"assistant", "content":null, "tool_calls":[]}),
            "message.tool_calls.count=0",
        ),
        (
            json!({"role":"assistant", "content":null, "tool_calls":[{}, {}]}),
            "message.tool_calls[0].function",
        ),
        (
            json!({"role":"assistant", "content":7}),
            "message.content.type",
        ),
        (
            json!({"role":"assistant", "content":null, "tool_calls":[{"function":{"name":"read","arguments":"{"}}]}),
            "message.tool_calls[0].function.arguments.json",
        ),
    ];
    for (message, expected) in cases {
        assert_eq!(
            projected_reasoning_output(Some(&message)).unwrap_err(),
            ModelMessageError::field(expected)
        );
    }
}

#[test]
fn private_stream_rejects_sequence_and_raw_output_mismatches() {
    let sequence_mismatch = frame_lines(&[final_frame(
        2,
        "hello",
        json!({"role": "assistant", "content": "hello"}),
    )]);
    assert!(matches!(
        decode_generation(&sequence_mismatch, "run:1:1", None, false, false),
        Err(InferenceServiceError::IdentityMismatch)
    ));

    let raw_mismatch = frame_lines(&[
        json!({
            "schema": "agentlibre.llama-stream/v1",
            "attempt_id": "run:1:1",
            "sequence": 1,
            "kind": "delta",
            "content": "hello"
        }),
        final_frame(
            2,
            "different",
            json!({"role": "assistant", "content": "different"}),
        ),
    ]);
    assert!(matches!(
        decode_generation(&raw_mismatch, "run:1:1", None, false, false),
        Err(InferenceServiceError::InvalidResult)
    ));
}

#[test]
fn private_stream_accepts_multiple_structured_tool_calls() {
    let raw = r#"{"tool":"agentlibre.builtins:fs_read","arguments":{"path":"a"}}"#;
    let bytes = frame_lines(&[
        json!({
            "schema": "agentlibre.llama-stream/v1",
            "attempt_id": "run:1:1",
            "sequence": 1,
            "kind": "delta",
            "content": raw
        }),
        final_frame(
            2,
            raw,
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"function": {"name": "agentlibre.builtins:fs_read", "arguments": "{\"path\":\"a\"}"}},
                    {"function": {"name": "agentlibre.builtins:fs_read", "arguments": "{\"path\":\"b\"}"}}
                ]
            }),
        ),
    ]);

    let generated = decode_generation(&bytes, "run:1:1", None, false, false).unwrap();
    let ModelGenerationOutput::ToolCalls(calls) = generated.output else {
        panic!("expected a structured tool call batch")
    };
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].tool_id.as_str(), "agentlibre.builtins:fs_read");
    assert_eq!(calls[1].input["path"], "b");
}

#[test]
fn invalid_empty_and_oversized_answers_keep_diagnostics_without_raw_content() {
    for raw in [String::new(), "x".repeat(1_048_577)] {
        let mut frames = Vec::new();
        if !raw.is_empty() {
            frames.push(json!({"schema":"agentlibre.llama-stream/v1", "attempt_id":"run:1:1", "sequence":1, "kind":"delta", "content":raw}));
        }
        frames.push(final_frame(
            frames.len() as u64 + 1,
            &raw,
            json!({"role":"assistant", "content":raw}),
        ));
        let bytes = frame_lines(&frames);
        let Err(InferenceServiceError::InvalidModelOutput(output)) =
            decode_generation(&bytes, "run:1:1", None, false, false)
        else {
            panic!("expected invalid answer diagnostic")
        };
        assert!(output.raw_output.is_none());
        assert_eq!(
            output.diagnostic.class,
            ModelOutputFailureClass::InvalidContent
        );
        assert_eq!(output.diagnostic.output_bytes, raw.len() as u64);
        assert_eq!(
            output.diagnostic.output_digest,
            PackageDigest::from_bytes(Sha256::digest(raw.as_bytes()).into())
        );
    }
}

#[test]
fn cancellation_requires_a_terminal_error_frame() {
    let terminal_error = frame_lines(&[json!({
        "schema": "agentlibre.llama-stream/v1",
        "attempt_id": "run:1:1",
        "sequence": 1,
        "kind": "error",
        "error": {"message": "cancelled"}
    })]);
    assert!(matches!(
        decode_generation(&terminal_error, "run:1:1", None, true, false),
        Err(InferenceServiceError::Cancelled)
    ));
    assert!(matches!(
        decode_generation(&[], "run:1:1", None, true, false),
        Err(InferenceServiceError::OutcomeUnknown)
    ));
    let completed_after_cancellation = frame_lines(&[
        json!({
            "schema": "agentlibre.llama-stream/v1",
            "attempt_id": "run:1:1",
            "sequence": 1,
            "kind": "delta",
            "content": "too late"
        }),
        final_frame(
            2,
            "too late",
            json!({"role": "assistant", "content": "too late"}),
        ),
    ]);
    assert!(matches!(
        decode_generation(&completed_after_cancellation, "run:1:1", None, true, false),
        Err(InferenceServiceError::OutcomeUnknown)
    ));
    assert!(!error_requires_engine_restart(
        &InferenceServiceError::Cancelled
    ));
    assert!(!error_requires_engine_restart(
        &InferenceServiceError::InvalidRequest
    ));
    assert!(error_requires_engine_restart(
        &InferenceServiceError::OutcomeUnknown
    ));
}
