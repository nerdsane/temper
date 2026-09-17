//! Context delivery must preserve memory initialized by the guest.
use std::sync::{Arc, RwLock};
use temper_wasm::{
    SimWasmHost, StreamRegistry, WasmEngine, WasmInvocationContext, WasmResourceLimits,
};

fn context(bytes: usize) -> WasmInvocationContext {
    serde_json::from_value(serde_json::json!({
        "tenant":"test", "entity_type":"Context", "entity_id":"one",
        "trigger_action":"Read", "trigger_params":{},
        "entity_state":{"payload":"x".repeat(bytes)}, "integration_config":{},
        "trace_id":""
    }))
    .unwrap()
}

#[tokio::test]
async fn host_context_reader_keeps_initialized_memory_for_every_context_size() {
    let engine = WasmEngine::new().unwrap();
    let hash = engine.compile_and_cache(br#"(module
        (import "env" "host_get_context" (func $context (param i32 i32) (result i32)))
        (import "env" "host_set_result" (func $result (param i32 i32)))
        (memory (export "memory") 32)
        (data (i32.const 1048576) "{\22action\22:\22Preserved\22,\22params\22:{},\22success\22:true}")
        (func (export "run") (param i32 i32) (result i32)
            ;; Read context through the declared host function without a guest buffer.
            i32.const 0 i32.const 0 call $context drop
            i32.const 1048576 i32.const 49 call $result
            i32.const 0))"#).unwrap();
    for size in [32, 1_000_000, 1_079_528, 3_000_000] {
        let result = engine
            .invoke(
                &hash,
                &context(size),
                Arc::new(SimWasmHost::new()),
                &WasmResourceLimits::default(),
                Arc::new(RwLock::new(StreamRegistry::default())),
            )
            .await
            .unwrap_or_else(|e| panic!("context {size}: {e}"));
        assert_eq!(result.callback_action, "Preserved", "context {size}");
    }
}

#[tokio::test]
async fn pointer_context_is_complete_and_does_not_replace_guest_data() {
    let engine = WasmEngine::new().unwrap();
    let hash = engine
        .compile_and_cache(
            br#"(module
        (import "env" "host_set_result" (func $result (param i32 i32)))
        (memory (export "memory") 1)
        (data (i32.const 1024) "{\22action\22:\22Preserved\22,\22params\22:{},\22success\22:true}")
        (func (export "run") (param $ptr i32) (param $len i32) (result i32)
            local.get $ptr i32.load8_u i32.const 123 i32.ne if unreachable end
            local.get $ptr local.get $len i32.add i32.const 1 i32.sub
            i32.load8_u i32.const 125 i32.ne if unreachable end
            i32.const 1024 i32.const 49 call $result
            i32.const 0))"#,
        )
        .unwrap();
    for size in [32, 100_000] {
        let result = engine
            .invoke(
                &hash,
                &context(size),
                Arc::new(SimWasmHost::new()),
                &WasmResourceLimits::default(),
                Arc::new(RwLock::new(StreamRegistry::default())),
            )
            .await
            .unwrap_or_else(|e| panic!("context {size}: {e}"));
        assert_eq!(result.callback_action, "Preserved", "context {size}");
    }
}

#[tokio::test]
async fn pointer_context_fails_before_guest_execution_when_memory_is_full() {
    let engine = WasmEngine::new().unwrap();
    let hash = engine
        .compile_and_cache(
            br#"(module
        (memory (export "memory") 1 1)
        (func (export "run") (param i32 i32) (result i32) unreachable))"#,
        )
        .unwrap();
    let error = engine
        .invoke(
            &hash,
            &context(100_000),
            Arc::new(SimWasmHost::new()),
            &WasmResourceLimits::default(),
            Arc::new(RwLock::new(StreamRegistry::default())),
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("cannot reserve context memory"),
        "{error}"
    );
}
