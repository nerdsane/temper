use super::*;

#[tokio::test]
async fn self_callback_stops_at_the_runtime_budget_before_the_spec_guard() {
    let spec = r#"
[automaton]
name = "Job"
states = ["Idle"]
initial = "Idle"
strict_action_params = true
[[state]]
name = "ticks"
type = "counter"
initial = "0"
[[action]]
name = "Tick"
from = ["Idle"]
params = []
guard = "ticks < 12"
effect = [{type="increment",var="ticks"},{type="trigger",name="local_job"}]
[[integration]]
name = "local_job"
trigger = "local_job"
type = "wasm"
module = "local_job"
on_success = "Tick"
"#;
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).unwrap(),
        CSDL.into(),
        &[("Job", spec)],
    );
    let state = ServerState::from_registry(ActorSystem::new("callback-budget"), registry);
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let payload = json!({"action":"Tick","params":{},"success":true}).to_string();
    let data = payload
        .bytes()
        .map(|byte| format!("\\{byte:02x}"))
        .collect::<String>();
    let wat = format!(
        r#"(module
        (import "env" "host_set_result" (func $result (param i32 i32)))
        (memory (export "memory") 1)
        (data (i32.const 0) "{data}")
        (func (export "run") (param i32 i32) (result i32)
          i32.const 0 i32.const {} call $result i32.const 0))"#,
        payload.len()
    );
    let hash = state.wasm_engine.compile_and_cache(wat.as_bytes()).unwrap();
    let tenant = TenantId::default();
    state
        .wasm_module_registry
        .write()
        .unwrap()
        .register(&tenant, "local_job", &hash);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        state.dispatch(temper_server::state::DispatchCommand {
            tenant: &tenant,
            entity_type: "Job",
            entity_id: "job",
            action: "Tick",
            params: json!({}),
            agent_ctx: &Default::default(),
            await_integration: true,
            await_reactions: true,
        }),
    )
    .await
    .expect("self callback did not terminate")
    .unwrap();
    assert!(!result.success);
    let actual = state
        .get_tenant_entity_state(&tenant, "Job", "job")
        .await
        .unwrap()
        .state;
    assert!(
        actual.counters["ticks"] <= 9,
        "callback chain bypassed runtime budget: {}",
        actual.counters["ticks"]
    );
    assert!(
        result
            .error
            .as_deref()
            .is_some_and(|error| error.contains("callback depth")),
        "{:?}",
        result.error
    );
}

