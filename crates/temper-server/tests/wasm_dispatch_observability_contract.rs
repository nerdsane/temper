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

/// ARN-243 wiring contract: per-tenant LLM content redaction must run before
/// every telemetry sink reads the callback params. A refactor that moved the
/// strip after any sink would leak prompts/completions for non-opted-in
/// tenants — this guards the ordering that unit tests on the helper cannot.
/// See ADR-0166.
#[test]
fn llm_content_redaction_precedes_every_dispatch_sink() {
    let src = WASM_DISPATCH_SOURCE;
    let strip = src
        .find("// ARN-243: redact LLM content")
        .expect("dispatch must redact LLM content before recording telemetry");

    // Every sink below reads content from `result.callback_params`; each must
    // come after the strip so it observes the redacted map.
    let sinks = [
        "let callback_params = &result.callback_params;",
        "llm_call_wide_event(",
        "submit_llmobs_llm_span(",
        "submit_llmobs_tool_spans(",
    ];
    for sink in sinks {
        let at = src
            .find(sink)
            .unwrap_or_else(|| panic!("expected dispatch telemetry sink `{sink}`"));
        assert!(
            strip < at,
            "LLM content redaction (byte {strip}) must precede sink `{sink}` (byte {at})"
        );
    }
}
