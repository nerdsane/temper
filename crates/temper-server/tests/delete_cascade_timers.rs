//! A delete that cascades leaves no armed timer behind for the rows it
//! removes, as deleting one record does: a timer row left behind would arm
//! again at the next boot for a record that no longer exists.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Test.Timers" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Album">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Track">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="album_id" Type="Edm.String" Nullable="false"/>
        <NavigationProperty Name="Album" Type="Test.Timers.Album" Nullable="false">
          <ReferentialConstraint Property="album_id" ReferencedProperty="Id"/>
          <Annotation Term="Temper.Vocab.Relation.DeletePolicy" String="cascade"/>
        </NavigationProperty>
      </EntityType>
      <EntityContainer Name="TimersService">
        <EntitySet Name="Albums" EntityType="Test.Timers.Album"/>
        <EntitySet Name="Tracks" EntityType="Test.Timers.Track"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const ALBUM_IOA: &str = r#"
[automaton]
name = "Album"
states = ["Active"]
initial = "Active"
"#;

/// A track is created straight into a timed state, so its timer is armed
/// when it is created. The timer is long enough never to fire in a test.
const TRACK_IOA: &str = r#"
[automaton]
name = "Track"
states = ["Active", "Expired"]
initial = "Active"

[[action]]
name = "Expire"
kind = "input"
from = ["Active"]
to = "Expired"

[[state_timeout]]
state = "Active"
after_seconds = 3600
on_timeout = "Expire"
"#;

async fn server(name: &str, db: &std::path::Path) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).expect("CSDL parse"),
        CSDL_XML.to_string(),
        &[("Album", ALBUM_IOA), ("Track", TRACK_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    let store = TursoEventStore::new(&format!("file:{}", db.display()), None)
        .await
        .expect("open local turso db");
    state.set_storage_stack(StorageStack::from_turso(store));
    {
        let mut registry = state.registry.write().unwrap();
        for entity_type in ["Album", "Track"] {
            registry.set_verification_status(
                &TenantId::default(),
                entity_type,
                VerificationStatus::Completed(EntityVerificationResult {
                    all_passed: true,
                    levels: vec![EntityLevelSummary {
                        level: "L0 SMT".to_string(),
                        passed: true,
                        summary: "OK".to_string(),
                        details: None,
                    }],
                    verified_at: "2026-10-24T00:00:00Z".to_string(),
                }),
            );
        }
    }
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("test policy parses");
    state.start_runtime().await;
    state
}

async fn send(state: &ServerState, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let mut request = request;
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(
                "delete-cascade-timers-test",
                "test-agent",
                None,
            ),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

async fn post(state: &ServerState, set: &str, body: serde_json::Value) {
    let request = Request::post(format!("/tdata/{set}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let (status, response) = send(state, request).await;
    assert_eq!(status, StatusCode::CREATED, "create in {set}: {response}");
}

/// The ids of the Tracks with an armed timer row.
async fn timed_tracks(state: &ServerState) -> Vec<String> {
    let store = state
        .metadata_store_for_tenant("default")
        .await
        .expect("metadata store");
    let mut ids: Vec<String> = store
        .list_state_timers("default")
        .await
        .expect("list timer rows")
        .into_iter()
        .filter(|row| row.entity_type == "Track")
        .map(|row| row.entity_id)
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn a_cascade_leaves_no_timer_for_the_rows_it_deletes() {
    let directory = tempfile::tempdir().expect("create test directory");
    let state = server("delete-cascade-timers", &directory.path().join("timers.db")).await;
    for album in ["a-owner", "a-other"] {
        post(&state, "Albums", serde_json::json!({ "Id": album })).await;
    }
    for (track, album) in [
        ("t-1", "a-owner"),
        ("t-2", "a-owner"),
        ("t-3", "a-owner"),
        ("t-other", "a-other"),
    ] {
        post(
            &state,
            "Tracks",
            serde_json::json!({ "Id": track, "album_id": album }),
        )
        .await;
    }
    assert_eq!(
        timed_tracks(&state).await,
        ["t-1", "t-2", "t-3", "t-other"],
        "each Track is armed when it is created"
    );

    let (status, body) = send(
        &state,
        Request::delete("/tdata/Albums('a-owner')")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "owner delete: {body}");

    assert_eq!(
        timed_tracks(&state).await,
        ["t-other"],
        "the deleted Tracks keep no timer; the other album's Track keeps its own"
    );
}