#[tokio::test]
async fn failed_compensation_chain_keeps_the_callback_budget() {
    let spec = r#"
[automaton]
name = "Job"
states = ["Idle"]
initial = "Idle"
strict_action_params = true
[[state]]
name = "ticks"
type = "counter"
initial = "0"
[[action]]
name = "Fail"
from = ["Idle"]
params = ["error"]
guard = "ticks < 40"
effect = [{type="increment",var="ticks"},{type="trigger",name="failing_job"}]
[[integration]]
name = "failing_job"
trigger = "failing_job"
type = "wasm"
module = "failing_job"
"#;
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).unwrap(),
        CSDL.into(),
        &[("Job", spec)],
    );
    let state = ServerState::from_registry(ActorSystem::new("compensation-budget"), registry);
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let hash = state.wasm_engine.compile_and_cache(br#"(module (memory (export "memory") 1) (func (export "run") (param i32 i32) (result i32) unreachable))"#).unwrap();
    let tenant = TenantId::default();
    state
        .wasm_module_registry
        .write()
        .unwrap()
        .register(&tenant, "failing_job", &hash);
    let result = state
        .dispatch(temper_server::state::DispatchCommand {
            tenant: &tenant,
            entity_type: "Job",
            entity_id: "job",
            action: "Fail",
            params: json!({"error":"local fixture"}),
            agent_ctx: &temper_server::request_context::AgentContext {
                callback_hops: temper_server::request_context::MAX_CALLBACK_HOPS - 12,
                ..Default::default()
            },
            await_integration: false,
            await_reactions: true,
        })
        .await
        .unwrap();
    assert!(result.success);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let ticks = state
                .get_tenant_entity_state(&tenant, "Job", "job")
                .await
                .unwrap()
                .state
                .counters["ticks"];
            assert!(
                ticks <= 13,
                "compensation replenished the callback budget: {ticks}"
            );
            let refused = state
                .entity_observe_log
                .lock()
                .unwrap()
                .values()
                .flatten()
                .any(|event| {
                    event.event_name == "integration_failure_dropped"
                        && event.data["reason"]
                            .as_str()
                            .is_some_and(|reason| reason.contains("callback hop"))
                });
            if refused {
                assert_eq!(
                    ticks, 13,
                    "budget must allow all twelve remaining compensations"
                );
                let callback_refusal = state
                    .entity_observe_log
                    .lock()
                    .unwrap()
                    .values()
                    .flatten()
                    .any(|event| {
                        event.event_name == "integration_callback_rejected"
                            && event.data["action"] == "Fail"
                            && event.data["error"] == "integration callback hop budget exhausted"
                    });
                assert!(
                    callback_refusal,
                    "compensation exhaustion omitted the generated callback refusal event"
                );
                let unchanged = state
                    .get_tenant_entity_state(&tenant, "Job", "job")
                    .await
                    .unwrap()
                    .state;
                assert_eq!(unchanged.counters["ticks"], ticks);
                assert_eq!(unchanged.status, "Idle");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("compensation budget exhaustion was not surfaced");
}

#[tokio::test]
async fn detached_callbacks_complete_a_bounded_long_workflow() {
    let spec = r#"
[automaton]
name = "Job"
states = ["Idle"]
initial = "Idle"
strict_action_params = true
[[state]]
name = "ticks"
type = "counter"
initial = "0"
[[action]]
name = "Tick"
from = ["Idle"]
params = []
guard = "ticks < 241"
effect = [{type="increment",var="ticks"},{type="trigger",name="local_job"}]
[[integration]]
name = "local_job"
trigger = "local_job"
type = "wasm"
module = "local_job"
on_success = "Tick"
"#;
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).unwrap(),
        CSDL.into(),
        &[("Job", spec)],
    );
    let state = ServerState::from_registry(ActorSystem::new("detached-callback-budget"), registry);
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let payload = json!({"action":"Tick","params":{},"success":true}).to_string();
    let data = payload
        .bytes()
        .map(|byte| format!("\\{byte:02x}"))
        .collect::<String>();
    let wat = format!(
        r#"(module
        (import "env" "host_set_result" (func $result (param i32 i32)))
        (memory (export "memory") 1)
        (data (i32.const 0) "{data}")
        (func (export "run") (param i32 i32) (result i32)
          i32.const 0 i32.const {} call $result i32.const 0))"#,
        payload.len()
    );
    let hash = state.wasm_engine.compile_and_cache(wat.as_bytes()).unwrap();
    let tenant = TenantId::default();
    state
        .wasm_module_registry
        .write()
        .unwrap()
        .register(&tenant, "local_job", &hash);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        state.dispatch(temper_server::state::DispatchCommand {
            tenant: &tenant,
            entity_type: "Job",
            entity_id: "job",
            action: "Tick",
            params: json!({}),
            agent_ctx: &Default::default(),
            await_integration: false,
            await_reactions: true,
        }),
    )
    .await
    .expect("self callback did not terminate")
    .unwrap();
    assert!(result.success);
    // 40 rounds of six continuations plus the initial action; real detached
    // dispatch must not confuse this sequence with an inline recursive stack.
    // Debug WASM execution measured 95 actions in ten seconds. Preserve all
    // 240 callbacks with headroom for the unoptimized engine.
    let started = std::time::Instant::now();
    let completed = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            let actual = state
                .get_tenant_entity_state(&tenant, "Job", "job")
                .await
                .unwrap()
                .state;
            if actual.counters["ticks"] == 241 {
                break;
            }
            let refusal = state
                .entity_observe_log
                .lock()
                .unwrap()
                .values()
                .flatten()
                .find(|event| {
                    event.event_name == "integration_callback_rejected"
                        || event.event_name == "integration_failure_dropped"
                })
                .map(|event| event.data.clone());
            assert!(
                refusal.is_none(),
                "detached callback refused: ticks={}, refusal={refusal:?}",
                actual.counters["ticks"]
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    if completed.is_err() {
        let ticks = state
            .get_tenant_entity_state(&tenant, "Job", "job")
            .await
            .unwrap()
            .state
            .counters["ticks"];
        let refusals = state
            .entity_observe_log
            .lock()
            .unwrap()
            .values()
            .flatten()
            .filter(|event| {
                event.event_name == "integration_callback_rejected"
                    || event.event_name == "integration_failure_dropped"
            })
            .map(|event| event.data.clone())
            .collect::<Vec<_>>();
        panic!(
            "detached workflow stopped before its declared bound: ticks={ticks}, refusals={refusals:?}"
        );
    }
    eprintln!(
        "completed 240 callbacks: elapsed_ms={}",
        started.elapsed().as_millis()
    );
}

