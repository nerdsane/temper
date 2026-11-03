//! A spec's new declarations must read the same before and during a move.
#[path = "declared_initial_reads/postgres.rs"]
mod postgres;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_server::registry::{EntityVerificationResult, SpecRegistry, VerificationStatus};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const CSDL: &str = r#"<?xml version="1.0"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
<edmx:DataServices><Schema Namespace="Temper" xmlns="http://docs.oasis-open.org/odata/ns/edm">
<EntityType Name="Topping"><Key><PropertyRef Name="Id"/></Key>
<Property Name="Id" Type="Edm.String" Nullable="false"/>
<Property Name="label" Type="Edm.String" Nullable="false"/>
</EntityType><EntityContainer Name="Container"><EntitySet Name="Toppings" EntityType="Temper.Topping"/>
</EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;
const V1: &str = r#"
[automaton]
name = "Topping"
states = ["Ready"]
initial = "Ready"
strict_action_params = true
[[action]]
name = "Touch"
params = [{ name = "label", type = "string" }]
kind = "input"
from = ["Ready"]
to = "Ready"
"#;
const ADDED: &str = r#"
[[state]]
name = "stock"
type = "counter"
initial = 6
[[state]]
name = "available"
type = "bool"
initial = true
[[state]]
name = "label"
type = "string"
initial = "fresh"
[[state]]
name = "temperature"
type = "int"
initial = -2
[[state]]
name = "tags"
type = "list"
initial = ["new"]
[[action]]
name = "Delivered"
kind = "input"
from = ["Ready"]
to = "Ready"
params = [{ name = "amount", type = "counter" }]
effect = ["stock += params.amount"]
[[action]]
name = "Reset"
kind = "input"
from = ["Ready"]
to = "Ready"
effect = ["stock = 0", "available = false"]
"#;

fn install(state: &ServerState, tenant: &str, ioa: &str) {
    let mut registry = state.registry.write().unwrap();
    registry.register_tenant(
        tenant,
        parse_csdl(CSDL).unwrap(),
        CSDL.to_string(),
        &[("Topping", ioa)],
    );
    registry.set_verification_status(
        &TenantId::from(tenant),
        "Topping",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "2026-10-31T00:00:00Z".into(),
        }),
    );
    state
        .authz
        .reload_tenant_policies(tenant, "permit(principal, action, resource);")
        .unwrap();
}

async fn request(
    state: &ServerState,
    tenant: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("Content-Type", "application/json")
        .body(if body.is_null() {
            Body::empty()
        } else {
            Body::from(body.to_string())
        })
        .unwrap();
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::from(tenant),
            temper_authz::SecurityContext::from_resolved_identity("test", "test", None),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap();
    (status, body)
}

