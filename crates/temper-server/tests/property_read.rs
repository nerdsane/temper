//! `GET /tdata/Set('key')/Property` reads one field of one record, whatever
//! its size.
//!
//! A whole-record read inlines a field kept out of line only up to its
//! hydration budget and leaves a larger one as a bare reference. The property
//! read is how a reader gets that field: authorized as a read of the record,
//! answered as `{"value": ...}` with the field's whole value.
//!
//! Fixture: a `Batch` with an `Items` list and a `Label`, on a Turso store,
//! which keeps a field larger than the inline limit out of line.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::storage::StorageStack;
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const CSDL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Prop" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Batch">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Items" Type="Collection(Edm.String)"/>
        <Property Name="Label" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="PropService">
        <EntitySet Name="Batches" EntityType="Temper.Prop.Batch"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const BATCH_IOA: &str = r#"
[automaton]
name = "Batch"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "Fill"
kind = "input"
from = ["Draft"]
params = ["Items", "Label"]
"#;

const PERMIT_ALL: &str = "permit(principal, action, resource);";

/// A list of one item whose JSON is 1.5 MiB: above both the whole read's
/// per-field budget (128 KiB) and its per-response budget (1 MiB), and below
/// the 2 MiB request body the write takes.
const LARGE_ITEMS_BYTES: usize = 3 * 512 * 1024;

struct Fixture {
    state: ServerState,
    store: TursoEventStore,
    _temp: tempfile::TempDir,
}

async fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("temporary test directory");
    let db_url = format!("file:{}", temp.path().join("events.db").display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create Turso test store");
    let mut registry = SpecRegistry::new();
    registry.register_tenant_with_constraints(
        "default",
        parse_csdl(CSDL).expect("CSDL parse"),
        CSDL.to_string(),
        &[("Batch", BATCH_IOA)],
        None,
    );
    let mut state = ServerState::from_registry(ActorSystem::new("property-read"), registry);
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Batch",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![EntityLevelSummary {
                level: "L0 SMT".to_string(),
                passed: true,
                summary: "OK".to_string(),
                details: None,
            }],
            verified_at: "2026-11-03T00:00:00Z".to_string(),
        }),
    );
    state
        .authz
        .reload_tenant_policies("default", PERMIT_ALL)
        .expect("fixture policy should parse");
    state.data_dir = temp.path().join("data");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    Fixture {
        state,
        store,
        _temp: temp,
    }
}

async fn send(state: &ServerState, method: Method, uri: &str, body: Value) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(
                "property-read",
                "test-agent",
                None,
            ),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn get(state: &ServerState, uri: &str) -> impl Future<Output = (StatusCode, Value)> {
    send(state, Method::GET, uri, Value::Null)
}

/// Keep failure output short: a body may echo the large list.
fn short(value: &Value) -> String {
    value.to_string().chars().take(400).collect()
}

