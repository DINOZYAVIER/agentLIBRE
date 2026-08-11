use agl_inference::test_support::{
    CacheMutation, CancellationFixture, DescriptorMutation, EngineEnvironmentFixture,
    EngineFixture, EngineProtocolMutation, EngineRoute, EngineRuntimeMutation, MediaMutation,
    OutputCase, PromptMutation, ReadinessMutation, ToolFixture,
};
use agl_inference::{InferenceAttemptPhase, InferenceFailure};

// MIW-PROTO-001. The private framing and event sequence are bounded, closed
// and attempt-correlated; every malformed stream reconciles once.
#[test]
fn hostile_private_protocol_input_fails_closed_once() {
    for mutation in EngineProtocolMutation::all_hostile_cases() {
        let fixture = EngineFixture::canonical().mutate_protocol(mutation);
        assert!(matches!(
            fixture.run(),
            Err(InferenceFailure::EngineProtocol { .. })
        ));
        assert_eq!(fixture.terminal_record_count(), 1, "{mutation:?}");
        assert_eq!(fixture.unreconciled_process_count(), 0);
        assert_eq!(fixture.unreconciled_reservation_bytes(), 0);
    }
}

// MIW-FD-001 and MIW-ENG-002. Verification and engine load share one open
// description even after the source path is renamed/replaced.
#[test]
fn engine_reads_the_host_verified_inode_not_a_reopened_path() {
    let fixture = EngineFixture::canonical();
    let expected = fixture.original_model_bytes();
    fixture.pause_after_descriptor_verification();
    fixture.replace_every_source_path();
    fixture.resume();
    fixture.run().unwrap();
    assert_eq!(fixture.engine_loaded_bytes(), expected);
    assert_ne!(fixture.current_source_path_bytes(), expected);
    assert!(fixture.engine_input().source_paths().is_empty());
}

// MIW-FD-002. Main/projector/draft role, order, size and plan identity are all
// checked before native load.
#[test]
fn descriptor_set_matches_every_planned_role_exactly() {
    for mutation in DescriptorMutation::missing_duplicate_swapped_reordered_and_wrong_size() {
        let fixture = EngineFixture::all_roles().mutate_descriptors(mutation);
        assert!(matches!(
            fixture.run(),
            Err(InferenceFailure::DescriptorSet { .. })
        ));
        assert_eq!(fixture.engine_model_load_count(), 0);
    }
}

// MIW-FD-003. A file changing while it is hashed is rejected before admission
// and dispatch.
#[test]
fn descriptor_metadata_mutation_during_hash_is_terminal() {
    let fixture = EngineFixture::canonical().mutate_during_hash();
    assert!(matches!(
        fixture.run(),
        Err(InferenceFailure::DescriptorChanged { .. })
    ));
    assert_eq!(fixture.admission_count(), 0);
    assert_eq!(fixture.engine_dispatch_count(), 0);
}

// MIW-FD-004. Native descriptors live through the model generation and close
// only after its exact unload receipt.
#[test]
fn descriptor_lifetime_is_bound_to_matching_model_unload() {
    let fixture = EngineFixture::canonical();
    fixture.load_model().unwrap();
    assert!(fixture.all_model_descriptors_open());
    fixture.send_foreign_unload_receipt();
    assert!(fixture.all_model_descriptors_open());
    fixture.acknowledge_exact_unload();
    assert!(fixture.all_model_descriptors_closed());
    assert_eq!(fixture.descriptor_leak_count(), 0);
}

// MIW-FD-005. Split shards are exposed by their safe planned basenames through
// descriptor-backed storage and remain loadable after source replacement.
#[test]
fn split_gguf_loads_only_from_descriptor_backed_basenames() {
    let fixture = EngineFixture::split_gguf();
    let basenames = fixture.planned_shard_basenames();
    fixture.replace_every_source_path();
    fixture.run().unwrap();
    assert_eq!(fixture.engine_shard_basenames(), basenames);
    assert!(fixture.engine_source_directory_lookups().is_empty());
}

