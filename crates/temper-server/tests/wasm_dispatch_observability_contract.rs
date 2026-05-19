//! Observability contract tests for WASM dispatch attribution.

const WASM_DISPATCH_SOURCE: &str = include_str!("../src/state/dispatch/wasm.rs");

#[test]
fn wasm_dispatch_emits_integration_envelope_phase_spans() {
    let required_phases = [
        "dispatch.wasm.phase.module_cache",
        "dispatch.wasm.phase.replay_input_injection",
        "dispatch.wasm.phase.invocation_context_build",
        "dispatch.wasm.phase.blob_ref_hydration",
        "dispatch.wasm.phase.authz_secret_resolution",
        "dispatch.wasm.phase.host_chain_build",
        "dispatch.wasm.phase.integration_observe_start",
        "dispatch.wasm.phase.engine_invoke_and_handle",
        "dispatch.wasm.phase.engine_invoke",
        "dispatch.wasm.phase.result_observe_complete",
        "dispatch.wasm.phase.record_invocation",
        "dispatch.wasm.phase.dispatch_callback",
        "dispatch.wasm.phase.llmobs_submit",
    ];

    for phase in required_phases {
        assert!(
            WASM_DISPATCH_SOURCE.contains(phase),
            "missing required WASM dispatch observability phase span: {phase}"
        );
    }
}
