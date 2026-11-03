//! A route-table rebuild never drops a route because its row was slow to answer.
//!
//! On 20 Oct 2026 registering one HttpEndpoint rebuilt its tenant's table while four other rows' actors were loading
//! again after a long idle; each ask timed out at 500 ms, the rebuild skipped them, and the table it swapped in had
//! only the rows that answered, so those routes answered 404 until the next rebuild. A rebuild now waits longer for
//! every row at once, and a row it still cannot read keeps the route it had.

use std::time::Duration;

use serde_json::json;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::{install_deterministic_context, sim_now};
use temper_runtime::tenant::TenantId;
use temper_server::http_endpoint::{rebuild_tenant_table, rebuild_tenant_table_after};
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const HTTP_ENDPOINT_IOA: &str =
    include_str!("../../temper-platform/src/specs/HttpEndpoint.ioa.toml");

fn build(seed: u64) -> (ServerState, SimEventStore) {
    let store = SimEventStore::no_faults(seed);
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("HttpEndpoint", HTTP_ENDPOINT_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("http-endpoint-rebuild"), registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    (state, store)
}

fn fields(path: &str) -> serde_json::Value {
    json!({ "PathPrefix": path, "Methods": "POST", "IntegrationModule": "passes", "RequiresAuth": false, "TimeoutSecs": 10 })
}

async fn create(state: &ServerState, tenant: &TenantId, id: &str, path: &str) {
    let made = state
        .get_or_create_tenant_entity(tenant, "HttpEndpoint", id, fields(path))
        .await
        .expect("the route row is created");
    assert_eq!(made.state.status, "Active");
}

async fn paths(state: &ServerState, tenant: &TenantId) -> Vec<String> {
    let mut paths: Vec<String> = state
        .http_endpoint_tables
        .table_for(tenant)
        .await
        .rows()
        .await
        .into_iter()
        .map(|route| route.path_prefix)
        .collect();
    paths.sort();
    paths
}

/// Keep `id`'s actor busy for `delay`: it writes its route again (Create, which keeps it Active), that write waits
/// `delay`, and the actor answers nothing meanwhile. The handle says how the write went.
async fn busy(
    state: &ServerState,
    store: &SimEventStore,
    tenant: &TenantId,
    id: &str,
    path: &str,
    delay: Duration,
) -> tokio::task::JoinHandle<Result<temper_server::entity_actor::EntityResponse, String>> {
    store.inject_append_delay(&format!("{tenant}:HttpEndpoint:{id}"), delay);
    let (state, tenant, id) = (state.clone(), tenant.clone(), id.to_string());
    let params = json!({ "path_prefix": path, "methods": "POST", "integration_module": "passes", "requires_auth": false, "timeout_secs": 10 });
    // determinism-ok: the test's own concurrent write, so the rebuild meets a busy actor
    let handle = tokio::spawn(async move {
        state
            .dispatch_tenant_action(
                &tenant,
                "HttpEndpoint",
                &id,
                "Create",
                params,
                &AgentContext::default(),
            )
            .await
    });
    // Let the actor take the write before the rebuild asks it.
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    handle
}

#[tokio::test(start_paused = true)]
async fn a_rebuild_keeps_the_route_of_a_row_that_does_not_answer() {
    let (_guard, _clock, _ids) = install_deterministic_context(20);
    let (state, store) = build(20);
    let tenant = TenantId::default();
    create(&state, &tenant, "he-judge", "/katagami/judge").await;
    create(&state, &tenant, "he-pass-guest", "/katagami/pass/guest").await;
    rebuild_tenant_table(&state, &tenant).await;
    assert_eq!(
        paths(&state, &tenant).await,
        ["/katagami/judge", "/katagami/pass/guest"]
    );

    // The judge's row is busy far longer than any rebuild waits, and another route is registered meanwhile.
    let _write = busy(
        &state,
        &store,
        &tenant,
        "he-judge",
        "/katagami/judge",
        Duration::from_secs(600),
    )
    .await;
    create(&state, &tenant, "he-pass-files", "/katagami/pass/files").await;
    let started = tokio::time::Instant::now();
    rebuild_tenant_table_after(&state, &tenant, Some("he-pass-files")).await;
    assert!(
        started.elapsed() >= Duration::from_secs(5),
        "the rebuild asked the busy row before giving up on it"
    );
    assert_eq!(
        paths(&state, &tenant).await,
        [
            "/katagami/judge",
            "/katagami/pass/files",
            "/katagami/pass/guest"
        ],
        "a row the rebuild could not read keeps its route; the new one is added"
    );
}

#[tokio::test(start_paused = true)]
async fn the_row_that_changed_keeps_no_route_when_it_cannot_be_read() {
    let (_guard, _clock, _ids) = install_deterministic_context(23);
    let (state, store) = build(23);
    let tenant = TenantId::default();
    create(&state, &tenant, "he-judge", "/katagami/judge").await;
    create(&state, &tenant, "he-pass-guest", "/katagami/pass/guest").await;
    rebuild_tenant_table(&state, &tenant).await;

    // The guest pass's row changed and is busy past the rebuild's wait: its old route is what the change is about.
    let _write = busy(
        &state,
        &store,
        &tenant,
        "he-pass-guest",
        "/katagami/pass/guest",
        Duration::from_secs(600),
    )
    .await;
    rebuild_tenant_table_after(&state, &tenant, Some("he-pass-guest")).await;
    assert_eq!(paths(&state, &tenant).await, ["/katagami/judge"]);
}

#[tokio::test(start_paused = true)]
async fn a_rebuild_waits_for_a_row_that_answers_late_and_drops_a_paused_one() {
    let (_guard, _clock, _ids) = install_deterministic_context(21);
    let (state, store) = build(21);
    let tenant = TenantId::default();
    create(&state, &tenant, "he-judge", "/katagami/judge").await;
    create(&state, &tenant, "he-pass-guest", "/katagami/pass/guest").await;

    // At boot the table is empty, so there is no route to keep: a row that answers in seconds is waited for.
    let write = busy(
        &state,
        &store,
        &tenant,
        "he-judge",
        "/katagami/judge",
        Duration::from_secs(3),
    )
    .await;
    let started = tokio::time::Instant::now();
    rebuild_tenant_table(&state, &tenant).await;
    assert!(
        started.elapsed() >= Duration::from_secs(3),
        "the rebuild waited for the late row"
    );
    assert!(
        write
            .await
            .expect("the write ran")
            .expect("the write landed")
            .success
    );
    assert_eq!(
        paths(&state, &tenant).await,
        ["/katagami/judge", "/katagami/pass/guest"],
        "the late row was waited for"
    );

    // A row read as Paused leaves, even one the table had.
    let paused = state
        .dispatch_tenant_action(
            &tenant,
            "HttpEndpoint",
            "he-pass-guest",
            "Pause",
            json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("Pause");
    assert_eq!(paused.state.status, "Paused");
    rebuild_tenant_table(&state, &tenant).await;
    assert_eq!(
        paths(&state, &tenant).await,
        ["/katagami/judge"],
        "a paused row leaves the table"
    );
}

#[tokio::test(start_paused = true)]
async fn a_rebuild_loads_an_idle_row_again() {
    let (_guard, _clock, _ids) = install_deterministic_context(22);
    let (state, _store) = build(22);
    let tenant = TenantId::default();
    create(&state, &tenant, "he-judge", "/katagami/judge").await;
    create(&state, &tenant, "he-pass-guest", "/katagami/pass/guest").await;
    rebuild_tenant_table(&state, &tenant).await;
    assert_eq!(
        paths(&state, &tenant).await,
        ["/katagami/judge", "/katagami/pass/guest"]
    );

    // The judge's row goes idle and its actor stops, as an idle row's does in production.
    let key = format!("{tenant}:HttpEndpoint:he-judge");
    state
        .last_accessed
        .write()
        .unwrap()
        .insert(key.clone(), sim_now() - chrono::Duration::seconds(600));
    state.passivate_idle_actors().await;
    assert!(
        !state.actor_registry.read().unwrap().contains_key(&key),
        "the idle row's actor stopped"
    );

    create(&state, &tenant, "he-pass-files", "/katagami/pass/files").await;
    rebuild_tenant_table(&state, &tenant).await;
    assert_eq!(
        paths(&state, &tenant).await,
        [
            "/katagami/judge",
            "/katagami/pass/files",
            "/katagami/pass/guest"
        ],
        "the idle row was loaded again and read"
    );
    assert!(
        state.actor_registry.read().unwrap().contains_key(&key),
        "by its own actor, started again"
    );
}
