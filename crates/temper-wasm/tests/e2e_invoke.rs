//! End-to-end integration tests for WasmEngine invoke.
//!
//! Exercises the full compile → instantiate → run path using a real WASM
//! module (`echo_integration.wasm`) built from `crates/temper-wasm/tests/fixtures/echo-integration-src`.

use std::sync::{Arc, RwLock};

use base64::Engine;
use sha2::{Digest, Sha256};
use temper_wasm::{
    SimWasmHost, StreamRegistry, WasmEngine, WasmError, WasmInvocationContext, WasmResourceLimits,
};

/// Pre-built echo integration WASM binary (avoids needing wasm32 target in CI).
const ECHO_WASM: &[u8] = include_bytes!("fixtures/echo_integration.wasm");
/// Pre-built SDK-backed module that exercises `temper_wasm_sdk::Context::from_host`.
const SDK_CONTEXT_READER_WASM: &[u8] = include_bytes!("fixtures/sdk_context_reader.wasm");
const WAT_DATA_RESPONSE_LIFECYCLE: &str = r#"
    (module
      (import "env" "host_temper_data_call" (func $call (param i32 i32) (result i64)))
      (import "env" "host_temper_data_response_len" (func $len (param i32) (result i32)))
      (import "env" "host_temper_data_response_read" (func $read (param i32 i32 i32 i32) (result i32)))
      (import "env" "host_temper_data_response_close" (func $close (param i32) (result i32)))
      (import "env" "host_set_result" (func $result (param i32 i32)))
      (memory (export "memory") 1)
      (data (i32.const 1024) "x")
      (func (export "run") (param i32 i32) (result i32)
        (local $handle i32) (local $length i32)
        i32.const 1024 i32.const 1 call $call i32.wrap_i64 local.set $handle
        local.get $handle call $len local.set $length
        local.get $handle i32.const 0 i32.const 4096 local.get $length call $read drop
        local.get $handle call $close drop
        i32.const 4096 local.get $length call $result
        i32.const 0))
"#;

#[tokio::test]
async fn data_response_handle_read_and_close_lifecycle() {
    let engine = WasmEngine::new().unwrap();
    let hash = engine
        .compile_and_cache(WAT_DATA_RESPONSE_LIFECYCLE.as_bytes())
        .unwrap();
    let response =
        br#"{"action":"DataCompleted","params":{"via":"direct_abi"},"success":true}"#.to_vec();
    let host = Arc::new(SimWasmHost::new().with_data_response(response));
    let streams = Arc::new(RwLock::new(StreamRegistry::default()));
    let result = engine
        .invoke(
            &hash,
            &build_context(),
            host,
            &WasmResourceLimits::default(),
            streams,
        )
        .await
        .unwrap();
    assert!(result.success);
    assert_eq!(result.callback_action, "DataCompleted");
    assert_eq!(result.callback_params["via"], "direct_abi");
}

fn build_context() -> WasmInvocationContext {
    WasmInvocationContext {
        tenant: "test".to_string(),
        entity_type: "EchoTest".to_string(),
        entity_id: "e1".to_string(),
        trigger_action: "TriggerEcho".to_string(),
        wasm_module: Some("echo_integration".to_string()),
        trigger_params: serde_json::json!({}),
        entity_state: serde_json::json!({"status": "Pending"}),
        agent_id: None,
        session_id: None,
        integration_config: std::collections::BTreeMap::new(),
        trace_id: String::new(),
        workflow_root_entity_type: None,
        workflow_root_entity_id: None,
        workflow_run_id: None,
        http_request: None,
    }
}