// MIW-WRK-001. Private engine input has no host paths/public job and cannot
// manufacture plan, journal or admission evidence.
#[test]
fn private_engine_input_is_capability_narrow() {
    let fixture = EngineFixture::canonical();
    let input = fixture.engine_input();
    assert!(input.source_paths().is_empty());
    assert!(input.public_inference_job().is_none());
    assert!(input.plan().is_none());
    assert!(input.admission_receipt().is_none());
    assert!(input.journal_handle().is_none());
    assert!(input.descriptor_ordinals().len() > 0);
}

// MIW-WRK-002. Lazy Tool grammar commits the same constrained token stream
// with and without MTP and retains parser/repair evidence.
#[test]
fn lazy_tool_grammar_is_equivalent_with_mtp_enabled() {
    let ordinary = ToolFixture::lazy_grammar().mtp(false).run().unwrap();
    let speculative = ToolFixture::lazy_grammar().mtp(true).run().unwrap();
    assert_eq!(ordinary.committed_tokens(), speculative.committed_tokens());
    assert_eq!(ordinary.raw_output(), speculative.raw_output());
    assert_eq!(ordinary.parsed_output(), speculative.parsed_output());
    assert!(ordinary.grammar_evidence().is_some());
    assert!(speculative.grammar_evidence().is_some());
    assert!(speculative.parser_evidence().is_some());
    assert!(speculative.repair_evidence().is_some());
}

// MIW-ENG-001 and MIW-ENG-007. Only the sealed subordinate executable starts,
// on inherited private descriptors, with no reachable stock route/socket.
#[test]
fn exact_server_generation_exposes_only_versioned_agl_operations() {
    let fixture = EngineFixture::canonical();
    fixture.run().unwrap();
    assert_eq!(
        fixture.started_executable_digest(),
        fixture.sealed_executable_digest()
    );
    assert!(fixture.path_lookup_attempts().is_empty());
    assert!(fixture.tcp_listeners().is_empty());
    assert!(fixture.named_sockets().is_empty());
    assert!(fixture.private_data_descriptor_inherited());
    assert!(fixture.private_control_descriptor_inherited());
    for route in EngineRoute::stock_public_routes() {
        assert!(!fixture.route_reachable(route), "{route:?}");
    }
}

// MIW-ENG-003. Exact v3 settings reach the server unchanged and automatic
// fit/router/download behavior cannot alter them.
#[test]
fn engine_launch_and_request_match_the_frozen_plan_exactly() {
    let fixture = EngineFixture::canonical();
    fixture.run().unwrap();
    assert_eq!(
        fixture.engine_runtime_projection(),
        fixture.plan_runtime_projection()
    );
    assert_eq!(fixture.engine_device(), fixture.plan_device());
    assert_eq!(fixture.engine_context_shape(), fixture.plan_context_shape());
    assert!(!fixture.auto_fit_enabled());
    assert!(!fixture.router_enabled());
    assert!(!fixture.download_enabled());
}

// MIW-ENG-004. Readiness/allocation/unload facts are typed receipts bound to
// plan, reservation and generation; health/log text cannot synthesize them.
#[test]
fn engine_receipts_are_typed_and_identity_bound() {
    let fixture = EngineFixture::canonical();
    fixture.run().unwrap();
    for receipt in fixture.receipts() {
        assert_eq!(receipt.plan_digest(), fixture.plan_digest());
        assert_eq!(receipt.reservation_id(), fixture.reservation_id());
        assert_eq!(receipt.engine_generation(), fixture.engine_generation());
    }
    for unsupported in EngineFixture::health_and_log_only_receipt_cases() {
        assert!(unsupported.run().is_err());
    }
}

// MIW-ENG-005. Bounded streaming and every engine termination cause exactly
// one matching journal terminal.
#[test]
fn streaming_cancel_crash_and_device_loss_each_finish_once() {
    for fixture in EngineFixture::stream_terminal_cases() {
        let expected = fixture.expected_terminal_phase();
        let _ = fixture.run();
        assert_eq!(fixture.attempt_phase(), expected);
        assert_eq!(fixture.terminal_record_count(), 1);
    }
}

