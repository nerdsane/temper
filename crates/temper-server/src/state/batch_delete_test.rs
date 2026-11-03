//! A batch delete removes every member, or none of them when one changed
//! after it was read or the server keeps no journal.

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;

use super::BatchDeleteError;
use crate::entity_actor::EntityState;
use crate::registry::SpecRegistry;
use crate::state::ServerState;
use crate::storage::StorageStack;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Test.Batch" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Track">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="album_id" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="BatchService">
        <EntitySet Name="Tracks" EntityType="Test.Batch.Track"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const TRACK_IOA: &str = r#"
[automaton]
name = "Track"
states = ["Active"]
initial = "Active"
"#;

struct Fixture {
    state: ServerState,
    _directory: tempfile::TempDir,
}

/// A server that keeps each entity only in its actor: no event journal.
fn memory_only(name: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).expect("CSDL parses"),
        CSDL.to_string(),
        &[("Track", TRACK_IOA)],
    );
    ServerState::from_registry(ActorSystem::new(name), registry)
}

async fn fixture(name: &str) -> Fixture {
    let mut state = memory_only(name);
    let directory = tempfile::tempdir().expect("create batch delete test directory");
    let database_url = format!("file:{}", directory.path().join("batch.db").display());
    let store = TursoEventStore::new(&database_url, None)
        .await
        .expect("open local turso db");
    state.set_storage_stack(StorageStack::from_turso(store));
    Fixture {
        state,
        _directory: directory,
    }
}

async fn create_tracks(state: &ServerState, ids: &[&str], album: &str) {
    for id in ids {
        state
            .get_or_create_tenant_entity(
                &TenantId::default(),
                "Track",
                id,
                serde_json::json!({ "album_id": album }),
            )
            .await
            .unwrap_or_else(|error| panic!("create Track('{id}'): {error}"));
    }
}

async fn read(state: &ServerState, id: &str) -> EntityState {
    state
        .get_tenant_entity_state(&TenantId::default(), "Track", id)
        .await
        .unwrap_or_else(|error| panic!("read Track('{id}'): {error}"))
        .state
}

async fn move_track(state: &ServerState, id: &str, album: &str) {
    state
        .update_tenant_entity_fields(
            &TenantId::default(),
            "Track",
            id,
            serde_json::json!({ "album_id": album }),
            false,
        )
        .await
        .unwrap_or_else(|error| panic!("move Track('{id}'): {error}"));
}

/// Whether the journal ends in the kernel's tombstone.
async fn journal_deleted(state: &ServerState, id: &str) -> bool {
    let (store, _) = state.event_journal().expect("journal");
    let events = store
        .read_events(&format!("default:Track:{id}"), 0)
        .await
        .expect("read journal");
    events
        .last()
        .is_some_and(temper_runtime::persistence::PersistenceEnvelope::is_tombstone)
}

#[tokio::test]
async fn members_read_at_their_journal_head_are_removed_together() {
    let fixture = fixture("batch-delete-together").await;
    let state = &fixture.state;
    let ids = ["t-1", "t-2", "t-3"];
    create_tracks(state, &ids, "a-1").await;
    let mut members = Vec::new();
    for id in ids {
        members.push(read(state, id).await);
    }

    state
        .delete_entities_together(&TenantId::default(), members)
        .await
        .expect("batch delete");

    for id in ids {
        assert!(journal_deleted(state, id).await, "{id} has its tombstone");
        assert!(
            !state
                .ensure_entity_loaded(&TenantId::default(), "Track", id)
                .await,
            "{id} reads as deleted"
        );
    }
}

#[tokio::test]
async fn a_member_changed_after_it_was_read_leaves_every_member_in_place() {
    let fixture = fixture("batch-delete-changed-member").await;
    let state = &fixture.state;
    let ids = ["t-1", "t-2", "t-3"];
    create_tracks(state, &ids, "a-1").await;
    let mut members = Vec::new();
    for id in ids {
        members.push(read(state, id).await);
    }
    // t-2 moves to another album after the delete read it.
    move_track(state, "t-2", "a-2").await;

    let refused = state
        .delete_entities_together(&TenantId::default(), members)
        .await;
    assert!(
        matches!(refused, Err(BatchDeleteError::Changed(_))),
        "a member that changed refuses the batch: {refused:?}"
    );
    for id in ids {
        assert!(!journal_deleted(state, id).await, "{id} has no tombstone");
        assert!(
            state
                .ensure_entity_loaded(&TenantId::default(), "Track", id)
                .await,
            "{id} is still live"
        );
    }
    assert_eq!(read(state, "t-2").await.fields["album_id"], "a-2");
    assert_eq!(read(state, "t-1").await.fields["album_id"], "a-1");
}

#[tokio::test]
async fn a_requested_entity_changed_after_it_was_authorized_leaves_its_dependents() {
    let fixture = fixture("batch-delete-changed-root").await;
    let state = &fixture.state;
    create_tracks(state, &["t-root", "t-1", "t-2"], "a-1").await;
    // The requested entity, as its delete was authorized, and its dependents.
    let mut members = vec![read(state, "t-root").await];
    for id in ["t-1", "t-2"] {
        members.push(read(state, id).await);
    }
    move_track(state, "t-root", "a-2").await;

    let refused = state
        .delete_entities_together(&TenantId::default(), members)
        .await;
    assert!(
        matches!(refused, Err(BatchDeleteError::Changed(_))),
        "a changed root refuses the batch: {refused:?}"
    );
    for id in ["t-root", "t-1", "t-2"] {
        assert!(!journal_deleted(state, id).await, "{id} has no tombstone");
    }
}

#[tokio::test]
async fn without_a_journal_a_batch_is_refused_and_every_member_stays() {
    let state = &memory_only("batch-delete-memory");
    let ids = ["t-1", "t-2", "t-3"];
    create_tracks(state, &ids, "a-1").await;
    let mut members = Vec::new();
    for id in ids {
        members.push(read(state, id).await);
    }

    let refused = state
        .delete_entities_together(&TenantId::default(), members)
        .await;
    assert_eq!(refused, Err(BatchDeleteError::NoJournal));
    for id in ids {
        let track = read(state, id).await;
        assert_eq!(track.status, "Active", "{id} was not deleted");
        assert_eq!(track.fields["album_id"], "a-1");
    }
}