fn build_large_context(blob_len: usize) -> WasmInvocationContext {
    let mut ctx = build_context();
    ctx.entity_state = serde_json::json!({
        "status": "Pending",
        "large_blob": "x".repeat(blob_len),
    });
    ctx
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

#[tokio::test(flavor = "multi_thread")]
async fn invoke_sdk_module_with_large_context_succeeds() {
    let engine = WasmEngine::new().expect("engine should create");
    let hash = engine
        .compile_and_cache(SDK_CONTEXT_READER_WASM)
        .expect("sdk context reader should compile");

    let ctx = build_large_context(4_000_000);
    let host = Arc::new(SimWasmHost::new());

    let streams = Arc::new(RwLock::new(StreamRegistry::default()));
    let result = engine
        .invoke(&hash, &ctx, host, &WasmResourceLimits::default(), streams)
        .await
        .expect("invoke should complete");

    assert!(
        result.success,
        "sdk-backed module should read oversized invocation contexts successfully"
    );
    assert_eq!(result.callback_action, "callback");
    assert_eq!(
        result.callback_params["trigger_action"].as_str(),
        Some("TriggerEcho")
    );
    assert!(
        result.callback_params["entity_state_len"]
            .as_u64()
            .unwrap_or_default()
            > 4_000_000,
        "entity state should include the large payload"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "debug helper for real Genesis pack ingestion"]
async fn invoke_scm_ingest_pack_from_env() {
    let wasm_path = std::env::var("SCM_INGEST_WASM").expect("SCM_INGEST_WASM must be set");
    let pack_path = std::env::var("SCM_INGEST_PACK").expect("SCM_INGEST_PACK must be set");
    let head_sha = std::env::var("SCM_INGEST_HEAD")
        .unwrap_or_else(|_| "65fbd22270e4bf7304de2d9b6895a465c332d602".to_string());
    let wasm = std::fs::read(&wasm_path).expect("read wasm");
    let pack = std::fs::read(&pack_path).expect("read pack");

    let engine = WasmEngine::new().expect("engine should create");
    let hash = engine.compile_and_cache(&wasm).expect("compile scm ingest");
    let encoded_pack = base64::engine::general_purpose::STANDARD.encode(&pack);
    let serialized_pack =
        serde_json::to_vec(&serde_json::Value::String(encoded_pack)).expect("serialize pack field");
    let pack_blob_key = format!(
        "field-overflow/sha256/{:x}.json",
        Sha256::digest(&serialized_pack)
    );
    let mut integration_config = std::collections::BTreeMap::new();
    integration_config.insert(
        "temper_api_url".to_string(),
        "http://127.0.0.1:3000".to_string(),
    );
    let ctx = WasmInvocationContext {
        tenant: "default".to_string(),
        entity_type: "Repository".to_string(),
        entity_id: "rp-temperpaw-paw-agent".to_string(),
        trigger_action: "IngestPack".to_string(),
        wasm_module: Some("scm_ingest_pack".to_string()),
        trigger_params: serde_json::json!({
            "PackBytes": {
                "__temper_blob_ref": pack_blob_key,
                "__temper_blob_size": serialized_pack.len(),
                "__temper_blob_encoding": "json"
            },
            "RefUpdates": [{
                "Name": "refs/heads/main",
                "PreviousCommitSha": "0000000000000000000000000000000000000000",
                "NewCommitSha": head_sha
            }],
            "ClientRequestId": "debug-env"
        }),
        entity_state: serde_json::json!({"status": "Active"}),
        agent_id: None,
        session_id: None,
        integration_config,
        trace_id: String::new(),
        workflow_root_entity_type: None,
        workflow_root_entity_id: None,
        workflow_run_id: None,
        http_request: None,
    };

    let blob_url = format!("http://127.0.0.1:3000/_internal/blobs/{pack_blob_key}");
    let host = Arc::new(
        SimWasmHost::new()
            .with_response(
                &blob_url,
                200,
                std::str::from_utf8(&serialized_pack).expect("pack field utf8"),
            )
            .with_default_response(200, r#"{"value":[]}"#),
    );
    let streams = Arc::new(RwLock::new(StreamRegistry::default()));
    let limits = WasmResourceLimits {
        max_fuel: 20_000_000_000,
        max_memory: 1024 * 1024 * 1024,
        max_duration: std::time::Duration::from_secs(300),
        max_response_bytes: 128 * 1024 * 1024,
    };
    let result = engine
        .invoke(&hash, &ctx, host, &limits, streams)
        .await
        .expect("invoke should not trap");

    assert!(
        result.success,
        "scm ingest should succeed: {:?}",
        result.error
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&result.callback_params).unwrap()
    );
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
