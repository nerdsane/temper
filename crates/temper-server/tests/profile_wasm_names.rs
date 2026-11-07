//! A CPU profile names the wasm guest functions that were running.
//!
//! Guest code is compiled by wasmtime into memory that no symbol table covers,
//! so without the engine's code map every sample taken inside it loses its
//! frame. This runs a guest that spins in a named function while the profiler
//! samples the process, then reads the pprof the operator endpoint returns.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::Query;
use pprof::protos::{Message, Profile};
use temper_server::profiling::{CpuProfileQuery, cpu_profile_handler};
use temper_wasm::{
    SimWasmHost, StreamRegistry, WasmEngine, WasmInvocationContext, WasmResourceLimits,
};

/// `run` calls `spin`, which loops; both are named in the module's name section.
const BUSY_GUEST: &str = r#"(module $busy_guest
  (memory (export "memory") 1)
  (func $spin (param $n i32) (result i32)
    (local $acc i32)
    (loop $again
      local.get $acc
      local.get $n
      i32.add
      local.set $acc
      local.get $n
      i32.const 1
      i32.sub
      local.tee $n
      br_if $again)
    local.get $acc)
  (func $run (export "run") (param i32 i32) (result i32)
    i32.const 20000000
    call $spin
    drop
    i32.const 0))"#;

fn context() -> WasmInvocationContext {
    WasmInvocationContext {
        tenant: "profiling".to_string(),
        entity_type: "Busy".to_string(),
        entity_id: "b1".to_string(),
        trigger_action: "Spin".to_string(),
        wasm_module: Some("busy_guest".to_string()),
        trigger_params: serde_json::json!({}),
        entity_state: serde_json::json!({}),
        agent_id: None,
        session_id: None,
        integration_config: std::collections::BTreeMap::new(),
        trace_id: String::new(),
        workflow_root_entity_type: None,
        workflow_root_entity_id: None,
        workflow_run_id: None,
        http_request: None,
        trigger_principal: None,
    }
}

/// Every `(function name, file name)` a profile's samples reach, with counts.
fn sampled_functions(profile: &Profile) -> Vec<(String, String, i64)> {
    let text = |at: i64| profile.string_table[at as usize].clone();
    let mut out = Vec::new();
    for sample in &profile.sample {
        for location_id in &sample.location_id {
            let location = profile
                .location
                .iter()
                .find(|l| l.id == *location_id)
                .expect("a sample's location is in the profile");
            for line in &location.line {
                let function = profile
                    .function
                    .iter()
                    .find(|f| f.id == line.function_id)
                    .expect("a line's function is in the profile");
                out.push((
                    text(function.name),
                    text(function.filename),
                    sample.value[0],
                ));
            }
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cpu_profile_names_the_running_wasm_function() {
    // SAFETY: this test binary holds this one test, so nothing reads the
    // environment concurrently.
    unsafe { std::env::set_var("TEMPER_PROFILING_ENABLED", "true") };

    let engine = Arc::new(WasmEngine::new().expect("engine"));
    let hash = engine
        .compile_and_cache(BUSY_GUEST.as_bytes())
        .expect("the guest compiles");
    let stop = Arc::new(AtomicBool::new(false));
    let spinner = {
        let (engine, stop) = (Arc::clone(&engine), Arc::clone(&stop));
        tokio::spawn(async move {
            let limits = WasmResourceLimits {
                max_fuel: u64::MAX / 2,
                ..WasmResourceLimits::default()
            };
            let mut runs = 0u32;
            while !stop.load(Ordering::Relaxed) {
                // The guest returns no result; only the time it spins matters.
                let _ = engine
                    .invoke(
                        &hash,
                        &context(),
                        Arc::new(SimWasmHost::new()),
                        &limits,
                        Arc::new(std::sync::RwLock::new(StreamRegistry::default())),
                    )
                    .await;
                runs += 1;
            }
            runs
        })
    };

    let response = cpu_profile_handler(Query(CpuProfileQuery {
        seconds: 2,
        frequency: 250,
    }))
    .await;
    stop.store(true, Ordering::Relaxed);
    let runs = spinner.await.expect("the spinner ends");
    assert!(runs > 0, "the guest ran during the capture");

    assert_eq!(response.status(), 200);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("the profile body reads");
    let profile = Profile::decode(body.as_ref()).expect("the body is pprof");
    let functions = sampled_functions(&profile);
    let in_guest: Vec<_> = functions
        .iter()
        .filter(|(_, file, _)| file == "wasm/busy_guest")
        .collect();
    let spin_samples: i64 = in_guest
        .iter()
        .filter(|(name, _, _)| name == "spin")
        .map(|(_, _, count)| count)
        .sum();
    let total: i64 = profile.sample.iter().map(|s| s.value[0]).sum();
    assert!(
        spin_samples > 0,
        "no sample named the guest's spin function; {} samples in all, guest frames {in_guest:?}",
        total
    );
}
