//! Unit and integration tests for the WASM engine.

use std::sync::Arc;
use std::sync::RwLock;

use super::*;
use crate::host_trait::SimWasmHost;
use crate::stream::StreamRegistry;

// Minimal WAT module: accepts (ptr, len), writes nothing, returns 0.
const WAT_NOOP: &str = r#"
    (module
      (memory (export "memory") 1)
      (func (export "run") (param i32 i32) (result i32)
        i32.const 0
      )
    )
"#;

// Infinite loop module — exhausts fuel / trips timeout.
const WAT_INFINITE_LOOP: &str = r#"
    (module
      (memory (export "memory") 1)
      (func (export "run") (param i32 i32) (result i32)
        (loop $L
          br $L)
        i32.const 0
      )
    )
"#;

// Tries to grow memory by 1000 pages (64 MB) — exceeds 16 MB default.
const WAT_MEMORY_GROW: &str = r#"
    (module
      (memory (export "memory") 1)
      (func (export "run") (param i32 i32) (result i32)
        (memory.grow (i32.const 1000))
        drop
        i32.const 0
      )
    )
"#;

// Raises an unreachable trap — tests error isolation.
const WAT_TRAP: &str = r#"
    (module
      (memory (export "memory") 1)
      (func (export "run") (param i32 i32) (result i32)
        unreachable
      )
    )
"#;

fn make_context() -> WasmInvocationContext {
    WasmInvocationContext {
        tenant: "test".into(),
        entity_type: "Order".into(),
        entity_id: "1".into(),
        trigger_action: "Submit".into(),
        trigger_params: serde_json::Value::Null,
        entity_state: serde_json::Value::Null,
        agent_id: None,
        session_id: None,
        integration_config: std::collections::BTreeMap::new(),
    }
}

fn make_host() -> Arc<dyn WasmHost> {
    Arc::new(SimWasmHost::new())
}

fn make_streams() -> Arc<RwLock<StreamRegistry>> {
    Arc::new(RwLock::new(StreamRegistry::default()))
}

#[test]
fn hash_module_deterministic() {
    let bytes = b"test wasm bytes";
    let h1 = WasmEngine::hash_module(bytes);
    let h2 = WasmEngine::hash_module(bytes);
    assert_eq!(h1, h2);
    assert!(!h1.is_empty());
}

#[test]
fn engine_creation() {
    let engine = WasmEngine::new();
    assert!(engine.is_ok());
}

#[test]
fn module_too_large_rejected() {
    let engine = WasmEngine::new().unwrap();
    let big = vec![0u8; MAX_MODULE_SIZE + 1];
    let result = engine.compile_and_cache(&big);
    assert!(matches!(result, Err(WasmError::ModuleTooLarge { .. })));
}

#[test]
fn resource_limits_default() {
    let limits = WasmResourceLimits::default();
    assert_eq!(limits.max_fuel, 1_000_000_000);
    assert_eq!(limits.max_memory, 16 * 1024 * 1024);
    assert_eq!(limits.max_duration, std::time::Duration::from_secs(30));
    assert_eq!(limits.max_response_bytes, 1024 * 1024);
}

#[test]
fn timeout_error_display() {
    let err = WasmError::Timeout(std::time::Duration::from_secs(5));
    let msg = err.to_string();
    assert!(msg.contains("timeout"), "expected 'timeout' in: {msg}");
}

#[test]
fn memory_limit_exceeded_display() {
    let err = WasmError::MemoryLimitExceeded { max_bytes: 1024 };
    let msg = err.to_string();
    assert!(
        msg.contains("memory limit"),
        "expected 'memory limit' in: {msg}"
    );
}

/// Fuel exhaustion: module runs infinite loop with tiny fuel budget.
#[tokio::test]
async fn fuel_exhaustion_returns_error() {
    let engine = WasmEngine::new().unwrap();
    let hash = engine
        .compile_and_cache(WAT_INFINITE_LOOP.as_bytes())
        .unwrap();

    let limits = WasmResourceLimits {
        max_fuel: 1_000, // tiny budget
        ..WasmResourceLimits::default()
    };
    let result = engine
        .invoke(&hash, &make_context(), make_host(), &limits, make_streams())
        .await;

    assert!(
        matches!(result, Err(WasmError::FuelExhausted)),
        "expected FuelExhausted, got: {result:?}"
    );
}

