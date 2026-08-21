//! Unit and integration tests for the WASM engine.

use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use super::*;
use crate::host_trait::{SimWasmHost, WasmHost};
use crate::stream::StreamRegistry;
use async_trait::async_trait;

const _: () = {
    assert!(WASM_INVOKE_THREAD_STACK_BYTES >= 16 * 1024 * 1024);
};

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

// Tries to grow memory by 1000 pages (64 MB) — exceeds the budget. Traps
// (`unreachable`) if the grow unexpectedly *succeeds* (result != -1), so a test
// asserting the invocation is Ok proves the limiter actually denied the growth —
// it is not vacuous.
const WAT_MEMORY_GROW: &str = r#"
    (module
      (memory (export "memory") 1)
      (func (export "run") (param i32 i32) (result i32)
        (memory.grow (i32.const 1000))
        i32.const -1
        i32.ne
        if
          unreachable
        end
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

// Makes one host HTTP function call, then returns normally. That call gives another
// invocation enough time to time out while this store is still active.
const WAT_HOST_HTTP_THEN_RETURN: &str = r#"
    (module
      (import "env" "host_http_call" (func $host_http_call
        (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32) (result i32)))
      (memory (export "memory") 1)
      (data (i32.const 2048) "GET")
      (data (i32.const 2056) "https://slow.test/")
      (func (export "run") (param i32 i32) (result i32)
        i32.const 2048
        i32.const 3
        i32.const 2056
        i32.const 18
        i32.const 0
        i32.const 0
        i32.const 0
        i32.const 0
        i32.const 4096
        i32.const 64
        call $host_http_call
        drop
        i32.const 0
      )
    )
"#;

const WAT_GUEST_SPAN_LIFECYCLE: &str = r#"
    (module
      (import "env" "host_start_span" (func $host_start_span
        (param i32 i32) (result i64)))
      (import "env" "host_add_span_event" (func $host_add_span_event
        (param i64 i32 i32) (result i32)))
      (import "env" "host_set_span_attributes" (func $host_set_span_attributes
        (param i64 i32 i32) (result i32)))
      (import "env" "host_end_span" (func $host_end_span
        (param i64 i32 i32) (result i32)))
      (memory (export "memory") 1)
      (data (i32.const 2048) "{\"name\":\"guest.root\",\"attributes\":{\"tool.name\":\"proof\",\"custom\":\"value\"}}")
      (data (i32.const 2176) "{\"name\":\"guest.event\",\"attributes\":{\"phase\":\"inside\"}}")
      (data (i32.const 2304) "{\"attributes\":{\"gen_ai.operation.name\":\"execute_tool\",\"tool.call_id\":\"call-1\"}}")
      (data (i32.const 2432) "{\"status\":\"ok\",\"attributes\":{\"result\":\"done\"}}")
      (func (export "run") (param i32 i32) (result i32)
        (local $span i64)
        i32.const 2048
        i32.const 73
        call $host_start_span
        local.set $span

        local.get $span
        i64.const 0
        i64.le_s
        if
          unreachable
        end

        local.get $span
        i32.const 2176
        i32.const 54
        call $host_add_span_event
        if
          unreachable
        end

        local.get $span
        i32.const 2304
        i32.const 79
        call $host_set_span_attributes
        if
          unreachable
        end

        local.get $span
        i32.const 2432
        i32.const 46
        call $host_end_span
        if
          unreachable
        end

        i32.const 0
      )
    )
"#;

struct SlowHttpHost {
    delay: Duration,
}

#[async_trait]
impl WasmHost for SlowHttpHost {
    async fn http_call(
        &self,
        _method: &str,
        _url: &str,
        _headers: &[(String, String)],
        _body: &str,
    ) -> Result<(u16, String), String> {
        tokio::time::sleep(self.delay).await;
        Ok((200, r#"{"ok":true}"#.to_string()))
    }

    async fn http_call_binary(
        &self,
        _method: &str,
        _url: &str,
        _headers: &[(String, String)],
        _body: &[u8],
    ) -> Result<(u16, Vec<u8>), String> {
        Ok((200, Vec::new()))
    }

    fn get_secret(&self, key: &str) -> Result<String, String> {
        Err(format!("test secret not found: {key}"))
    }

    fn log(&self, _level: &str, _message: &str) {}
}

pub(super) fn make_context() -> WasmInvocationContext {
    WasmInvocationContext {
        tenant: "test".into(),
        entity_type: "Order".into(),
        entity_id: "1".into(),
        trigger_action: "Submit".into(),
        wasm_module: Some("test_module".into()),
        trigger_params: serde_json::Value::Null,
        entity_state: serde_json::Value::Null,
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

pub(super) fn make_host() -> Arc<dyn WasmHost> {
    Arc::new(SimWasmHost::new())
}

pub(super) fn make_streams() -> Arc<RwLock<StreamRegistry>> {
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
    assert_eq!(limits.max_memory, 64 * 1024 * 1024);
    // ADR-0045: raised from 30s to cover HTTP-fronted integrations under load.
    assert_eq!(limits.max_duration, std::time::Duration::from_secs(120));
    assert_eq!(limits.max_response_bytes, 1024 * 1024);
}

#[tokio::test]
async fn wasm_guest_can_drive_structured_observability_span_lifecycle() {
    let engine = WasmEngine::new().unwrap();
    let hash = engine
        .compile_and_cache(WAT_GUEST_SPAN_LIFECYCLE.as_bytes())
        .unwrap();

    let result = engine
        .invoke(
            &hash,
            &make_context(),
            make_host(),
            &WasmResourceLimits::default(),
            make_streams(),
        )
        .await;

    assert!(
        result.is_ok(),
        "guest span host functions should link and complete: {result:?}"
    );
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

/// Host calls must be bounded by the configured invocation duration, not by a
/// hidden platform-wide deadline or the eventual completion of the host future.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_call_respects_invocation_duration_budget() {
    let engine = WasmEngine::new().unwrap();
    let hash = engine
        .compile_and_cache(WAT_HOST_HTTP_THEN_RETURN.as_bytes())
        .unwrap();

    let limits = WasmResourceLimits {
        max_fuel: u64::MAX,
        max_duration: Duration::from_millis(50),
        ..WasmResourceLimits::default()
    };
    let host: Arc<dyn WasmHost> = Arc::new(SlowHttpHost {
        delay: Duration::from_millis(350),
    });

    let start = std::time::Instant::now();
    let result = engine
        .invoke(&hash, &make_context(), host, &limits, make_streams())
        .await;
    let elapsed = start.elapsed();

    assert!(
        result.is_ok() || matches!(result, Err(WasmError::Timeout(_))),
        "host-call budget expiry should return through the WASM invocation path, got: {result:?}"
    );
    assert!(
        elapsed < Duration::from_millis(250),
        "host call should return on the configured invocation budget, got {elapsed:?}"
    );
}

/// Timeout isolation: one invocation timing out must not interrupt a different
/// active store that has not exhausted its own deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timed_out_invocation_does_not_interrupt_unrelated_active_invocation() {
    let engine = Arc::new(WasmEngine::new().unwrap());
    let slow_hash = engine
        .compile_and_cache(WAT_HOST_HTTP_THEN_RETURN.as_bytes())
        .unwrap();
    let timeout_hash = engine
        .compile_and_cache(WAT_INFINITE_LOOP.as_bytes())
        .unwrap();

    let slow_engine = Arc::clone(&engine);
    let slow_task = tokio::spawn(async move {
        let limits = WasmResourceLimits {
            max_fuel: u64::MAX,
            max_duration: Duration::from_millis(800),
            ..WasmResourceLimits::default()
        };
        let host: Arc<dyn WasmHost> = Arc::new(SlowHttpHost {
            delay: Duration::from_millis(200),
        });
        slow_engine
            .invoke(&slow_hash, &make_context(), host, &limits, make_streams())
            .await
    });

    tokio::time::sleep(Duration::from_millis(20)).await;
    let timeout_limits = WasmResourceLimits {
        max_fuel: u64::MAX,
        max_duration: Duration::from_millis(50),
        ..WasmResourceLimits::default()
    };
    let timeout_result = engine
        .invoke(
            &timeout_hash,
            &make_context(),
            make_host(),
            &timeout_limits,
            make_streams(),
        )
        .await;
    assert!(
        matches!(timeout_result, Err(WasmError::Timeout(_))),
        "expected the infinite loop to time out, got: {timeout_result:?}"
    );

    let slow_result = slow_task.await.expect("slow invocation task should join");
    assert!(
        !matches!(slow_result, Err(WasmError::Timeout(_))),
        "timeout leaked into unrelated active invocation: {slow_result:?}"
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

    // The module traps (`unreachable`) if `memory.grow` returned anything other
    // than -1, so a successful invocation proves the limiter actually denied the
    // 1000-page growth (grow returned -1 per spec, not a crash). If the limiter
    // had NOT fired, the grow would succeed and the module would trap — surfacing
    // as an error, failing this assertion. Not vacuous.
    assert!(
        result.is_ok(),
        "memory.grow must return -1 (limiter denied growth), got: {result:?}"
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

// ARN-169: WASI compatibility + sandbox-limit regressions across the
// wasmtime 29 -> 36 bump. A module declaring more initial memory than the
// budget must be rejected at instantiation, and a WASIp1 module must still
// invoke end to end after the `wasmtime_wasi::p2` module reshuffle.

const WAT_INITIAL_MEMORY_TWO_PAGES: &str = r#"
    (module
      (memory (export "memory") 2)
      (func (export "run") (param i32 i32) (result i32)
        i32.const 0
      )
    )
"#;

const WAT_WASI_STDERR: &str = r#"
    (module
      (import "wasi_snapshot_preview1" "fd_write"
        (func $fd_write (param i32 i32 i32 i32) (result i32)))
      (memory (export "memory") 1)
      (data (i32.const 32) "wasi stderr\n")
      (func (export "run") (param i32 i32) (result i32)
        i32.const 0
        i32.const 32
        i32.store
        i32.const 4
        i32.const 12
        i32.store
        i32.const 2
        i32.const 0
        i32.const 1
        i32.const 16
        call $fd_write
        if
          unreachable
        end
        i32.const 0
      )
    )
"#;

#[tokio::test]
async fn initial_memory_over_budget_is_rejected() {
    let engine = WasmEngine::new().unwrap();
    let hash = engine
        .compile_and_cache(WAT_INITIAL_MEMORY_TWO_PAGES.as_bytes())
        .unwrap();
    let limits = WasmResourceLimits {
        max_memory: 64 * 1024, // 1 WASM page; the module declares 2
        ..WasmResourceLimits::default()
    };

    let result = engine
        .invoke(&hash, &make_context(), make_host(), &limits, make_streams())
        .await;

    assert!(
        matches!(result, Err(WasmError::Instantiation(_))),
        "oversized initial memory must fail at instantiation: {result:?}"
    );
}

#[tokio::test]
async fn wasi_preview1_module_invokes_end_to_end() {
    let engine = WasmEngine::new().unwrap();
    let hash = engine
        .compile_and_cache(WAT_WASI_STDERR.as_bytes())
        .unwrap();

    let result = engine
        .invoke(
            &hash,
            &make_context(),
            make_host(),
            &WasmResourceLimits::default(),
            make_streams(),
        )
        .await;

    assert!(result.is_ok(), "WASIp1 invocation failed: {result:?}");
}

// ARN-169: the wasmtime 36 feature surface is pinned to match the 29 sandbox —
// memory64 and multiple memories are rejected at compile, and table growth is
// bounded like linear memory.

const WAT_MEMORY64: &str = r#"
    (module
      (memory i64 1)
      (func (export "run") (param i32 i32) (result i32)
        i32.const 0
      )
    )
"#;

const WAT_MULTI_MEMORY: &str = r#"
    (module
      (memory 1)
      (memory 1)
      (func (export "run") (param i32 i32) (result i32)
        i32.const 0
      )
    )
"#;

const WAT_TABLE_GROW: &str = r#"
    (module
      (memory (export "memory") 1)
      (table 1 funcref)
      (func (export "run") (param i32 i32) (result i32)
        (table.grow (ref.null func) (i32.const 2000000))
        i32.const -1
        i32.ne
        if
          unreachable
        end
        i32.const 0
      )
    )
"#;

#[test]
fn memory64_module_is_rejected() {
    let engine = WasmEngine::new().unwrap();
    assert!(
        engine.compile_and_cache(WAT_MEMORY64.as_bytes()).is_err(),
        "memory64 must be rejected at compile — it widens the surface and was a \
         RUSTSEC-2026-0096 prerequisite"
    );
}

#[test]
fn multi_memory_module_is_rejected() {
    let engine = WasmEngine::new().unwrap();
    assert!(
        engine
            .compile_and_cache(WAT_MULTI_MEMORY.as_bytes())
            .is_err(),
        "multiple memories must be rejected: the per-memory limiter does not sum them"
    );
}

#[tokio::test]
async fn table_growth_denied_past_cap() {
    let engine = WasmEngine::new().unwrap();
    let hash = engine.compile_and_cache(WAT_TABLE_GROW.as_bytes()).unwrap();

    let result = engine
        .invoke(
            &hash,
            &make_context(),
            make_host(),
            &WasmResourceLimits::default(),
            make_streams(),
        )
        .await;

    // The module traps if table.grow returned anything but -1, so a successful
    // invocation proves the limiter denied the 2,000,000-element growth.
    assert!(
        result.is_ok(),
        "table.grow past the cap must return -1 (limiter denied), got: {result:?}"
    );
}

// A shared memory (threads proposal) must be rejected — the pin is load-bearing
// because wasmtime does not invoke the memory limiter for shared memories.
const WAT_SHARED_MEMORY: &str = r#"
    (module
      (memory 1 1 shared)
      (func (export "run") (param i32 i32) (result i32)
        i32.const 0
      )
    )
"#;

const WAT_MANY_TABLES: &str = r#"
    (module
      (memory (export "memory") 1)
      (table $0 1 funcref)
      (table $1 1 funcref)
      (table $2 1 funcref)
      (table $3 1 funcref)
      (table $4 1 funcref)
      (table $5 1 funcref)
      (table $6 1 funcref)
      (table $7 1 funcref)
      (table $8 1 funcref)
      (func (export "run") (param i32 i32) (result i32)
        i32.const 0
      )
    )
"#;
#[test]
fn shared_memory_module_is_rejected() {
    let engine = WasmEngine::new().unwrap();
    assert!(
        engine
            .compile_and_cache(WAT_SHARED_MEMORY.as_bytes())
            .is_err(),
        "shared memory must be rejected: the limiter is not consulted for it"
    );
}

#[tokio::test]
async fn too_many_tables_is_rejected() {
    // MAX_TABLES tables is allowed; more must be denied at instantiation so the
    // per-table element cap is a real store-wide budget, not per-table only.
    let engine = WasmEngine::new().unwrap();
    let hash = engine
        .compile_and_cache(WAT_MANY_TABLES.as_bytes())
        .unwrap();
    let result = engine
        .invoke(
            &hash,
            &make_context(),
            make_host(),
            &WasmResourceLimits::default(),
            make_streams(),
        )
        .await;
    assert!(
        matches!(result, Err(WasmError::Instantiation(_))),
        "a module declaring more than MAX_TABLES tables must fail instantiation: {result:?}"
    );
}
