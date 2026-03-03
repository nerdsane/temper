use std::sync::Arc;

use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_store_turso::TursoEventStore;

use temper_server::event_store::ServerEventStore;
use temper_server::registry::SpecRegistry;
use temper_server::state::ServerState;

#[tokio::test]
async fn ensure_entity_loaded_returns_false_when_no_transition_table_exists() {
    let db_path =
        std::env::temp_dir().join(format!("temper-ensure-loaded-{}.db", uuid::Uuid::new_v4()));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let pid = "tenant-a:Order:ord-1";
    let envelope = PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "Created".to_string(),
        payload: serde_json::json!({"id": "ord-1"}),
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: sim_now(),
            actor_id: pid.to_string(),
        },
    };
    store
        .append(pid, 0, &[envelope])
        .await
        .expect("append seed event");

    let mut state =
        ServerState::from_registry(ActorSystem::new("test-ensure-loaded"), SpecRegistry::new());
    state.event_store = Some(Arc::new(ServerEventStore::Turso(store)));

    let loaded = state
        .ensure_entity_loaded(&TenantId::new("tenant-a"), "Order", "ord-1")
        .await;
    assert!(
        !loaded,
        "entity should not be considered loaded when transition table is missing"
    );

    let _ = std::fs::remove_file(db_path);
}
