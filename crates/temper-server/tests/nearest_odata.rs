//! Integration test for the ADR-0155 `Temper.Nearest` bound function, driven
//! end to end through the real axum OData router: register a vector-declaring
//! entity, create entities (whose vectors co-commit), then GET the kNN function
//! and assert the OData list shape, ranking order, per-row `@temper.score`, and
//! self-exclusion.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EntityVectorRow, EventStore};
use temper_runtime::tenant::TenantId;
use temper_server::build_router;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

const VEC_ITEM_IOA: &str = r#"
[automaton]
name = "VecItem"
states = ["New", "Ready", "Deleted"]
initial = "New"

[[state]]
name = "Embedding"
type = "string"
initial = ""

[[state]]
name = "EmbeddingModel"
type = "string"
initial = ""

[[state]]
name = "Category"
type = "string"
initial = ""

[[vector]]
name = "embed"
property = "Embedding"
model_property = "EmbeddingModel"
dims = 4
metric = "cosine"

[[action]]
name = "Create"
kind = "input"
from = ["New"]
to = "Ready"
params = ["Embedding", "EmbeddingModel", "Category"]

[[action]]
name = "Delete"
kind = "input"
from = ["Ready"]
to = "Deleted"
"#;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="VecItem">
        <Key>
          <PropertyRef Name="Id"/>
        </Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Embedding" Type="Edm.String"/>
        <Property Name="EmbeddingModel" Type="Edm.String"/>
        <Property Name="Category" Type="Edm.String"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="TestService">
        <EntitySet Name="VecItems" EntityType="Temper.Test.VecItem"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"#;

fn build_unreconciled_state_from_store(store: SimEventStore, system_name: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("VecItem", VEC_ITEM_IOA)],
    );
    let system = ActorSystem::new(system_name);
    let mut state = ServerState::from_registry(system, registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state
}

fn build_unreconciled_state_with_store() -> (ServerState, SimEventStore) {
    let store = SimEventStore::no_faults(7);
    (
        build_unreconciled_state_from_store(store.clone(), "nearest-odata"),
        store,
    )
}

fn build_unreconciled_state() -> ServerState {
    build_unreconciled_state_with_store().0
}

async fn build_state() -> ServerState {
    build_state_with_store().await.0
}

async fn build_state_with_store() -> (ServerState, SimEventStore) {
    let (state, store) = build_unreconciled_state_with_store();
    assert!(
        state
            .reconcile_declared_projections(&TenantId::from("default"))
            .await,
        "initial vector generation should reconcile"
    );
    (state, store)
}

async fn create_item(
    state: &ServerState,
    tenant: &TenantId,
    id: &str,
    embedding: &[f32],
    model: &str,
) {
    create_item_cat(state, tenant, id, embedding, model, "std").await;
}

async fn create_item_cat(
    state: &ServerState,
    tenant: &TenantId,
    id: &str,
    embedding: &[f32],
    model: &str,
    category: &str,
) {
    let embedding_json = serde_json::to_string(embedding).unwrap();
    let response = state
        .dispatch_tenant_action(
            tenant,
            "VecItem",
            id,
            "Create",
            serde_json::json!({ "Embedding": embedding_json, "EmbeddingModel": model, "Category": category }),
            &AgentContext::default(),
        )
        .await
        .expect("dispatch Create");
    assert!(response.success, "Create {id} failed: {:?}", response.error);
}

