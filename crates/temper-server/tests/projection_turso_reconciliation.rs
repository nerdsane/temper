//! Turso-specific upgrade regressions for projection authority and stale replay.

use temper_runtime::ActorSystem;
use temper_runtime::persistence::{
    EntityVectorRow, EventMetadata, EventStore, PersistenceEnvelope,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_store_turso::TursoEventStore;

#[path = "support/projection_legacy.rs"]
mod projection_legacy_support;
use projection_legacy_support::build_registry;

fn build_state(store: TursoEventStore, actor_system_name: &str) -> ServerState {
    let mut state =
        ServerState::from_registry(ActorSystem::new(actor_system_name), build_registry());
    state.set_storage_stack(StorageStack::from_turso(store));
    state
}

async fn seed_deleted_item(state: &ServerState) {
    let tenant = TenantId::default();
    let context = AgentContext::for_service("legacy-turso-projection-test");
    let create = state
        .dispatch_tenant_action(
            &tenant,
            "Item",
            "dead",
            "Create",
            serde_json::json!({
                "Slug": "dead-slug",
                "Embedding": "[1,0,0,0]",
                "EmbeddingModel": "m1"
            }),
            &context,
        )
        .await
        .expect("create dispatch");
    assert!(create.success, "create failed: {:?}", create.error);
    let delete = state
        .dispatch_tenant_action(
            &tenant,
            "Item",
            "dead",
            "Delete",
            serde_json::json!({}),
            &context,
        )
        .await
        .expect("delete dispatch");
    assert!(delete.success, "delete failed: {:?}", delete.error);
}

fn duplicate_tombstone() -> PersistenceEnvelope {
    let timestamp = sim_now();
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Deleted".to_string(),
        payload: serde_json::json!({
            "action": "Deleted",
            "from_status": "Deleted",
            "to_status": "Deleted",
            "timestamp": timestamp,
            "params": {},
            "idempotency_key": null
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp,
            actor_id: "legacy-turso-projection-test".to_string(),
        },
    }
}

async fn local_store(name: &str) -> (tempfile::TempDir, TursoEventStore) {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_url = format!("file:{}", dir.path().join(format!("{name}.db")).display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("open local Turso store");
    (dir, store)
}

#[tokio::test]
async fn non_authoritative_turso_keys_never_cache_backfill_coverage() {
    let (_dir, store) = local_store("key-authority").await;
    assert!(!store.has_authoritative_key_index());
    let state = build_state(store.clone(), "legacy-turso-key-authority");

    state
        .populate_key_index_from_snapshots(&TenantId::default())
        .await;

    assert!(
        state.key_index_backfilled.read().unwrap().is_empty(),
        "an unsupported key backend must never become authoritative in cache"
    );
    assert!(
        store
            .mark_key_index_backfilled("default", "Item", "v2|slug")
            .await
            .is_err(),
        "unsupported durable key authority must fail closed"
    );
}

#[tokio::test]
async fn duplicate_legacy_tombstone_purges_turso_vector_at_the_durable_head() {
    let (_dir, store) = local_store("duplicate-tombstone").await;
    let state = build_state(store.clone(), "legacy-turso-duplicate-tombstone");
    seed_deleted_item(&state).await;
    let tombstone_head = store
        .read_events("default:Item:dead", 0)
        .await
        .expect("read first tombstone head")
        .last()
        .expect("deleted item has durable history")
        .sequence_nr;

    store
        .backfill_entity_vectors(
            "default",
            "Item",
            "dead",
            tombstone_head,
            &[EntityVectorRow {
                decl_name: "embed".to_string(),
                model_tag: "m1".to_string(),
                vector: vec![1.0, 0.0, 0.0, 0.0],
            }],
        )
        .await
        .expect("seed stale vector at first tombstone");
    store
        .append(
            "default:Item:dead",
            tombstone_head,
            &[duplicate_tombstone()],
        )
        .await
        .expect("append legacy duplicate tombstone");
    assert_eq!(
        store
            .vector_candidates("default", "Item", "embed", "m1", 10)
            .await
            .expect("stale vector precondition")
            .len(),
        1
    );

    let restarted = build_state(store.clone(), "legacy-turso-duplicate-restart");
    let recovered = restarted
        .get_tenant_entity_state(&TenantId::default(), "Item", "dead")
        .await
        .expect("recover duplicate tombstone journal");
    assert_eq!(recovered.state.status, "Deleted");
    assert_eq!(recovered.state.sequence_nr, tombstone_head + 1);
    restarted
        .populate_vector_index_from_snapshots(&TenantId::default())
        .await;

    assert!(
        store
            .vector_candidates("default", "Item", "embed", "m1", 10)
            .await
            .expect("vector candidates after restart repair")
            .is_empty(),
        "repair at the durable duplicate-tombstone head must purge the stale vector"
    );
}
