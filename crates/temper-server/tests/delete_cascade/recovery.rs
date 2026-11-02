//! A lifecycle state is not a tombstone, and a conflict must let the next
//! delete decision see another server's committed state.

use axum::http::StatusCode;
use serde_json::json;
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_server::storage::{BackendLabel, BoxedEventStore};
use temper_server::{ServerState, StorageStack};
use temper_store_sim::SimEventStore;

use super::{AgentContext, album_with_tracks, delete, field, get, server_on};

/// A lifecycle whose live terminal state shares the tombstone's status name.
pub(super) const TRACK_IOA: &str = r#"
[automaton]
name = "Track"
states = ["Active", "Deleted"]
initial = "Active"
allow_indefinite_states = ["Active", "Deleted"]

[[action]]
name = "Retire"
kind = "input"
from = ["Active"]
to = "Deleted"
"#;

fn server(name: &str, store: &SimEventStore) -> ServerState {
    server_on(
        name,
        StorageStack::new(
            BackendLabel::Sim,
            BoxedEventStore::new(store.clone()),
            None,
            None,
            None,
            None,
            None,
            None,
        ),
    )
}

fn tombstones(store: &SimEventStore, entity_type: &str, id: &str) -> usize {
    store
        .dump_journal(&format!("default:{entity_type}:{id}"))
        .iter()
        .filter(|event| event.is_tombstone())
        .count()
}

#[tokio::test]
async fn a_cascade_tombstones_a_live_row_in_a_state_named_deleted() {
    // Regression seed 0; explore the same lifecycle with fresh simulated ids.
    for seed in 0..16 {
        let _sim = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let state = server("cascade-lifecycle", &store);
        album_with_tracks(&state, "owner", &["retired", "active"]).await;
        let retired = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Track",
                "retired",
                "Retire",
                json!({}),
                &AgentContext::default(),
            )
            .await
            .expect("retire track");
        assert!(retired.success, "seed {seed}: {retired:?}");
        assert_eq!(retired.state.status, "Deleted");
        assert_eq!(tombstones(&store, "Track", "retired"), 0);

        let (status, body) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            delete(&state, "Albums", "owner"),
        )
        .await
        .expect("delete must finish after concurrent writes stop");
        assert_eq!(status, StatusCode::NO_CONTENT, "seed {seed}: {body}");
        for id in ["retired", "active"] {
            assert_eq!(tombstones(&store, "Track", id), 1, "seed {seed}: {id}");
            assert_eq!(get(&state, "Tracks", id).await, StatusCode::NOT_FOUND);
        }
    }
}

#[tokio::test]
async fn a_batch_conflict_replans_on_another_servers_moved_row() {
    for seed in 0..16 {
        let _sim = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let state = server("cascade-stale-member", &store);
        let other = server("cascade-other-member", &store);
        album_with_tracks(&state, "owner", &["moved", "kept"]).await;
        album_with_tracks(&state, "new-owner", &[]).await;
        other
            .update_tenant_entity_fields(
                &TenantId::default(),
                "Track",
                "moved",
                json!({ "album_id": "new-owner" }),
                false,
            )
            .await
            .expect("another server moves the track");
        assert_eq!(field(&state, "Track", "moved", "album_id").await, "owner");

        let (status, body) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            delete(&state, "Albums", "owner"),
        )
        .await
        .expect("delete must finish after concurrent writes stop");
        assert_eq!(status, StatusCode::NO_CONTENT, "seed {seed}: {body}");
        assert_eq!(tombstones(&store, "Album", "owner"), 1);
        assert_eq!(tombstones(&store, "Track", "kept"), 1);
        assert_eq!(tombstones(&store, "Track", "moved"), 0);
        assert_eq!(
            field(&state, "Track", "moved", "album_id").await,
            "new-owner"
        );
        assert_eq!(get(&state, "Tracks", "moved").await, StatusCode::OK);
    }
}