// MIW-ENG-006 and MIW-CACHE-001. AGL chooses the single slot deterministically
// from exact ContextKey/prefix identity; similarity and stale media never win.
#[test]
fn exact_context_key_and_prefix_control_cache_reuse() {
    let exact = EngineFixture::exact_context_reuse();
    exact.run().unwrap();
    assert!(exact.context_reused());
    assert_eq!(exact.engine_slot_id(), exact.host_selected_slot_id());

    for mutation in CacheMutation::text_tools_image_and_prefix_drift() {
        let fixture = EngineFixture::exact_context_reuse().mutate_cache_input(mutation);
        fixture.run().unwrap();
        assert!(!fixture.context_reused(), "{mutation:?}");
        assert!(fixture.full_transcript_rebuilt());
    }
    let overflow = EngineFixture::context_overflow();
    assert!(matches!(
        overflow.run(),
        Err(InferenceFailure::ContextOverflow { .. })
    ));
    assert!(!overflow.context_shifted_or_trimmed());
}

// MIW-ENG-008. Every request/process resource bound is enforced by AGL without
// partial success.
#[test]
fn server_body_output_log_fd_and_thread_bounds_are_total() {
    for fixture in EngineFixture::every_bound_exceeded() {
        assert!(fixture.run().is_err(), "{}", fixture.label());
        assert_ne!(fixture.attempt_phase(), InferenceAttemptPhase::Succeeded);
        assert_eq!(fixture.terminal_record_count(), 1);
        assert!(fixture.observed_value() <= fixture.kill_or_reject_bound());
    }
}

// MIW-ENG-009. Cancellation requires the matching typed ack; timeout reaps the
// generation and records Cancelled once.
#[test]
fn cancellation_acknowledgement_is_attempt_correlated_and_bounded() {
    let exact = CancellationFixture::matching_ack();
    exact.run().unwrap();
    assert_eq!(exact.attempt_phase(), InferenceAttemptPhase::Cancelled);
    assert_eq!(exact.terminal_record_count(), 1);

    for fixture in [
        CancellationFixture::foreign_ack(),
        CancellationFixture::no_ack(),
    ] {
        fixture.run().unwrap();
        assert_eq!(fixture.child_reap_count(), 1);
        assert_eq!(fixture.attempt_phase(), InferenceAttemptPhase::Cancelled);
        assert_eq!(fixture.terminal_record_count(), 1);
        assert!(!fixture.late_completion_accepted());
    }
}

// MIW-ENG-010. Typed readiness reports actual allocated shape; any cap, fit,
// disabled speculative setup or other drift kills startup.
#[test]
fn typed_readiness_must_equal_the_admitted_shape() {
    let ready = EngineFixture::canonical();
    ready.start().unwrap();
    assert_eq!(
        ready.readiness().context_tokens(),
        ready.plan_context_tokens()
    );
    assert_eq!(ready.readiness().batch_size(), ready.plan_batch_size());
    assert_eq!(ready.readiness().cache_types(), ready.plan_cache_types());
    assert_eq!(ready.readiness().device(), ready.plan_device());
    assert_eq!(ready.readiness().mtp(), ready.plan_mtp());
    assert_eq!(ready.readiness().allocated_bytes(), ready.receipted_bytes());
    for mutation in ReadinessMutation::all_drift_cases() {
        assert!(
            EngineFixture::canonical()
                .mutate_readiness(mutation)
                .start()
                .is_err()
        );
    }
}

// MIW-ENG-011. Hidden server authority stays disabled regardless of upstream
// or template defaults.
#[test]
fn reasoning_shift_sleep_parallel_fit_and_remote_media_are_disabled() {
    for mutation in EngineRuntimeMutation::hidden_authority_features() {
        let fixture = EngineFixture::canonical().enable_runtime_mutation(mutation);
        assert!(fixture.start().is_err(), "{mutation:?}");
    }
}