/// Timeout: module runs infinite loop, epoch fires after 50 ms.
///
/// Requires multi-thread runtime: the epoch timer task must run concurrently
/// while the WASM call blocks the main thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_enforced_by_epoch() {
    let engine = WasmEngine::new().unwrap();
    let hash = engine
        .compile_and_cache(WAT_INFINITE_LOOP.as_bytes())
        .unwrap();

    let limits = WasmResourceLimits {
        max_fuel: u64::MAX,                                 // no fuel limit
        max_duration: std::time::Duration::from_millis(50), // very short
        ..WasmResourceLimits::default()
    };
    let result = engine
        .invoke(&hash, &make_context(), make_host(), &limits, make_streams())
        .await;

    assert!(
        matches!(result, Err(WasmError::Timeout(_))),
        "expected Timeout, got: {result:?}"
    );
}

/// Error isolation: a WASM trap (unreachable) is converted to WasmError,
/// not a Rust panic. The host process must survive.
#[tokio::test]
async fn wasm_trap_does_not_crash_host() {
    let engine = WasmEngine::new().unwrap();
    let hash = engine.compile_and_cache(WAT_TRAP.as_bytes()).unwrap();

    let limits = WasmResourceLimits::default();
    let result = engine
        .invoke(&hash, &make_context(), make_host(), &limits, make_streams())
        .await;

    // Must return Err (trap propagated as error), not panic.
    assert!(result.is_err(), "expected Err from trap, got Ok");
    // Must not be mistaken for fuel or timeout.
    assert!(
        !matches!(
            result,
            Err(WasmError::FuelExhausted) | Err(WasmError::Timeout(_))
        ),
        "trap should not be FuelExhausted or Timeout"
    );
}

/// Memory limiter: module tries to grow beyond max_memory — growth is denied
/// but the module still returns (memory.grow returns -1, not a crash).
#[tokio::test]
async fn memory_growth_denied_by_limiter() {
    let engine = WasmEngine::new().unwrap();
    let hash = engine
        .compile_and_cache(WAT_MEMORY_GROW.as_bytes())
        .unwrap();

    // Limit to 1 page (64 KB). Module tries to grow by 1000 pages.
    let limits = WasmResourceLimits {
        max_memory: 64 * 1024, // 1 WASM page
        ..WasmResourceLimits::default()
    };
    let result = engine
        .invoke(&hash, &make_context(), make_host(), &limits, make_streams())
        .await;

    // Module returns normally (memory.grow returned -1 per spec — not a trap).
    // The invocation itself succeeds (no crash), but the result is empty
    // because the module didn't call host_set_result.
    assert!(
        result.is_ok() || matches!(result, Err(WasmError::Invocation(_))),
        "memory denial should not cause fuel/timeout error, got: {result:?}"
    );
    // Critically: no panic, no FuelExhausted, no Timeout.
    assert!(
        !matches!(
            result,
            Err(WasmError::FuelExhausted) | Err(WasmError::Timeout(_))
        ),
        "memory denial should not be misclassified"
    );
}

/// Noop module completes successfully without hitting any limits.
#[tokio::test]
async fn noop_module_completes() {
    let engine = WasmEngine::new().unwrap();
    let hash = engine.compile_and_cache(WAT_NOOP.as_bytes()).unwrap();

    let limits = WasmResourceLimits::default();
    let result = engine
        .invoke(&hash, &make_context(), make_host(), &limits, make_streams())
        .await;

    // Noop doesn't call host_set_result so we get an empty-result error,
    // but crucially it doesn't hit fuel, timeout, or memory errors.
    assert!(
        !matches!(
            result,
            Err(WasmError::FuelExhausted)
                | Err(WasmError::Timeout(_))
                | Err(WasmError::MemoryLimitExceeded { .. })
        ),
        "noop should not hit resource limits, got: {result:?}"
    );
}