#[tokio::test]
async fn a_batch_conflict_reauthorizes_the_root_on_another_servers_state() {
    for seed in 0..16 {
        let _sim = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let state = server("cascade-stale-root", &store);
        let other = server("cascade-other-root", &store);
        album_with_tracks(&state, "owner", &["track"]).await;
        state
            .authz
            .reload_tenant_policies(
                "default",
                r#"permit(principal, action, resource);
                forbid(principal, action == Action::"delete", resource)
                when { resource has latest_track_id && resource.latest_track_id == "locked" };"#,
            )
            .expect("delete policy");
        other
            .update_tenant_entity_fields(
                &TenantId::default(),
                "Album",
                "owner",
                json!({ "latest_track_id": "locked" }),
                false,
            )
            .await
            .expect("another server changes the root's authorization attributes");
        assert_eq!(
            field(&state, "Album", "owner", "latest_track_id").await,
            "track"
        );

        let (status, body) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            delete(&state, "Albums", "owner"),
        )
        .await
        .expect("delete must finish after concurrent writes stop");
        assert_eq!(status, StatusCode::FORBIDDEN, "seed {seed}: {body}");
        assert_eq!(tombstones(&store, "Album", "owner"), 0);
        assert_eq!(tombstones(&store, "Track", "track"), 0);
        assert_eq!(
            field(&state, "Album", "owner", "latest_track_id").await,
            "locked"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_failed_conflict_reload_keeps_every_child_discoverable_for_retry() {
    // Regression seed 0: the first child fails replay; later children must
    // remain discoverable both during recovery and after it returns an error.
    for seed in 0..16 {
        let _sim = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let state = server("cascade-failed-reload", &store);
        let tracks = ["a-first", "b-middle", "c-last"];
        album_with_tracks(&state, "owner", &tracks).await;
        let tenant = TenantId::default();
        assert_eq!(state.list_entity_ids_lazy(&tenant, "Track").await, tracks);

        // Planning reads resident actors. Reject the batch, then fail the
        // first child's replay and its three supervised restarts while
        // subsequent children are still evicted.
        let other = server("cascade-failed-reload-other", &store);
        other
            .update_tenant_entity_fields(
                &tenant,
                "Album",
                "owner",
                json!({ "latest_track_id": "a-first" }),
                false,
            )
            .await
            .expect("another server changes the root after its actor was loaded");
        store.fail_next_reads("default:Track:a-first", 4);
        let observe_recovery = async {
            loop {
                let last_is_resident = state
                    .actor_registry
                    .read()
                    .expect("actor registry")
                    .contains_key("default:Track:c-last");
                if !last_is_resident {
                    return state.list_entity_ids_lazy(&tenant, "Track").await;
                }
                tokio::task::yield_now().await;
            }
        };
        let ((status, body), during_recovery) =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(delete(&state, "Albums", "owner"), observe_recovery)
            })
            .await
            .expect("recovery must report the read failure");
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "seed {seed}: {body}"
        );
        assert!(body.to_string().contains("delete conflict recovery failed"));
        for id in tracks {
            assert_eq!(tombstones(&store, "Track", id), 0, "seed {seed}: {id}");
        }
        assert_eq!(tombstones(&store, "Album", "owner"), 0);

        let after_recovery = state.list_entity_ids_lazy(&tenant, "Track").await;

        // Restore storage and retry through OData, without reading children
        // individually (which could repopulate an incomplete index).
        store.fail_next_reads("default:Track:a-first", 0);
        let (status, body) = delete(&state, "Albums", "owner").await;
        assert_eq!(status, StatusCode::NO_CONTENT, "seed {seed}: {body}");
        assert_eq!(tombstones(&store, "Album", "owner"), 1);
        for id in tracks {
            assert_eq!(tombstones(&store, "Track", id), 1, "seed {seed}: {id}");
            assert_eq!(get(&state, "Tracks", id).await, StatusCode::NOT_FOUND);
        }
        assert_eq!(during_recovery, tracks, "seed {seed}: concurrent discovery");
        assert_eq!(
            after_recovery, tracks,
            "seed {seed}: discovery after failure"
        );
    }
}