/// Create batch `id` and fill it with `items` and the label "nightly".
async fn filled_batch(state: &ServerState, id: &str, items: &Value) {
    let (status, body) = send(state, Method::POST, "/tdata/Batches", json!({"Id": id})).await;
    assert_eq!(status, StatusCode::CREATED, "create {id}: {}", short(&body));
    let (status, body) = send(
        state,
        Method::POST,
        &format!("/tdata/Batches('{id}')/Temper.Prop.Fill"),
        json!({"Items": items, "Label": "nightly"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "fill {id}: {}", short(&body));
}

fn large_items() -> Value {
    // `["` + item + `"]`
    let items = json!(["x".repeat(LARGE_ITEMS_BYTES - 4)]);
    assert_eq!(serde_json::to_vec(&items).unwrap().len(), LARGE_ITEMS_BYTES);
    items
}

#[tokio::test]
async fn a_field_too_large_for_the_whole_read_reads_whole_on_its_own() {
    let fixture = fixture().await;
    let state = &fixture.state;
    let items = large_items();
    filled_batch(state, "big", &items).await;

    // The whole read leaves the list as its bare reference.
    let (status, record) = get(state, "/tdata/Batches('big')").await;
    assert_eq!(status, StatusCode::OK, "{}", short(&record));
    assert!(
        record["fields"]["Items"].get("__temper_blob_ref").is_some(),
        "the whole read inlines a 1.5 MiB field: {}",
        short(&record["fields"]["Items"])
    );

    let (status, body) = get(state, "/tdata/Batches('big')/Items").await;
    assert_eq!(status, StatusCode::OK, "{}", short(&body));
    assert_eq!(
        body["@odata.context"],
        "$metadata#Batches('big')/Items",
        "{}",
        short(&body)
    );
    assert!(
        body["value"] == items,
        "the list read whole: {}",
        short(&body)
    );
}

#[tokio::test]
async fn an_inline_field_reads_on_its_own() {
    let fixture = fixture().await;
    let state = &fixture.state;
    filled_batch(state, "small", &json!(["a", "b"])).await;

    let (status, body) = get(state, "/tdata/Batches('small')/Label").await;
    assert_eq!(status, StatusCode::OK, "{}", short(&body));
    assert_eq!(body["value"], "nightly");
    let (status, body) = get(state, "/tdata/Batches('small')/Items").await;
    assert_eq!(status, StatusCode::OK, "{}", short(&body));
    assert_eq!(body["value"], json!(["a", "b"]));
}

#[tokio::test]
async fn a_field_the_record_lacks_or_a_record_that_is_missing_is_not_found() {
    let fixture = fixture().await;
    let state = &fixture.state;
    filled_batch(state, "here", &json!(["a"])).await;

    let (status, body) = get(state, "/tdata/Batches('here')/Nothing").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", short(&body));
    let (status, body) = get(state, "/tdata/Batches('absent')/Label").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", short(&body));
}

#[tokio::test]
async fn a_reader_refused_the_record_is_refused_its_fields() {
    let fixture = fixture().await;
    let state = &fixture.state;
    filled_batch(state, "kept", &large_items()).await;
    state
        .authz
        .reload_tenant_policies(
            "default",
            &format!("{PERMIT_ALL}\nforbid(principal, action == Action::\"read\", resource);"),
        )
        .expect("refusing policy should parse");

    for field in ["Items", "Label"] {
        let (status, body) = get(state, &format!("/tdata/Batches('kept')/{field}")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{field}: {}", short(&body));
        assert!(body.get("value").is_none(), "{field}: {}", short(&body));
    }
}

async fn property_response(state: &ServerState, uri: &str) -> axum::response::Response {
    let mut request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(
                "property-read",
                "test-agent",
                None,
            ),
        ));
    build_router(state.clone()).oneshot(request).await.unwrap()
}

#[tokio::test]
async fn a_legacy_field_streams_in_bounded_chunks_with_its_content_length() {
    use futures_util::StreamExt as _;
    let fixture = fixture().await;
    let state = &fixture.state;
    let items = large_items();
    filled_batch(state, "legacy", &items).await;
    let (status, record) = get(state, "/tdata/Batches('legacy')").await;
    assert_eq!(status, StatusCode::OK);
    let key = record["fields"]["Items"]["__temper_blob_ref"]
        .as_str()
        .unwrap();
    // Today's overflow writes go only to object storage. Seed the legacy row
    // explicitly, then remove the object, as on pre-migration deployments.
    fixture
        .store
        .put_blob(key, &serde_json::to_vec(&items).unwrap())
        .await
        .unwrap();
    // Only the old database copy remains, as on pre-migration deployments.
    tokio::fs::remove_dir_all(state.data_dir.join("blobs"))
        .await
        .unwrap();
    let response = property_response(state, "/tdata/Batches('legacy')/Items").await;
    assert_eq!(response.status(), StatusCode::OK);
    let length = response.headers()["Content-Length"]
        .to_str()
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let mut stream = response.into_body().into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        assert!(
            chunk.len() <= 64 * 1024,
            "legacy read buffered {} bytes",
            chunk.len()
        );
        bytes.extend_from_slice(&chunk);
    }
    assert_eq!(length, bytes.len());
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(body["value"] == items);
}

#[tokio::test]
async fn postgres_actor_fields_share_the_whole_record_loader_and_authorization() {
    use std::sync::Arc;
    use temper_actor_runtime::{ActorSystem as PgActors, SchedulerConfig, SpecDrivenActor};
    let (pool, _container) = temper_actor_runtime::test_utils::setup_test_pg().await;
    let actors = Arc::new(PgActors::new(pool, SchedulerConfig::default()));
    actors
        .register(Arc::new(SpecDrivenActor::from_ioa(BATCH_IOA).unwrap()))
        .await
        .unwrap();
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).unwrap(),
        CSDL.into(),
        &[("Batch", BATCH_IOA)],
    );
    registry.set_verification_status(
        &TenantId::default(),
        "Batch",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "2026-11-03T00:00:00Z".into(),
        }),
    );
    let mut state = ServerState::from_pg_registry(actors, registry);
    state.actor_backed_types.insert("Batch".into());
    state
        .authz
        .reload_tenant_policies("default", PERMIT_ALL)
        .unwrap();
    let id = temper_runtime::scheduler::sim_uuid().to_string();
    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Batches",
        json!({"Id": id, "Label": "postgres"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let path = format!("/tdata/Batches('{id}')");
    let (status, whole) = get(&state, &path).await;
    assert_eq!(status, StatusCode::OK, "{whole}");
    let (status, property) = get(&state, &format!("{path}/Label")).await;
    assert_eq!(status, StatusCode::OK, "{property}");
    assert_eq!(property["value"], "postgres");
    assert_eq!(property["value"], whole["fields"]["Label"]);
    state
        .authz
        .reload_tenant_policies(
            "default",
            &format!("{PERMIT_ALL}\nforbid(principal, action == Action::\"read\", resource);"),
        )
        .unwrap();
    let (status, body) = get(&state, &format!("{path}/Label")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.get("value").is_none());
    let (status, _) = get(&state, "/tdata/Batches('absent')/Label").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