// MIW-ENG-012. The server has one fixed slot; clear is acknowledged but only a
// full process rebuild releases fixed KV memory.
#[test]
fn one_slot_clear_and_rebuild_have_exact_memory_semantics() {
    let fixture = EngineFixture::canonical();
    fixture.start().unwrap();
    assert_eq!(fixture.engine_slot_count(), 1);
    fixture.clear_context().unwrap();
    assert!(fixture.slot_clear_acknowledged());
    assert_eq!(fixture.fixed_kv_bytes_released_by_clear(), 0);
    fixture.full_rebuild().unwrap();
    assert_eq!(fixture.fixed_kv_bytes_after_rebuild(), 0);
    assert!(!fixture.invalid_slot_id_was_wrapped());
}

// MIW-ENG-013. Inventory is checked, canonical and never disrupts a healthy
// resident generation.
#[test]
fn private_inventory_is_machine_readable_and_generation_safe() {
    let resident = EngineFixture::resident_inventory();
    let generation = resident.engine_generation();
    resident.inventory().unwrap();
    assert_eq!(resident.engine_generation(), generation);
    assert_eq!(resident.replacement_count(), 0);

    let cold = EngineFixture::cold_inventory();
    cold.inventory().unwrap();
    assert_eq!(cold.short_lived_child_count(), 1);
    assert_eq!(cold.child_reap_count(), 1);
    assert!(cold.human_text_inventory_ignored());
}

// MIW-ENG-014. Idle/manual unload policy remains host-owned and completion is
// after process reap plus reservation release.
#[test]
fn unload_policy_and_completion_are_host_owned() {
    let busy = EngineFixture::busy_unload();
    assert!(matches!(busy.unload(), Err(InferenceFailure::Busy { .. })));
    let absent = EngineFixture::absent_unload();
    absent.unload().unwrap();
    absent.unload().unwrap();
    let idle = EngineFixture::idle_unload();
    idle.unload().unwrap();
    assert_eq!(idle.child_reap_count(), 1);
    assert_eq!(idle.reserved_bytes(), 0);
    assert!(!idle.server_sleep_enabled());
    assert_eq!(idle.implicit_restart_count(), 0);
}

// MIW-ENG-015. Hostile inherited environment cannot become configuration,
// secret, route or logging authority.
#[test]
fn engine_environment_is_minimal_and_structured() {
    let fixture = EngineEnvironmentFixture::hostile();
    fixture.start().unwrap();
    for name in fixture.forbidden_names() {
        assert!(!fixture.child_environment().contains_key(name), "{name}");
    }
    assert_eq!(fixture.child_argv(), fixture.plan_derived_argv());
    assert_eq!(fixture.enabled_routes(), fixture.private_agl_routes());
}

// MIW-TOOL-001. Lazy constrained generation admits plain output or one
// sequential call and records grammar/parser/repair evidence.
#[test]
fn lazy_tool_generation_is_closed_and_sequential() {
    for fixture in [ToolFixture::plain_answer(), ToolFixture::one_tool_call()] {
        let result = fixture.run().unwrap();
        assert!(result.grammar_evidence().is_some());
        assert!(result.parser_evidence().is_some());
        assert!(result.repair_evidence().is_some());
        assert!(result.tool_calls().len() <= 1);
    }
    assert!(ToolFixture::parallel_tool_calls().run().is_err());
}

// MIW-TOOL-002 and MIW-TOOL-003. Native parsing is advisory: AGL validates raw
// parity, schema and permission before constructing ParsedModelOutput.
#[test]
fn agl_retains_tool_validation_permission_and_output_authority() {
    for fixture in [
        ToolFixture::schema_invalid(),
        ToolFixture::permission_denied(),
        ToolFixture::raw_projection_disagreement(),
    ] {
        assert!(fixture.run().is_err());
        assert_eq!(fixture.executed_action_count(), 0);
    }
    let valid = ToolFixture::one_tool_call().run().unwrap();
    assert_eq!(
        valid.agl_parsed_output().raw_bytes(),
        valid.engine_raw_bytes()
    );
    assert_eq!(
        valid.agl_parsed_output().projection(),
        valid.engine_projection()
    );
    assert!(!valid.verbose_prompt_logging_enabled());
}

