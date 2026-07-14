//! ARN-240: GET /observe/evolution/feature-requests must be idempotent.
//!
//! The handler generates feature requests from trajectory gaps on every read.
//! Each generated record minted a fresh UUID-suffixed id, so the store
//! "upsert" inserted a NEW row per GET, and a fresh `FR-{uuid}` system entity
//! was dispatched per generated record per GET — reads spawned unbounded
//! duplicates, and re-generation clobbered developer-owned fields.

use std::collections::BTreeMap;

use crate::registry::SpecRegistry;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use temper_runtime::ActorSystem;

use crate::state::{ServerState, TrajectoryEntry, TrajectorySource};
use crate::storage::StorageStack;

fn failing_platform_entry(n: u64) -> TrajectoryEntry {
    TrajectoryEntry {
        timestamp: format!("2026-07-13T00:00:{n:02}Z"),
        tenant: "arn240".to_string(),
        entity_type: "Invoice".to_string(),
        entity_id: format!("inv-{n}"),
        action: "GenerateInvoice".to_string(),
        success: false,
        from_status: None,
        to_status: None,
        error: Some("EntitySetNotFound: Invoice".to_string()),
        agent_id: Some("agent-1".to_string()),
        session_id: None,
        authz_denied: None,
        denied_resource: None,
        denied_module: None,
        source: Some(TrajectorySource::Platform),
        spec_governed: Some(false),
        agent_type: None,
        intent: None,
        request_body: None,
        matched_policy_ids: None,
    }
}

const FEATURE_REQUEST_IOA: &str = r#"
[automaton]
name = "FeatureRequest"
states = ["New", "Ready"]
initial = "New"

[[state]]
name = "category"
type = "string"
initial = ""

[[state]]
name = "description"
type = "string"
initial = ""

[[state]]
name = "frequency"
type = "string"
initial = ""

[[state]]
name = "developer_notes"
type = "string"
initial = ""

[[state]]
name = "legacy_record_id"
type = "string"
initial = ""

[[action]]
name = "CreateFeatureRequest"
kind = "input"
from = ["New"]
to = "Ready"
params = ["category", "description", "frequency", "developer_notes", "legacy_record_id"]
hint = "Record a platform gap surfaced by the insight generator."
"#;

const FEATURE_REQUEST_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.System" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="FeatureRequest">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="FeatureRequests" EntityType="Temper.System.FeatureRequest"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

fn registry_with_system_feature_request_spec() -> SpecRegistry {
    let mut registry = SpecRegistry::new();
    let csdl = temper_spec::parse_csdl(FEATURE_REQUEST_CSDL).expect("csdl parses");
    registry.register_tenant(
        "temper-system",
        csdl,
        FEATURE_REQUEST_CSDL.to_string(),
        &[("FeatureRequest", FEATURE_REQUEST_IOA)],
    );
    registry
}

async fn state_with_gap_trajectories() -> (ServerState, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_url = format!("file:{}", dir.path().join("arn240.db").display());
    let turso = temper_store_turso::TursoEventStore::new(&db_url, None)
        .await
        .expect("turso store");
    let stack = StorageStack::from_turso(turso);

    // Three failing Platform-source entries for the same (action, error
    // pattern) — exactly the FEATURE_REQUEST_THRESHOLD gap group.
    let sink = stack.trajectory.clone().expect("trajectory sink");
    for n in 0..3 {
        sink.persist_trajectory_entry(&failing_platform_entry(n))
            .await
            .expect("persist trajectory");
    }

    let system = ActorSystem::new("arn240-test");
    let mut state = ServerState::from_registry(system, registry_with_system_feature_request_spec());
    state.set_storage_stack(stack);
    (state, dir)
}

fn system_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-temper-principal-kind", "system".parse().expect("hdr"));
    headers
}

async fn get_feature_requests(state: &ServerState) -> serde_json::Value {
    let response = super::handle_feature_requests(
        State(state.clone()),
        system_headers(),
        Query(BTreeMap::new()),
    )
    .await
    .expect("GET feature-requests");
    response.0
}

/// A read must not create anything new on re-read: the same gap group must
/// map to the same feature request, however many times it is listed.
#[tokio::test]
async fn repeated_get_does_not_duplicate_feature_requests() {
    let (state, _dir) = state_with_gap_trajectories().await;

    let first = get_feature_requests(&state).await;
    assert_eq!(
        first["total"], 1,
        "one gap group must yield one feature request, got: {first}"
    );

    let second = get_feature_requests(&state).await;
    assert_eq!(
        second["total"], 1,
        "a GET is a read — re-reading must not create a duplicate feature \
         request for the same gap group, got: {second}"
    );
    assert_eq!(
        second["feature_requests"][0]["id"], first["feature_requests"][0]["id"],
        "the same gap group must keep the same identity across reads"
    );
}

/// Re-generation must not clobber developer-owned fields: a disposition set
/// via PATCH survives subsequent GETs while agents keep hitting the same gap.
#[tokio::test]
async fn get_preserves_developer_disposition_and_notes() {
    let (state, _dir) = state_with_gap_trajectories().await;

    let first = get_feature_requests(&state).await;
    let id = first["feature_requests"][0]["id"]
        .as_str()
        .expect("feature request id")
        .to_string();

    let store = state.platform_metadata_store().expect("platform store");
    store
        .update_feature_request(&id, "WontFix", Some("duplicate of FR-1"))
        .await
        .expect("developer updates disposition");

    let after = get_feature_requests(&state).await;
    assert_eq!(after["total"], 1, "still exactly one row, got: {after}");
    assert_eq!(
        after["feature_requests"][0]["disposition"], "WontFix",
        "a GET must not reset a developer's disposition, got: {after}"
    );
    assert_eq!(
        after["feature_requests"][0]["developer_notes"], "duplicate of FR-1",
        "a GET must not wipe developer notes, got: {after}"
    );
}

/// The system entity behind a feature request is created ONCE — the entity
/// journal for the record's deterministic id holds exactly one creation
/// event however many times the listing runs. (Previously every GET
/// dispatched a fresh `FR-{uuid}` entity per generated record.)
#[tokio::test]
async fn repeated_get_creates_the_system_entity_exactly_once() {
    let (state, _dir) = state_with_gap_trajectories().await;

    let first = get_feature_requests(&state).await;
    let id = first["feature_requests"][0]["id"]
        .as_str()
        .expect("feature request id")
        .to_string();
    get_feature_requests(&state).await;

    let events = state
        .storage_stack
        .as_ref()
        .expect("stack")
        .events
        .read_events(&format!("temper-system:FeatureRequest:{id}"), 0)
        .await
        .expect("read entity journal");
    assert_eq!(
        events.len(),
        1,
        "two GETs must leave exactly one creation event on the record's \
         entity journal, got {} (ids minted per read would journal under \
         fresh ids and leave this journal empty or duplicated)",
        events.len()
    );
}