#[tokio::test]
async fn detached_callbacks_surface_cumulative_budget_exhaustion() {
    let spec = r#"
[automaton]
name = "Job"
states = ["Idle"]
initial = "Idle"
strict_action_params = true
[[state]]
name = "ticks"
type = "counter"
initial = "0"
[[action]]
name = "Tick"
from = ["Idle"]
params = []
guard = "ticks < 40"
effect = [{type="increment",var="ticks"},{type="trigger",name="local_job"}]
[[integration]]
name = "local_job"
trigger = "local_job"
type = "wasm"
module = "local_job"
on_success = "Tick"
"#;
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).unwrap(),
        CSDL.into(),
        &[("Job", spec)],
    );
    let state = ServerState::from_registry(ActorSystem::new("detached-callback-budget"), registry);
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let payload = json!({"action":"Tick","params":{},"success":true}).to_string();
    let data = payload
        .bytes()
        .map(|byte| format!("\\{byte:02x}"))
        .collect::<String>();
    let wat = format!(
        r#"(module
        (import "env" "host_set_result" (func $result (param i32 i32)))
        (memory (export "memory") 1)
        (data (i32.const 0) "{data}")
        (func (export "run") (param i32 i32) (result i32)
          i32.const 0 i32.const {} call $result i32.const 0))"#,
        payload.len()
    );
    let hash = state.wasm_engine.compile_and_cache(wat.as_bytes()).unwrap();
    let tenant = TenantId::default();
    state
        .wasm_module_registry
        .write()
        .unwrap()
        .register(&tenant, "local_job", &hash);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        state.dispatch(temper_server::state::DispatchCommand {
            tenant: &tenant,
            entity_type: "Job",
            entity_id: "job",
            action: "Tick",
            params: json!({}),
            agent_ctx: &temper_server::request_context::AgentContext {
                callback_hops: temper_server::request_context::MAX_CALLBACK_HOPS - 12,
                ..Default::default()
            },
            await_integration: false,
            await_reactions: true,
        }),
    )
    .await
    .expect("self callback did not terminate")
    .unwrap();
    assert!(result.success);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let ticks = state
                .get_tenant_entity_state(&tenant, "Job", "job")
                .await
                .unwrap()
                .state
                .counters["ticks"];
            assert!(
                ticks <= 13,
                "detached task replenished the callback budget: {ticks}"
            );
            let refused = state
                .entity_observe_log
                .lock()
                .unwrap()
                .values()
                .flatten()
                .any(|event| {
                    event.event_name == "integration_callback_rejected"
                        && event.data["error"]
                            .as_str()
                            .is_some_and(|reason| reason.contains("callback hop"))
                });
            if refused {
                assert_eq!(
                    ticks, 13,
                    "budget must allow all twelve remaining callbacks"
                );
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("detached callback budget exhaustion was not surfaced");
}
