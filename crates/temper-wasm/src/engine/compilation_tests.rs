//! Compiled code can be reused, but registration and guest state must not be.

use super::tests::{make_context, make_host, make_streams};
use super::*;

const STATEFUL_MODULE: &[u8] = br#"
    (module
      (memory (export "memory") 1)
      (global $counter (mut i32) (i32.const 0))
      (func (export "run") (param i32 i32) (result i32)
        global.get $counter
        if unreachable end
        i32.const 0
        i32.load
        if unreachable end
        i32.const 1
        global.set $counter
        i32.const 0
        i32.const 1
        i32.store
        i32.const 0))
"#;

#[tokio::test]
async fn shared_compilation_preserves_registration_and_guest_isolation() {
    let first = WasmEngine::new().unwrap();
    let hash = first.compile_and_cache(STATEFUL_MODULE).unwrap();
    let second = WasmEngine::new().unwrap();

    // A restart must restore/register the module before it can invoke it.
    assert_eq!(second.cache_size(), 0);
    assert!(!second.is_cached(&hash));
    assert!(matches!(
        second
            .invoke(
                &hash,
                &make_context(),
                make_host(),
                &WasmResourceLimits::default(),
                make_streams(),
            )
            .await,
        Err(WasmError::ModuleNotFound(_))
    ));

    assert_eq!(second.compile_and_cache(STATEFUL_MODULE).unwrap(), hash);
    let compiled = first.cache.read().unwrap().get(&hash).unwrap().clone();
    let restored = second.cache.read().unwrap().get(&hash).unwrap().clone();
    assert_eq!(
        Arc::ptr_eq(&compiled, &restored),
        configured_profiling_strategy().is_none(),
        "tests should share compiled code unless explicit profiling requests private engines"
    );

    // Shared code must not share linear memory, mutable globals, or Stores.
    // The guest traps if it observes writes from any previous invocation.
    for engine in [&first, &second, &first, &second] {
        engine
            .invoke(
                &hash,
                &make_context(),
                make_host(),
                &WasmResourceLimits::default(),
                make_streams(),
            )
            .await
            .unwrap();
    }

    drop(first);
    assert!(second.is_cached(&hash));
}

#[test]
fn concurrent_boots_compile_once() {
    let compiler = Arc::new(compilation::SharedCompiler::new().unwrap());
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let engine = compiler.new_engine();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let hash = engine.compile_and_cache(STATEFUL_MODULE).unwrap();
                engine.cache.read().unwrap().get(&hash).unwrap().clone()
            })
        })
        .collect();
    let compiled: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(compiled.iter().all(|m| Arc::ptr_eq(m, &compiled[0])));
    assert_eq!(compiler.modules.lock().unwrap().len(), 1);
}

#[test]
fn private_production_engines_still_compile_independently() {
    let first = WasmEngine::new_private().unwrap();
    let second = WasmEngine::new_private().unwrap();
    let hash = first.compile_and_cache(STATEFUL_MODULE).unwrap();
    second.compile_and_cache(STATEFUL_MODULE).unwrap();
    let a = first.cache.read().unwrap().get(&hash).unwrap().clone();
    let b = second.cache.read().unwrap().get(&hash).unwrap().clone();
    assert!(!Arc::ptr_eq(&a, &b));
    assert!(first.shared_compiler.is_none());
    assert!(second.shared_compiler.is_none());
}

#[tokio::test]
async fn shared_cache_eviction_does_not_evict_registered_modules() {
    let compiler = Arc::new(compilation::SharedCompiler::new().unwrap());
    let engine = compiler.new_engine();
    let hash = engine.compile_and_cache(STATEFUL_MODULE).unwrap();
    let compiled = engine.cache.read().unwrap().get(&hash).unwrap().clone();
    // Force eviction of this particular entry independently of its hash order.
    {
        let mut modules = compiler.modules.lock().unwrap();
        modules.clear();
        modules.insert(String::new(), Arc::clone(&compiled));
        for index in 1..compilation::MAX_SHARED_MODULES {
            modules.insert(format!("padding-{index}"), Arc::clone(&compiled));
        }
    }
    let other = compiler.new_engine();
    other
        .compile_and_cache(b"(module (memory (export \"memory\") 1))")
        .unwrap();
    assert_eq!(
        compiler.modules.lock().unwrap().len(),
        compilation::MAX_SHARED_MODULES
    );
    assert!(!compiler.modules.lock().unwrap().contains_key(""));
    engine
        .invoke(
            &hash,
            &make_context(),
            make_host(),
            &WasmResourceLimits::default(),
            make_streams(),
        )
        .await
        .unwrap();
    assert!(engine.evict(&hash));
    assert!(!engine.is_cached(&hash));
    // A warm compiler must not make an explicitly evicted registration visible.
    assert!(matches!(
        engine
            .invoke(
                &hash,
                &make_context(),
                make_host(),
                &WasmResourceLimits::default(),
                make_streams()
            )
            .await,
        Err(WasmError::ModuleNotFound(_))
    ));
}

#[tokio::test]
async fn shared_code_uses_each_invocations_host_secrets() {
    let wasm = br#"(module
      (import "env" "host_get_secret" (func $secret (param i32 i32 i32 i32) (result i32)))
      (import "env" "host_set_result" (func $result (param i32 i32)))
      (memory (export "memory") 1)
      (data (i32.const 0) "payload")
      (func (export "run") (param i32 i32) (result i32)
        i32.const 1024
        i32.const 0
        i32.const 7
        i32.const 1024
        i32.const 1024
        call $secret
        call $result
        i32.const 0))"#;
    for tenant in ["tenant-a", "tenant-b", "tenant-a"] {
        let engine = WasmEngine::new().unwrap();
        let hash = engine.compile_and_cache(wasm).unwrap();
        let payload = serde_json::json!({
            "success": true,
            "action": tenant,
            "params": {}
        })
        .to_string();
        let host = Arc::new(crate::host_trait::SimWasmHost::new().with_secret("payload", &payload));
        let mut context = make_context();
        context.tenant = tenant.to_owned();
        let result = engine
            .invoke(
                &hash,
                &context,
                host,
                &WasmResourceLimits::default(),
                make_streams(),
            )
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.callback_action, tenant);
    }
}