async fn read(state: &ServerState, path: &str) -> Value {
    let (status, body) = request(state, "default", "GET", path, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
    body
}

fn assert_defaults(body: &Value) {
    assert_eq!(body["counters"]["stock"], 6, "{body}");
    assert_eq!(body["booleans"]["available"], true, "{body}");
    assert_eq!(body["fields"]["stock"], 6, "{body}");
    assert_eq!(body["fields"]["available"], true, "{body}");
    assert_eq!(body["fields"]["label"], "fresh", "{body}");
    assert_eq!(body["fields"]["temperature"], -2, "{body}");
    assert_eq!(body["lists"]["tags"], json!(["new"]), "{body}");
}

#[tokio::test]
async fn catalog_reads_agree_with_moves_after_adding_declared_initial_values() {
    let dir = tempfile::tempdir().unwrap();
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("initial.db").display()),
        None,
    )
    .await
    .unwrap();
    let mut state = ServerState::from_registry(
        ActorSystem::new("declared-initial-reads"),
        SpecRegistry::new(),
    );
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    install(&state, "default", V1);
    for id in ["moved", "untouched"] {
        let (status, body) = request(
            &state,
            "default",
            "POST",
            "/tdata/Toppings",
            json!({"id":id}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    install(&state, "default", &format!("{V1}\n{ADDED}"));
    let collection = read(&state, "/tdata/Toppings").await;
    let single = read(&state, "/tdata/Toppings('moved')").await;
    let scalar_selected = read(&state, "/tdata/Toppings?$select=stock,available,label,tags").await;
    let selected = read(
        &state,
        "/tdata/Toppings?$select=counters,booleans,label,temperature,lists",
    )
    .await;
    let stored = store
        .load_entity_catalog_rows("default", "Topping", &["moved".into()])
        .await
        .unwrap();
    assert!(
        stored[0].state.as_ref().unwrap()["counters"]
            .get("stock")
            .is_none(),
        "a read must not rewrite the committed projection"
    );
    let (status, moved) = request(
        &state,
        "default",
        "POST",
        "/tdata/Toppings('moved')/Temper.Delivered",
        json!({"amount":6}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{moved}");
    let after = read(&state, "/tdata/Toppings('moved')").await;
    eprintln!(
        "before collection: {collection}\nbefore single: {single}\nbefore select: {selected}\nafter Delivered(+6): {after}"
    );
    assert_eq!(after["counters"]["stock"], 12, "{after}");
    assert_defaults(&single);
    for entity in collection["value"].as_array().unwrap() {
        assert_defaults(entity);
    }
    for entity in scalar_selected["value"].as_array().unwrap() {
        assert_eq!(entity["stock"], 6, "{entity}");
        assert_eq!(entity["available"], true, "{entity}");
        assert_eq!(entity["label"], "fresh", "{entity}");
        assert_eq!(entity["tags"], json!(["new"]), "{entity}");
    }
    for entity in selected["value"].as_array().unwrap() {
        assert_eq!(entity["counters"]["stock"], 6, "{entity}");
        assert_eq!(entity["booleans"]["available"], true, "{entity}");
        assert_eq!(entity["label"], "fresh", "{entity}");
        assert_eq!(entity["temperature"], -2, "{entity}");
        assert_eq!(entity["lists"]["tags"], json!(["new"]), "{entity}");
    }

    for filter in [
        "label%20eq%20'fresh'",
        "available%20eq%20true",
        "stock%20eq%206",
    ] {
        let body = read(
            &state,
            &format!("/tdata/Toppings?$filter={filter}&$count=true"),
        )
        .await;
        let expected = if filter.starts_with("stock") { 1 } else { 2 };
        assert_eq!(
            body["value"].as_array().unwrap().len(),
            expected,
            "{filter}: {body}"
        );
        assert_eq!(body["@odata.count"], expected, "{filter}: {body}");
    }
    let (status, body) = request(
        &state,
        "default",
        "POST",
        "/tdata/Toppings('moved')/Temper.Reset",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let reset = read(&state, "/tdata/Toppings('moved')").await;
    assert_eq!(reset["counters"]["stock"], 0, "{reset}");
    assert_eq!(reset["booleans"]["available"], false, "{reset}");
    assert_eq!(reset["fields"]["stock"], 0, "{reset}");
    assert_eq!(reset["fields"]["available"], false, "{reset}");
    let available = read(&state, "/tdata/Toppings?$filter=available%20eq%20true").await;
    assert_eq!(
        available["value"].as_array().unwrap().len(),
        1,
        "{available}"
    );
    assert_eq!(
        available["value"][0]["entity_id"], "untouched",
        "{available}"
    );
}

#[tokio::test]
async fn actor_reads_agree_with_declared_values_across_spec_changes() {
    use temper_runtime::scheduler::install_deterministic_context;
    use temper_store_sim::SimEventStore;

    for seed in 0..16 {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let mut state =
            ServerState::from_registry(ActorSystem::new("initial-actor-read"), SpecRegistry::new());
        let store = SimEventStore::no_faults(seed);
        state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
        let v1 = V1.replace(
            "strict_action_params = true",
            &format!("strict_action_params = {}", seed % 2 == 0),
        );
        install(&state, "default", &v1);
        let (status, body) = request(
            &state,
            "default",
            "POST",
            "/tdata/Toppings",
            json!({"id":"old"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "seed {seed}: {body}");
        let stock = seed + 1;
        let added = ADDED.replace("initial = 6", &format!("initial = {stock}"));
        install(&state, "default", &format!("{v1}\n{added}"));
        let single = read(&state, "/tdata/Toppings('old')").await;
        assert_eq!(single["counters"]["stock"], stock, "seed {seed}: {single}");
        assert_eq!(
            single["booleans"]["available"], true,
            "seed {seed}: {single}"
        );
        assert_eq!(single["fields"]["label"], "fresh", "seed {seed}: {single}");
        let list = read(&state, "/tdata/Toppings").await;
        assert_eq!(
            list["value"][0]["counters"]["stock"], stock,
            "seed {seed}: {list}"
        );
        let (status, moved) = request(
            &state,
            "default",
            "POST",
            "/tdata/Toppings('old')/Temper.Delivered",
            json!({"amount":6}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "seed {seed}: {moved}");
        let after = read(&state, "/tdata/Toppings('old')").await;
        assert_eq!(
            after["counters"]["stock"],
            stock + 6,
            "seed {seed}: {after}"
        );
        let mut restarted = ServerState::from_registry(
            ActorSystem::new("initial-actor-restart"),
            SpecRegistry::new(),
        );
        restarted.set_storage_stack(StorageStack::from_sim(store, None));
        install(&restarted, "default", &format!("{v1}\n{added}"));
        let cold = read(&restarted, "/tdata/Toppings('old')").await;
        assert_eq!(cold["counters"]["stock"], stock + 6, "seed {seed}: {cold}");
        assert_eq!(cold["booleans"]["available"], true, "seed {seed}: {cold}");
        assert_eq!(cold["fields"]["label"], "fresh", "seed {seed}: {cold}");
    }
}

#[tokio::test]
async fn declared_initial_fields_sort_with_stored_values_and_stay_tenant_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("sort.db").display()),
        None,
    )
    .await
    .unwrap();
    let mut state =
        ServerState::from_registry(ActorSystem::new("initial-sort"), SpecRegistry::new());
    state.set_storage_stack(StorageStack::from_turso(store));
    for tenant in ["default", "other"] {
        install(&state, tenant, V1);
        let (status, body) = request(
            &state,
            tenant,
            "POST",
            "/tdata/Toppings",
            json!({"id":"old"}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    install(&state, "default", &format!("{V1}\n{ADDED}"));
    let other = ADDED
        .replace("initial = 6", "initial = 21")
        .replace("initial = \"fresh\"", "initial = \"other\"");
    install(&state, "other", &format!("{V1}\n{other}"));
    let (status, other) = request(
        &state,
        "other",
        "GET",
        "/tdata/Toppings('old')",
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{other}");
    assert_eq!(other["counters"]["stock"], 21, "{other}");
    assert_eq!(other["fields"]["label"], "other", "{other}");
    for (id, label) in [
        ("low", "alpha".to_string()),
        ("high", "zulu".to_string()),
        ("large", "z".repeat(2101)),
    ] {
        let (status, body) = request(
            &state,
            "default",
            "POST",
            "/tdata/Toppings",
            json!({"id":id}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let (status, body) = request(
            &state,
            "default",
            "POST",
            &format!("/tdata/Toppings('{id}')/Temper.Touch"),
            json!({"label":label}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    // A nonempty indexed match must also include the older row with no indexed flag.
    let available = read(
        &state,
        "/tdata/Toppings?$filter=available%20eq%20true&$count=true",
    )
    .await;
    assert_eq!(
        available["value"].as_array().unwrap().len(),
        4,
        "{available}"
    );
    assert_eq!(available["@odata.count"], 4, "{available}");
    // Oversized stored strings have no field-index entry either. They must
    // be rejected before pagination/count, rather than treated as defaults.
    let filtered = read(
        &state,
        "/tdata/Toppings?$filter=label%20eq%20'fresh'&$top=1&$count=true",
    )
    .await;
    assert_eq!(filtered["value"].as_array().unwrap().len(), 1, "{filtered}");
    assert_eq!(filtered["value"][0]["entity_id"], "old", "{filtered}");
    assert_eq!(filtered["@odata.count"], 1, "{filtered}");
    let sorted = read(&state, "/tdata/Toppings?$orderby=label%20asc&$count=true").await;
    let ids: Vec<_> = sorted["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["entity_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["low", "old", "high", "large"], "{sorted}");
    assert_eq!(sorted["@odata.count"], 4, "{sorted}");
    let (status, body) = request(
        &state,
        "default",
        "POST",
        "/tdata/Toppings",
        json!({"id":"fresh"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let changed = ADDED
        .replace("initial = 6", "initial = 21")
        .replace("initial = true", "initial = false");
    install(&state, "default", &format!("{V1}\n{changed}"));
    let fresh = read(&state, "/tdata/Toppings('fresh')").await;
    assert_eq!(fresh["counters"]["stock"], 6, "{fresh}");
    let selected = read(&state, "/tdata/Toppings?$select=entity_id,stock").await;
    let fresh = selected["value"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["entity_id"] == "fresh")
        .unwrap();
    assert_eq!(
        fresh["stock"], 6,
        "a selected read must retain the creation value: {fresh}"
    );
    let filtered = read(
        &state,
        "/tdata/Toppings?$filter=available%20eq%20true&$count=true",
    )
    .await;
    assert_eq!(filtered["@odata.count"], 4, "{filtered}");
    assert!(
        filtered["value"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["entity_id"] == "fresh"),
        "a stored true value must survive a later false default: {filtered}"
    );
}