// MIW-OUT-001. Native reasons map to the exact four terminal outcomes once.
#[test]
fn engine_stop_reasons_map_to_exact_attempt_terminals() {
    for case in OutputCase::all() {
        let fixture = EngineFixture::output_case(case);
        let _ = fixture.run();
        assert_eq!(fixture.attempt_phase(), case.expected_phase());
        assert_eq!(fixture.terminal_record_count(), 1);
    }
}

// MIW-PROMPT-001. Engine-neutral input renders through the pinned template
// exactly once, with stable prompt/template/Tool identities and no thinking.
#[test]
fn prompt_template_and_visible_tools_match_golden_bytes() {
    let golden = EngineFixture::prompt_golden();
    golden.run().unwrap();
    assert_eq!(
        golden.rendered_prompt_bytes(),
        golden.expected_prompt_bytes()
    );
    assert_eq!(golden.prompt_digest(), golden.expected_prompt_digest());
    assert_eq!(golden.template_digest(), golden.expected_template_digest());
    assert_eq!(golden.tool_digest(), golden.expected_tool_digest());
    assert_eq!(golden.tool_schema_injection_count(), 1);
    assert!(!golden.thinking_present());
    for mutation in PromptMutation::ordering_template_and_duplicate_tools() {
        assert!(
            EngineFixture::prompt_golden()
                .mutate_prompt(mutation)
                .run()
                .is_err()
        );
    }
}

// MIW-MEDIA-001. Media is bounded binary multipart, ordered and volatile; no
// path/URL/base64 or unsupported content reaches the engine.
#[test]
fn vision_media_transport_is_bounded_binary_and_volatile() {
    let fixture = EngineFixture::vision_multipart();
    fixture.run().unwrap();
    assert_eq!(fixture.engine_part_order(), fixture.request_part_order());
    assert_eq!(
        fixture.engine_media_identity(),
        fixture.resolved_media_identity()
    );
    assert!(fixture.engine_media_paths().is_empty());
    assert!(fixture.engine_media_urls().is_empty());
    assert!(fixture.engine_base64_fields().is_empty());
    assert_eq!(fixture.durable_private_media_bytes(), 0);
    for mutation in MediaMutation::oversized_foreign_audio_video_nonvision_and_mtp() {
        let rejected = EngineFixture::vision_multipart().mutate_media(mutation);
        assert!(rejected.run().is_err());
        assert_eq!(rejected.engine_media_bytes_seen(), 0);
    }
}

// MIW-OBS-001 and MIW-ID-001. Product backend remains llama_cpp while typed
// evidence binds the private implementation generation and never logs payload
// sentinels.
#[test]
fn observation_and_runtime_identity_are_typed_and_payload_free() {
    let fixture = EngineFixture::with_log_sentinels();
    fixture.run().unwrap();
    let evidence = fixture.evidence();
    assert_eq!(evidence.backend(), "llama_cpp");
    assert_eq!(evidence.device(), fixture.plan_device());
    assert_eq!(evidence.prefill_tokens(), fixture.actual_prefill_tokens());
    assert_eq!(evidence.configured_batch(), fixture.plan_batch_size());
    assert_eq!(evidence.actual_chunks(), fixture.actual_chunks());
    assert_eq!(evidence.closed_native_stages(), fixture.native_stages());
    assert!(fixture.runtime_log_bytes().len() <= 4 * 1024 * 1024);
    assert!(!fixture.runtime_log_contains_sentinels());

    let identity = evidence.runtime_identity();
    assert_eq!(
        identity.server_executable_digest(),
        fixture.sealed_executable_digest()
    );
    assert_eq!(
        identity.native_closure_digest(),
        fixture.native_closure_digest()
    );
    assert_eq!(
        identity.llama_cpp_commit(),
        fixture.pinned_llama_cpp_commit()
    );
    assert_eq!(
        identity.private_patch_digest(),
        fixture.private_patch_digest()
    );
    assert_eq!(identity.protocol_identity(), fixture.protocol_identity());
    assert!(
        !identity
            .as_canonical_string()
            .contains("agl-inference-worker")
    );
}
