//! Integration test for the ADR-0155 `Temper.Nearest` bound function, driven
//! end to end through the real axum OData router: register a vector-declaring
//! entity, create entities (whose vectors co-commit), then GET the kNN function
//! and assert the OData list shape, ranking order, per-row `@temper.score`, and
//! self-exclusion.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
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
states = ["New", "Ready"]
initial = "New"

[[state]]
name = "Embedding"
type = "string"
initial = ""

[[state]]
name = "EmbeddingModel"
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
params = ["Embedding", "EmbeddingModel"]
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
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="TestService">
        <EntitySet Name="VecItems" EntityType="Temper.Test.VecItem"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"#;

fn build_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("VecItem", VEC_ITEM_IOA)],
    );
    let system = ActorSystem::new("nearest-odata");
    let mut state = ServerState::from_registry(system, registry);
    state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(7), None));
    state
}

async fn create_item(
    state: &ServerState,
    tenant: &TenantId,
    id: &str,
    embedding: &[f32],
    model: &str,
) {
    let embedding_json = serde_json::to_string(embedding).unwrap();
    let response = state
        .dispatch_tenant_action(
            tenant,
            "VecItem",
            id,
            "Create",
            serde_json::json!({ "Embedding": embedding_json, "EmbeddingModel": model }),
            &AgentContext::default(),
        )
        .await
        .expect("dispatch Create");
    assert!(response.success, "Create {id} failed: {:?}", response.error);
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
    let state = build_state();
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
async fn nearest_rejects_unknown_decl() {
    let state = build_state();
    let tenant = TenantId::from("default");
    create_item(&state, &tenant, "item-a", &[1.0, 0.0, 0.0, 0.0], "m1").await;

    let (status, body) = get_json(
        &state,
        "/tdata/VecItems/Temper.Nearest(decl='nope',to='item-a',k=5)",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
}