async fn delete_item(state: &ServerState, tenant: &TenantId, id: &str) {
    let response = state
        .dispatch_tenant_action(
            tenant,
            "VecItem",
            id,
            "Delete",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("dispatch Delete");
    assert!(response.success, "Delete {id} failed: {:?}", response.error);
}

async fn get_json(state: &ServerState, path: &str) -> (StatusCode, serde_json::Value) {
    let router = build_router(state.clone());
    let req = Request::builder().uri(path).body(Body::empty()).unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

#[tokio::test]
async fn nearest_bound_function_ranks_and_scores_over_http() {
    let state = build_state().await;
    let tenant = TenantId::from("default");

    // item-a is the query reference; item-b is close to it, item-c orthogonal.
    create_item(&state, &tenant, "item-a", &[1.0, 0.0, 0.0, 0.0], "m1").await;
    create_item(&state, &tenant, "item-b", &[0.9, 0.1, 0.0, 0.0], "m1").await;
    create_item(&state, &tenant, "item-c", &[0.0, 1.0, 0.0, 0.0], "m1").await;

    let (status, body) = get_json(
        &state,
        "/tdata/VecItems/Temper.Nearest(decl='embed',to='item-a',k=5)",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    let value = body["value"].as_array().expect("value array");
    // item-a is the reference and is excluded from its own results.
    assert_eq!(
        value.len(),
        2,
        "expected two ranked neighbours, got: {body}"
    );

    let ids: Vec<&str> = value
        .iter()
        .map(|e| e["entity_id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        ids,
        vec!["item-b", "item-c"],
        "ranking order (nearest first)"
    );

    // Every row carries a numeric @temper.score, descending (nearest first).
    let score0 = value[0]["@temper.score"].as_f64().expect("score 0");
    let score1 = value[1]["@temper.score"].as_f64().expect("score 1");
    assert!(
        score0 > score1,
        "scores must be descending: {score0} !> {score1}"
    );

    // A raw-vector query with a model tag ranks the same partition.
    let (status, body) = get_json(
        &state,
        "/tdata/VecItems/Temper.Nearest(decl='embed',vector='%5B1,0,0,0%5D',k=1,model='m1')",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "raw-vector body: {body}");
    let value = body["value"].as_array().expect("value array");
    assert_eq!(value.len(), 1);
    assert_eq!(
        value[0]["entity_id"].as_str(),
        Some("item-a"),
        "nearest to [1,0,0,0] is item-a"
    );
}

#[tokio::test]
async fn nearest_fails_closed_until_current_vector_generation_is_reconciled() {
    let state = build_unreconciled_state();
    let tenant = TenantId::from("default");
    create_item(&state, &tenant, "item-a", &[1.0, 0.0, 0.0, 0.0], "m1").await;

    let (status, body) = get_json(
        &state,
        "/tdata/VecItems/Temper.Nearest(decl='embed',vector='%5B1,0,0,0%5D',k=1,model='m1')",
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert_eq!(body["error"]["code"], "VectorIndexRebuilding");
}

#[tokio::test]
async fn nearest_waits_through_exact_vector_reconciliation() {
    let (state, store) = build_state_with_store().await;
    let tenant = TenantId::from("default");
    let entity_id = "item-a";
    create_item(&state, &tenant, entity_id, &[1.0, 0.0, 0.0, 0.0], "m1").await;
    let persistence_id = format!("{tenant}:VecItem:{entity_id}");
    let source_sequence = store
        .read_events(&persistence_id, 0)
        .await
        .expect("read vector entity journal")
        .last()
        .expect("created entity has journal history")
        .sequence_nr;

    let reconciliation = store
        .acquire_projection_reconciliation_fence(tenant.as_str(), "VecItem")
        .await
        .expect("acquire exact vector reconciliation fence");
    store
        .backfill_entity_vectors(tenant.as_str(), "VecItem", entity_id, source_sequence, &[])
        .await
        .expect("purge vector inside exact reconciliation");

    let request = get_json(
        &state,
        "/tdata/VecItems/Temper.Nearest(decl='embed',vector='%5B1,0,0,0%5D',k=1,model='m1')",
    );
    tokio::pin!(request);
    let remained_blocked = tokio::select! {
        biased;
        result = &mut request => panic!("nearest observed the vector purge window: {result:?}"),
        _ = tokio::task::yield_now() => true,
    };
    assert!(remained_blocked);

    store
        .backfill_entity_vectors(
            tenant.as_str(),
            "VecItem",
            entity_id,
            source_sequence,
            &[EntityVectorRow {
                decl_name: "embed".to_string(),
                model_tag: "m1".to_string(),
                vector: vec![1.0, 0.0, 0.0, 0.0],
            }],
        )
        .await
        .expect("restore exact vector row");
    drop(reconciliation);

    let (status, body) = request.await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["value"][0]["entity_id"], entity_id);
}

#[tokio::test]
async fn nearest_cold_materialization_yields_fair_locks_to_a_queued_generation_writer() {
    let (seed_state, store) = build_state_with_store().await;
    let tenant = TenantId::from("default");
    create_item(
        &seed_state,
        &tenant,
        "cold-item",
        &[1.0, 0.0, 0.0, 0.0],
        "m1",
    )
    .await;
    drop(seed_state);

    // A fresh server has durable vector/catalog authority but no live actor.
    // Materializing the ranked candidate therefore exercises actor pre-start.
    let state = build_unreconciled_state_from_store(store.clone(), "nearest-cold-start");
    let projection_writer = store
        .acquire_projection_reconciliation_fence(tenant.as_str(), "VecItem")
        .await
        .expect("hold projection writer");
    let request = get_json(
        &state,
        "/tdata/VecItems/Temper.Nearest(decl='embed',vector='%5B1,0,0,0%5D',k=1,model='m1')",
    );
    tokio::pin!(request);
    tokio::select! {
        biased;
        result = &mut request => panic!("nearest bypassed the held projection writer: {result:?}"),
        _ = tokio::task::yield_now() => {}
    }

    // Nearest is now waiting on the projection read side while retaining its
    // staged generation reader. Queue a generation writer behind it. Once the
    // projection writer releases, Nearest must copy candidates and drop BOTH
    // read guards before cold actor startup; otherwise fair lock ordering cycles.
    let generation_writer = state.acquire_spec_generation_lock(&tenant);
    tokio::pin!(generation_writer);
    tokio::select! {
        biased;
        _ = &mut generation_writer => panic!("generation writer bypassed nearest's staged read"),
        _ = tokio::task::yield_now() => {}
    }
    drop(projection_writer);

    let mut writer_completed = false;
    let mut response = None;
    for _ in 0..128 {
        tokio::select! {
            biased;
            result = &mut request, if response.is_none() => {
                response = Some(result);
            }
            guard = &mut generation_writer, if !writer_completed => {
                drop(guard);
                writer_completed = true;
            }
            _ = tokio::task::yield_now() => {}
        }
        if writer_completed && response.is_some() {
            break;
        }
    }
    assert!(
        writer_completed,
        "queued generation writer must complete before cold actor materialization"
    );
    let (status, body) = response
        .expect("cold actor materialization must not deadlock behind nested fair-lock readers");
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert_eq!(body["error"]["code"], "VectorIndexRebuilding");
}

#[tokio::test]
async fn nearest_rejects_unknown_decl() {
    let state = build_state().await;
    let tenant = TenantId::from("default");
    create_item(&state, &tenant, "item-a", &[1.0, 0.0, 0.0, 0.0], "m1").await;

    let (status, body) = get_json(
        &state,
        "/tdata/VecItems/Temper.Nearest(decl='nope',to='item-a',k=5)",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

#[tokio::test]
async fn nearest_excludes_deleted_and_404s_on_deleted_reference() {
    let state = build_state().await;
    let tenant = TenantId::from("default");
    create_item(&state, &tenant, "item-a", &[1.0, 0.0, 0.0, 0.0], "m1").await;
    create_item(&state, &tenant, "item-b", &[0.9, 0.1, 0.0, 0.0], "m1").await;
    create_item(&state, &tenant, "item-c", &[0.0, 1.0, 0.0, 0.0], "m1").await;
    // Soft-delete item-b — its vector row is purged at write time (the actor emits no
    // row for a Deleted status), and the read-side status filter is the backstop.
    delete_item(&state, &tenant, "item-b").await;

    let (status, body) = get_json(
        &state,
        "/tdata/VecItems/Temper.Nearest(decl='embed',to='item-a',k=10)",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let ids: Vec<&str> = body["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["entity_id"].as_str().unwrap_or(""))
        .collect();
    assert!(
        !ids.contains(&"item-b"),
        "a deleted entity must never be ranked; got {ids:?}"
    );
    assert_eq!(ids, vec!["item-c"], "only the live neighbour remains");

    // A deleted reference is treated as absent.
    let (status, _body) = get_json(
        &state,
        "/tdata/VecItems/Temper.Nearest(decl='embed',to='item-b',k=5)",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "to='<deleted>' must 404");
}

#[tokio::test]
async fn nearest_applies_equality_filter_before_top_k() {
    let state = build_state().await;
    let tenant = TenantId::from("default");
    // Two "red" and one "blue" — all near the query vector, so without the filter the
    // blue one would rank; the filter must exclude it before top-k.
    create_item_cat(&state, &tenant, "red-1", &[1.0, 0.0, 0.0, 0.0], "m1", "red").await;
    create_item_cat(
        &state,
        &tenant,
        "blue-1",
        &[0.99, 0.01, 0.0, 0.0],
        "m1",
        "blue",
    )
    .await;
    create_item_cat(&state, &tenant, "red-2", &[0.9, 0.1, 0.0, 0.0], "m1", "red").await;

    let (status, body) = get_json(
        &state,
        "/tdata/VecItems/Temper.Nearest(decl='embed',vector='%5B1,0,0,0%5D',k=10,model='m1',filter='Category%20eq%20''red''')",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let ids: Vec<&str> = body["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["entity_id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        ids,
        vec!["red-1", "red-2"],
        "only red items, ranked; blue filtered out"
    );
}

#[tokio::test]
async fn nearest_authorizes_reference_and_walk_rows() {
    let state = build_state().await;
    let tenant = TenantId::from("default");
    create_item(&state, &tenant, "item-a", &[1.0, 0.0, 0.0, 0.0], "m1").await;
    create_item(
        &state,
        &tenant,
        "item-secret",
        &[0.95, 0.05, 0.0, 0.0],
        "m1",
    )
    .await;
    create_item(&state, &tenant, "item-c", &[0.0, 1.0, 0.0, 0.0], "m1").await;

    // Permit list + read, but forbid read on item-secret specifically.
    state
        .authz
        .reload_tenant_policies(
            tenant.as_str(),
            r#"
                permit(principal, action in [Action::"list", Action::"read"], resource is VecItem);
                forbid(principal, action == Action::"read", resource == VecItem::"item-secret");
            "#,
        )
        .expect("install Cedar policy");

    // Walk: a row the caller may not read is skipped, not leaked, in the ranking.
    let (status, body) = get_json(
        &state,
        "/tdata/VecItems/Temper.Nearest(decl='embed',to='item-a',k=10)",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let ids: Vec<&str> = body["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["entity_id"].as_str().unwrap_or(""))
        .collect();
    assert!(
        !ids.contains(&"item-secret"),
        "a read-denied row must not be served by Nearest; got {ids:?}"
    );
    assert_eq!(ids, vec!["item-c"]);

    // Reference: reading a forbidden entity as the query seed is denied, not disclosed.
    let (status, _body) = get_json(
        &state,
        "/tdata/VecItems/Temper.Nearest(decl='embed',to='item-secret',k=5)",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "reading a forbidden reference entity must be denied"
    );
}

#[tokio::test]
async fn nearest_rejects_system_query_options() {
    let state = build_state().await;
    let tenant = TenantId::from("default");
    create_item(&state, &tenant, "item-a", &[1.0, 0.0, 0.0, 0.0], "m1").await;

    // $top is not layered over the ranked result — reject it, don't silently ignore.
    let (status, body) = get_json(
        &state,
        "/tdata/VecItems/Temper.Nearest(decl='embed',to='item-a',k=5)?$top=1",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}

#[tokio::test]
async fn nearest_rejects_duplicate_named_argument() {
    let state = build_state().await;
    let tenant = TenantId::from("default");
    create_item(&state, &tenant, "item-a", &[1.0, 0.0, 0.0, 0.0], "m1").await;

    let (status, _body) = get_json(
        &state,
        "/tdata/VecItems/Temper.Nearest(decl='embed',to='item-a',k=5,k=10)",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "duplicate 'k' must be rejected"
    );
}
