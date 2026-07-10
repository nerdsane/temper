//! Sandbox-control and WASI compatibility regressions.

use super::*;

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
async fn memory_growth_denied_by_limiter() {
    let engine = WasmEngine::new().unwrap();
    let hash = engine
        .compile_and_cache(WAT_MEMORY_GROW.as_bytes())
        .unwrap();
    let limits = WasmResourceLimits {
        max_memory: 64 * 1024,
        ..WasmResourceLimits::default()
    };

    let result = engine
        .invoke(&hash, &make_context(), make_host(), &limits, make_streams())
        .await;

    assert!(
        result.is_ok(),
        "memory.grow must return -1 when the limiter denies it: {result:?}"
    );
}

#[tokio::test]
async fn initial_memory_over_budget_is_rejected() {
    let engine = WasmEngine::new().unwrap();
    let hash = engine
        .compile_and_cache(WAT_INITIAL_MEMORY_TWO_PAGES.as_bytes())
        .unwrap();
    let limits = WasmResourceLimits {
        max_memory: 64 * 1024,
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
