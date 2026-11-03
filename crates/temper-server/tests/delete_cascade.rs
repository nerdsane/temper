//! A relation's declared delete policy decides what deleting its target does.
//!
//! The model mirrors a parent that owns its children and also points at one of
//! them: `Track.album_id -> Album` is `cascade` (an album owns its tracks),
//! `Album.latest_track_id -> Track` is `none` (a pointer, not ownership), and
//! `PlaylistEntry.track_id -> Track` keeps the default `restrict`.
//! `Folder.parent_id -> Folder` is `cascade` within one type, so folders can
//! own each other in a cycle, and `Credit.album_id -> Album` is `cascade` into
//! a strict type, which only a declared action deletes.

mod common;
#[path = "delete_cascade/recovery.rs"]
mod recovery;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Test.Music" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Album">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="latest_track_id" Type="Edm.String"/>
        <NavigationProperty Name="LatestTrack" Type="Test.Music.Track">
          <ReferentialConstraint Property="latest_track_id" ReferencedProperty="Id"/>
          <Annotation Term="Temper.Vocab.Relation.DeletePolicy" String="none"/>
        </NavigationProperty>
      </EntityType>
      <!-- Declared before Track, so an album's credits are found before its
           tracks. -->
      <EntityType Name="Credit">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="album_id" Type="Edm.String"/>
        <NavigationProperty Name="Album" Type="Test.Music.Album">
          <ReferentialConstraint Property="album_id" ReferencedProperty="Id"/>
          <Annotation Term="Temper.Vocab.Relation.DeletePolicy" String="cascade"/>
        </NavigationProperty>
      </EntityType>
      <EntityType Name="Track">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="album_id" Type="Edm.String" Nullable="false"/>
        <NavigationProperty Name="Album" Type="Test.Music.Album">
          <ReferentialConstraint Property="album_id" ReferencedProperty="Id"/>
          <Annotation Term="Temper.Vocab.Relation.DeletePolicy" String="cascade"/>
        </NavigationProperty>
      </EntityType>
      <EntityType Name="PlaylistEntry">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="track_id" Type="Edm.String" Nullable="false"/>
        <NavigationProperty Name="Track" Type="Test.Music.Track">
          <ReferentialConstraint Property="track_id" ReferencedProperty="Id"/>
        </NavigationProperty>
      </EntityType>
      <EntityType Name="Folder">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="parent_id" Type="Edm.String"/>
        <NavigationProperty Name="Parent" Type="Test.Music.Folder">
          <ReferentialConstraint Property="parent_id" ReferencedProperty="Id"/>
          <Annotation Term="Temper.Vocab.Relation.DeletePolicy" String="cascade"/>
        </NavigationProperty>
      </EntityType>
      <EntityContainer Name="MusicService">
        <EntitySet Name="Albums" EntityType="Test.Music.Album"/>
        <EntitySet Name="Tracks" EntityType="Test.Music.Track"/>
        <EntitySet Name="PlaylistEntries" EntityType="Test.Music.PlaylistEntry"/>
        <EntitySet Name="Folders" EntityType="Test.Music.Folder"/>
        <EntitySet Name="Credits" EntityType="Test.Music.Credit"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// Each type only needs to exist and keep the fields it was created with.
fn ioa(name: &str) -> String {
    format!("[automaton]\nname = \"{name}\"\nstates = [\"Active\"]\ninitial = \"Active\"\n")
}

/// A strict type: its album is set by a declared action, and only a declared
/// action may delete it.
const CREDIT_IOA: &str = r#"
[automaton]
name = "Credit"
states = ["Draft", "Attached"]
initial = "Draft"
strict_action_params = true

[[action]]
name = "Attach"
kind = "input"
from = ["Draft"]
to = "Attached"
params = ["album_id"]
"#;

const ENTITY_TYPES: [&str; 5] = ["Album", "Track", "PlaylistEntry", "Folder", "Credit"];

struct Store {
    db_path: std::path::PathBuf,
    db_url: String,
}

impl Store {
    fn new(name: &str) -> Self {
        let db_path = std::env::temp_dir().join(format!(
            "temper-delete-cascade-{name}-{}.db",
            uuid::Uuid::new_v4()
        ));
        let db_url = format!("file:{}", db_path.display());
        Self { db_path, db_url }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.db_path);
    }
}

async fn open(store: &Store) -> TursoEventStore {
    TursoEventStore::new(&store.db_url, None)
        .await
        .expect("open local turso db")
}

async fn server(name: &str, store: &Store) -> ServerState {
    server_on(name, StorageStack::from_turso(open(store).await))
}

fn server_on(name: &str, stack: StorageStack) -> ServerState {
    let mut state = memory_only(name);
    state.set_storage_stack(stack);
    state
}

