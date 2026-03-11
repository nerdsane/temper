//! End-to-end integration tests for WasmEngine invoke.
//!
//! Exercises the full compile → instantiate → run path using a real WASM
//! module (`echo_integration.wasm`) built from `crates/temper-wasm/tests/fixtures/echo-integration-src`.

use std::sync::{Arc, RwLock};

use temper_wasm::{
    SimWasmHost, StreamRegistry, WasmEngine, WasmError, WasmInvocationContext, WasmResourceLimits,
};

/// Pre-built echo integration WASM binary (avoids needing wasm32 target in CI).
const ECHO_WASM: &[u8] = include_bytes!("fixtures/echo_integration.wasm");

fn build_context() -> WasmInvocationContext {
    WasmInvocationContext {
        tenant: "test".to_string(),
        entity_type: "EchoTest".to_string(),
        entity_id: "e1".to_string(),
        trigger_action: "TriggerEcho".to_string(),
        trigger_params: serde_json::json!({}),
        entity_state: serde_json::json!({"status": "Pending"}),
        agent_id: None,
        session_id: None,
        integration_config: std::collections::BTreeMap::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn invoke_echo_module_end_to_end() {
    let engine = WasmEngine::new().expect("engine should create");

    // Compile and cache
    let hash = engine
        .compile_and_cache(ECHO_WASM)
        .expect("echo module should compile");
    assert!(!hash.is_empty(), "hash should not be empty");
    assert!(engine.is_cached(&hash), "module should be cached");

    // Build context and host
    let ctx = build_context();
    let host = Arc::new(
        SimWasmHost::new()
            .with_response("https://echo.example.com/ping", 200, "pong")
            .with_secret("ECHO_API_KEY", "test-secret-key"),
    );

    // Invoke
    let streams = Arc::new(RwLock::new(StreamRegistry::default()));
    let result = engine
        .invoke(&hash, &ctx, host, &WasmResourceLimits::default(), streams)
        .await
        .expect("invoke should succeed");

    // Assert result
    assert!(result.success, "result should be successful");
    assert_eq!(
        result.callback_action, "EchoSucceeded",
        "callback action should be EchoSucceeded"
    );
    // duration_ms is u64 so always >= 0; just verify it was measured
    assert!(
        result.duration_ms < 30_000,
        "should complete well within timeout"
    );

    // Verify callback params contain the expected fields
    let params = &result.callback_params;
    assert!(
        params.get("echo_context_len").is_some(),
        "params should have echo_context_len"
    );
    assert!(
        params.get("http_response").is_some(),
        "params should have http_response"
    );

    // The HTTP response should contain the SimWasmHost response ("200\npong")
    let http_resp = params["http_response"].as_str().unwrap_or("");
    assert!(
        http_resp.contains("pong"),
        "HTTP response should contain 'pong', got: {http_resp}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn invoke_missing_module_returns_error() {
    let engine = WasmEngine::new().expect("engine should create");
    let ctx = build_context();
    let host = Arc::new(SimWasmHost::new());

    let streams = Arc::new(RwLock::new(StreamRegistry::default()));
    let result = engine
        .invoke(
            "nonexistent_hash_abc123",
            &ctx,
            host,
            &WasmResourceLimits::default(),
            streams,
        )
        .await;

    assert!(result.is_err(), "should error for missing module");
    match result.unwrap_err() {
        WasmError::ModuleNotFound(hash) => {
            assert_eq!(hash, "nonexistent_hash_abc123");
        }
        other => panic!("expected ModuleNotFound, got: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn invoke_with_http_failure_still_succeeds() {
    // The echo module handles HTTP failure gracefully — returns "-1\n" as response
    let engine = WasmEngine::new().expect("engine should create");
    let hash = engine.compile_and_cache(ECHO_WASM).expect("should compile");

    let ctx = build_context();
    // Use a host that returns errors for HTTP calls
    let host = Arc::new(SimWasmHost::new().with_default_response(500, "internal error"));

    let streams = Arc::new(RwLock::new(StreamRegistry::default()));
    let result = engine
        .invoke(&hash, &ctx, host, &WasmResourceLimits::default(), streams)
        .await
        .expect("invoke should succeed even with HTTP error response");

    assert!(result.success, "echo module handles HTTP errors gracefully");
    assert_eq!(result.callback_action, "EchoSucceeded");
}

#[test]
fn compile_caches_by_hash() {
    let engine = WasmEngine::new().expect("engine should create");

    let hash1 = engine.compile_and_cache(ECHO_WASM).expect("first compile");
    let hash2 = engine
        .compile_and_cache(ECHO_WASM)
        .expect("second compile (cached)");

    assert_eq!(hash1, hash2, "same bytes should produce same hash");
    assert_eq!(engine.cache_size(), 1, "should only cache once");
}
