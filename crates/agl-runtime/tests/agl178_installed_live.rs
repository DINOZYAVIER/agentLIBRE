use agl_runtime::test_support::InstalledProductFixture;

// AGL178-LIVE-001. This is intentionally an ignored hardware/systemd test in
// the core suite. The dedicated live runner executes it on the real discrete
// Vulkan host after both repositories are installed from their exact pins.
#[test]
#[ignore = "requires a real user systemd manager, installed 32K model and discrete Vulkan GPU"]
fn fresh_installed_product_runs_terminal_effect_and_32k_vulkan_inference() {
    let fixture = InstalledProductFixture::from_environment().unwrap();
    let result = fixture.run_fresh_install_acceptance().unwrap();

    assert!(result.used_real_user_systemd());
    assert!(result.cold_socket_activation_succeeded());
    assert!(result.bare_cli_succeeded());
    assert!(result.durable_session_lifecycle_succeeded());
    assert!(result.terminal_process_effect_succeeded());
    assert_eq!(result.workspace_schema(), "agentlibre.workspace/v3");
    assert_eq!(result.package_lock_schema(), "agentlibre.package-lock/v1");
    assert!(result.context_tokens() >= 32 * 1024);
    assert_eq!(result.namespace_device_name(), "Vulkan0");
    assert!(result.selected_discrete_device_matches_receipt());
    assert!(result.gpu_offload_is_positive());
    assert!(!result.cpu_fallback_attempted());
    assert!(result.agent_runtime_identity_is_sealed());
    assert!(result.terminal_manifest_and_process_identity_are_recorded());
    assert!(result.engine_generation_and_protocol_are_recorded());

    let restarted = result.restart_services().unwrap();
    assert!(restarted.durable_state_preserved());
    assert!(restarted.installed_generations_preserved());
    assert!(restarted.live_process_generations_rotated());
}