/// A server that keeps each entity only in its actor: no event journal.
fn memory_only(name: &str) -> ServerState {
    let (album, track, entry, folder) = (
        ioa("Album"),
        recovery::TRACK_IOA.to_string(),
        ioa("PlaylistEntry"),
        ioa("Folder"),
    );
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).expect("CSDL parse"),
        CSDL_XML.to_string(),
        &[
            ("Album", album.as_str()),
            ("Track", track.as_str()),
            ("PlaylistEntry", entry.as_str()),
            ("Folder", folder.as_str()),
            ("Credit", CREDIT_IOA),
        ],
    );
    let state = ServerState::from_registry(ActorSystem::new(name), registry);
    {
        let mut registry = state.registry.write().unwrap();
        for entity_type in ENTITY_TYPES {
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
    state
}

async fn create(state: &ServerState, entity_type: &str, id: &str, fields: serde_json::Value) {
    state
        .get_or_create_tenant_entity(&TenantId::default(), entity_type, id, fields)
        .await
        .unwrap_or_else(|error| panic!("create {entity_type}('{id}'): {error}"));
}

async fn send(state: &ServerState, request: Request<Body>) -> (StatusCode, serde_json::Value) {
    let mut request = request;
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(
                "delete-cascade-test",
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

async fn delete(state: &ServerState, set: &str, id: &str) -> (StatusCode, serde_json::Value) {
    let path = format!("/tdata/{set}('{id}')");
    send(state, Request::delete(path).body(Body::empty()).unwrap()).await
}

async fn get(state: &ServerState, set: &str, id: &str) -> StatusCode {
    let path = format!("/tdata/{set}('{id}')");
    send(state, Request::get(path).body(Body::empty()).unwrap())
        .await
        .0
}

async fn field(state: &ServerState, entity_type: &str, id: &str, field: &str) -> serde_json::Value {
    state
        .get_tenant_entity_state(&TenantId::default(), entity_type, id)
        .await
        .expect("read row")
        .state
        .fields[field]
        .clone()
}

async fn post(
    state: &ServerState,
    set: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let request = Request::post(format!("/tdata/{set}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(state, request).await
}

/// An album that owns `tracks` and points at the last of them.
async fn album_with_tracks(state: &ServerState, album: &str, tracks: &[&str]) {
    let latest = tracks.last().copied().unwrap_or_default();
    create(
        state,
        "Album",
        album,
        serde_json::json!({ "latest_track_id": latest }),
    )
    .await;
    for track in tracks {
        create(
            state,
            "Track",
            track,
            serde_json::json!({ "album_id": album }),
        )
        .await;
    }
}

#[tokio::test]
async fn deleting_an_owner_deletes_what_it_owns() {
    let store = Store::new("owner");
    let state = server("delete-cascade-owner", &store).await;
    let owned = ["t-1", "t-2", "t-3", "t-4", "t-5"];
    album_with_tracks(&state, "a-owner", &owned).await;
    album_with_tracks(&state, "a-other", &["t-other"]).await;

    let (status, body) = delete(&state, "Albums", "a-owner").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "owner delete: {body}");

    assert_eq!(
        get(&state, "Albums", "a-owner").await,
        StatusCode::NOT_FOUND
    );
    for track in owned {
        assert_eq!(
            get(&state, "Tracks", track).await,
            StatusCode::NOT_FOUND,
            "{track} outlived its album"
        );
    }
    assert_eq!(get(&state, "Albums", "a-other").await, StatusCode::OK);
    assert_eq!(get(&state, "Tracks", "t-other").await, StatusCode::OK);
}

#[tokio::test]
async fn a_restrict_reference_into_what_would_be_deleted_refuses_all_of_it() {
    let store = Store::new("restricted");
    let state = server("delete-cascade-restricted", &store).await;
    album_with_tracks(&state, "a-kept", &["t-kept-1", "t-kept-2"]).await;
    create(
        &state,
        "PlaylistEntry",
        "e-1",
        serde_json::json!({ "track_id": "t-kept-2" }),
    )
    .await;

    let (status, body) = delete(&state, "Albums", "a-kept").await;
    assert_eq!(status, StatusCode::CONFLICT, "restricted delete: {body}");
    assert_eq!(body["error"]["details"]["type"], "relation_integrity");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("Track('t-kept-2')") && message.contains("PlaylistEntry('e-1')"),
        "the refusal names the owned row and what references it: {message}"
    );

    assert_eq!(get(&state, "Albums", "a-kept").await, StatusCode::OK);
    assert_eq!(get(&state, "Tracks", "t-kept-1").await, StatusCode::OK);
    assert_eq!(get(&state, "Tracks", "t-kept-2").await, StatusCode::OK);
}

#[tokio::test]
async fn a_none_relation_does_not_block_deleting_its_target() {
    let store = Store::new("pointer");
    let state = server("delete-cascade-pointer", &store).await;
    album_with_tracks(&state, "a-owning", &["t-pointed"]).await;
    create(
        &state,
        "Album",
        "a-pointer",
        serde_json::json!({ "latest_track_id": "t-pointed" }),
    )
    .await;

    let (status, body) = delete(&state, "Tracks", "t-pointed").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "pointed-at delete: {body}");
    assert_eq!(
        get(&state, "Tracks", "t-pointed").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(get(&state, "Albums", "a-pointer").await, StatusCode::OK);
    assert_eq!(get(&state, "Albums", "a-owning").await, StatusCode::OK);
}

#[tokio::test]
async fn an_owned_row_cannot_be_created_for_a_missing_owner() {
    let store = Store::new("create");
    let state = server("delete-cascade-create", &store).await;
    album_with_tracks(&state, "a-present", &[]).await;

    let (status, body) = post(
        &state,
        "Tracks",
        serde_json::json!({ "Id": "t-orphan", "album_id": "a-missing" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "orphan create: {body}");
    assert_eq!(body["error"]["details"]["type"], "relation_integrity");
    assert_eq!(
        get(&state, "Tracks", "t-orphan").await,
        StatusCode::NOT_FOUND
    );

    let (status, body) = post(
        &state,
        "Tracks",
        serde_json::json!({ "Id": "t-owned", "album_id": "a-present" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "owned create: {body}");
}

#[tokio::test]
async fn folders_that_own_each_other_are_deleted_once_each() {
    let store = Store::new("cycle");
    let state = server("delete-cascade-cycle", &store).await;
    // f-a and f-b own each other; f-c sits in f-b; f-self owns itself.
    for (folder, parent) in [
        ("f-a", "f-b"),
        ("f-b", "f-a"),
        ("f-c", "f-b"),
        ("f-self", "f-self"),
        ("f-kept", "f-elsewhere"),
    ] {
        create(
            &state,
            "Folder",
            folder,
            serde_json::json!({ "parent_id": parent }),
        )
        .await;
    }

    let (status, body) = delete(&state, "Folders", "f-a").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "cycle delete: {body}");
    for folder in ["f-a", "f-b", "f-c"] {
        assert_eq!(
            get(&state, "Folders", folder).await,
            StatusCode::NOT_FOUND,
            "{folder} outlived the cycle it belongs to"
        );
    }

    let (status, body) = delete(&state, "Folders", "f-self").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "self-owned delete: {body}");
    assert_eq!(
        get(&state, "Folders", "f-self").await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(get(&state, "Folders", "f-kept").await, StatusCode::OK);
}

#[tokio::test]
async fn a_cascade_into_a_strict_type_is_refused_before_anything_is_deleted() {
    let store = Store::new("strict");
    let state = server("delete-cascade-strict", &store).await;
    album_with_tracks(&state, "a-credited", &["t-1", "t-2"]).await;
    create(&state, "Credit", "c-1", serde_json::json!({})).await;
    state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Credit",
            "c-1",
            "Attach",
            serde_json::json!({ "album_id": "a-credited" }),
            &AgentContext::default(),
        )
        .await
        .expect("attach the credit to its album");
    assert_eq!(
        field(&state, "Credit", "c-1", "album_id").await,
        "a-credited"
    );

    let (status, body) = delete(&state, "Albums", "a-credited").await;
    assert_eq!(status, StatusCode::CONFLICT, "strict cascade: {body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("Credit('c-1')"),
        "the refusal names the strict row: {message}"
    );
    assert_eq!(get(&state, "Albums", "a-credited").await, StatusCode::OK);
    for track in ["t-1", "t-2"] {
        assert_eq!(
            get(&state, "Tracks", track).await,
            StatusCode::OK,
            "{track} was deleted by a refused delete"
        );
    }
    assert_eq!(get(&state, "Credits", "c-1").await, StatusCode::OK);
}

#[tokio::test]
async fn without_a_journal_a_delete_that_cascades_is_refused_and_one_that_does_not_runs() {
    let state = memory_only("delete-cascade-memory-only");
    album_with_tracks(&state, "a-owner", &["t-1", "t-2"]).await;
    create(&state, "Album", "a-empty", serde_json::json!({})).await;

    let (status, body) = delete(&state, "Albums", "a-owner").await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "cascade: {body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("keeps no journal") && message.contains("nothing was deleted"),
        "the refusal says why and that nothing was deleted: {message}"
    );
    assert_eq!(get(&state, "Albums", "a-owner").await, StatusCode::OK);
    for track in ["t-1", "t-2"] {
        assert_eq!(get(&state, "Tracks", track).await, StatusCode::OK);
    }

    let (status, body) = delete(&state, "Albums", "a-empty").await;
    assert_eq!(status, StatusCode::NO_CONTENT, "no cascade: {body}");
    assert_eq!(
        get(&state, "Albums", "a-empty").await,
        StatusCode::NOT_FOUND
    );
}
