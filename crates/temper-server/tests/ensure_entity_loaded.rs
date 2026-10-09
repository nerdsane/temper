use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_store_sim::SimEventStore;
use temper_store_turso::TursoEventStore;

use temper_server::registry::SpecRegistry;
use temper_server::state::ServerState;
use temper_server::storage::StorageStack;
use temper_spec::csdl::parse_csdl;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

fn build_state_with_sim(system_name: &str, store: SimEventStore) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "tenant-a",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );

    let mut state = ServerState::from_registry(ActorSystem::new(system_name), registry);
    state.set_storage_stack(StorageStack::from_sim(store, None));
    state
}

fn build_state_with_turso(system_name: &str, store: TursoEventStore) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "tenant-a",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );

    let mut state = ServerState::from_registry(ActorSystem::new(system_name), registry);
    state.set_storage_stack(StorageStack::from_turso(store));
    state
}

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
    state.set_storage_stack(StorageStack::from_turso(store));

    let loaded = state
        .ensure_entity_loaded(&TenantId::new("tenant-a"), "Order", "ord-1")
        .await;
    assert!(
        !loaded,
        "entity should not be considered loaded when transition table is missing"
    );

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn ensure_entity_loaded_returns_true_for_indexed_entity_without_persistence() {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "tenant-a",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    let state = ServerState::from_registry(ActorSystem::new("test-ensure-loaded-inmem"), registry);

    let tenant = TenantId::new("tenant-a");
    let entity_type = "Order";
    let entity_id = "ord-memory";

    state
        .get_or_create_tenant_entity(
            &tenant,
            entity_type,
            entity_id,
            serde_json::json!({"Title": "in-memory"}),
        )
        .await
        .expect("create in-memory entity");

    let loaded = state
        .ensure_entity_loaded(&tenant, entity_type, entity_id)
        .await;
    assert!(
        loaded,
        "indexed in-memory entity should be considered loaded"
    );
}

#[tokio::test]
async fn list_entity_ids_lazy_populates_only_requested_type() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-lazy-list-scoped-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    store
        .append(
            "tenant-a:Order:ord-1",
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "Created".to_string(),
                payload: serde_json::json!({"id": "ord-1"}),
                metadata: EventMetadata {
                    event_id: uuid::Uuid::new_v4(),
                    causation_id: uuid::Uuid::new_v4(),
                    correlation_id: uuid::Uuid::new_v4(),
                    timestamp: sim_now(),
                    actor_id: "tenant-a:Order:ord-1".to_string(),
                },
            }],
        )
        .await
        .expect("append order event");
    store
        .append(
            "tenant-a:Task:task-1",
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "Created".to_string(),
                payload: serde_json::json!({"id": "task-1"}),
                metadata: EventMetadata {
                    event_id: uuid::Uuid::new_v4(),
                    causation_id: uuid::Uuid::new_v4(),
                    correlation_id: uuid::Uuid::new_v4(),
                    timestamp: sim_now(),
                    actor_id: "tenant-a:Task:task-1".to_string(),
                },
            }],
        )
        .await
        .expect("append task event");

    let state = build_state_with_turso("test-lazy-list-scoped", store);
    let tenant = TenantId::new("tenant-a");

    let order_ids = state.list_entity_ids_lazy(&tenant, "Order").await;

    assert_eq!(order_ids, vec!["ord-1".to_string()]);
    assert!(
        state.list_entity_ids(&tenant, "Task").is_empty(),
        "lazy listing one entity type should not populate unrelated types"
    );

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn delete_writes_tombstone_and_deleted_entity_stays_out_of_list_after_restart() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-delete-tombstone-list-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let tenant = TenantId::new("tenant-a");
    let entity_type = "Order";
    let entity_id = "ord-delete-list";
    let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");

    let state = build_state_with_turso("test-delete-tombstone-list-1", store.clone());
    state
        .get_or_create_tenant_entity(
            &tenant,
            entity_type,
            entity_id,
            serde_json::json!({"Title": "to-delete"}),
        )
        .await
        .expect("create entity");
    state
        .delete_tenant_entity(&tenant, entity_type, entity_id)
        .await
        .expect("delete entity");

    let events = store
        .read_events(&persistence_id, 0)
        .await
        .expect("read event journal");
    let last = events.last().expect("tombstone event exists");
    assert_eq!(last.event_type, "Deleted");
    assert_eq!(
        last.payload
            .get("action")
            .and_then(serde_json::Value::as_str),
        Some("Deleted")
    );
    assert_eq!(
        last.payload
            .get("to_status")
            .and_then(serde_json::Value::as_str),
        Some("Deleted")
    );

    let state_after_restart = build_state_with_turso("test-delete-tombstone-list-2", store);
    state_after_restart.populate_index_from_store(&tenant).await;
    let ids = state_after_restart
        .list_entity_ids_lazy(&tenant, entity_type)
        .await;
    assert!(
        !ids.iter().any(|id| id == entity_id),
        "deleted entity should not be listed after restart/index rebuild"
    );

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn delete_failure_does_not_remove_live_entity_from_index() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-delete-tombstone-failure-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let tenant = TenantId::new("tenant-a");
    let entity_type = "Order";
    let entity_id = "ord-delete-failure";
    let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");

    let state = build_state_with_turso("test-delete-tombstone-failure", store.clone());
    state
        .get_or_create_tenant_entity(
            &tenant,
            entity_type,
            entity_id,
            serde_json::json!({"Title": "concurrency-race"}),
        )
        .await
        .expect("create entity");

    // Force delete persistence to fail by appending an external event first,
    // so the actor's expected sequence is stale.
    let expected_sequence = store
        .read_events(&persistence_id, 0)
        .await
        .expect("read seeded events")
        .last()
        .map(|event| event.sequence_nr)
        .expect("created event sequence present");
    let external = PersistenceEnvelope {
        sequence_nr: 0,
        event_type: "ExternalWrite".to_string(),
        payload: serde_json::json!({"action": "ExternalWrite"}),
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: sim_now(),
            actor_id: "concurrency-racer".to_string(),
        },
    };
    store
        .append(&persistence_id, expected_sequence, &[external])
        .await
        .expect("append external race event");

    let error = state
        .delete_tenant_entity(&tenant, entity_type, entity_id)
        .await
        .expect_err("delete should fail when tombstone append hits sequence race");
    assert!(
        error.contains("persistence failed"),
        "expected persistence failure error, got: {error}"
    );

    assert!(
        state.entity_exists(&tenant, entity_type, entity_id),
        "failed delete must not evict entity from in-memory index"
    );
    let live = state
        .get_tenant_entity_state(&tenant, entity_type, entity_id)
        .await
        .expect("entity actor should still be reachable");
    assert_ne!(
        live.state.status, "Deleted",
        "failed delete must not advance state to Deleted"
    );

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn ensure_entity_loaded_returns_false_for_deleted_entity() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-delete-tombstone-ensure-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let tenant = TenantId::new("tenant-a");
    let entity_type = "Order";
    let entity_id = "ord-delete-ensure";

    let state = build_state_with_turso("test-delete-tombstone-ensure-1", store.clone());
    state
        .get_or_create_tenant_entity(
            &tenant,
            entity_type,
            entity_id,
            serde_json::json!({"Title": "to-delete"}),
        )
        .await
        .expect("create entity");
    state
        .delete_tenant_entity(&tenant, entity_type, entity_id)
        .await
        .expect("delete entity");

    let state_after_restart = build_state_with_turso("test-delete-tombstone-ensure-2", store);
    let loaded = state_after_restart
        .ensure_entity_loaded(&tenant, entity_type, entity_id)
        .await;
    assert!(
        !loaded,
        "deleted entity should not be considered loadable from persistence"
    );
    assert!(
        !state_after_restart.entity_exists(&tenant, entity_type, entity_id),
        "deleted entity should not be indexed after ensure_entity_loaded"
    );

    let _ = std::fs::remove_file(db_path);
}

/// Regression: a partially populated in-memory index (one resident actor) must
/// not hide other durable entities of the same type from `list_entity_ids_lazy`.
/// Previously a non-empty index short-circuited the store scan, so collection
/// queries returned only the resident subset, reading present durable entities
/// as "not found".
#[tokio::test]
async fn list_entity_ids_lazy_surfaces_durable_entities_missing_from_partial_index() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-lazy-list-partial-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let tenant = TenantId::new("tenant-a");

    // Persist two durable Order entities through the normal create path.
    let writer = build_state_with_turso("test-lazy-list-partial-writer", store.clone());
    for id in ["ord-1", "ord-2"] {
        writer
            .get_or_create_tenant_entity(&tenant, "Order", id, serde_json::json!({"Title": id}))
            .await
            .expect("create durable order");
    }

    // Fresh process: empty in-memory index (simulates a server restart).
    let state = build_state_with_turso("test-lazy-list-partial-reader", store);

    // Touch exactly one entity so the index holds ONLY ord-1 (partial index).
    assert!(
        state.ensure_entity_loaded(&tenant, "Order", "ord-1").await,
        "ord-1 should hydrate from the durable store"
    );
    assert_eq!(
        state.list_entity_ids(&tenant, "Order"),
        vec!["ord-1".to_string()],
        "precondition: in-memory index is partial (only the touched entity)"
    );

    // Lazy listing must reconcile against the store and return BOTH entities.
    let mut ids = state.list_entity_ids_lazy(&tenant, "Order").await;
    ids.sort();
    assert_eq!(
        ids,
        vec!["ord-1".to_string(), "ord-2".to_string()],
        "lazy listing must surface durable entities absent from the partial index"
    );

    let _ = std::fs::remove_file(db_path);
}

// ── temper#529 P2 follow-up (issue #529): a journal read failure during
// hydration must not be reported the same way as confirmed absence ────────

/// RED: a non-resident entity with real history whose journal read fails
/// during hydration must surface as `Err`, never collapse into the same
/// `Ok(false)` that confirmed absence (zero history) produces.
#[tokio::test]
async fn ensure_entity_loaded_or_error_distinguishes_journal_failure_from_absence() {
    let store = SimEventStore::no_faults(529_101);
    let tenant = TenantId::new("tenant-a");
    let entity_id = "ord-journal-fail";

    let writer = build_state_with_sim("test-ensure-or-error-writer", store.clone());
    writer
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            entity_id,
            serde_json::json!({"Title": "x"}),
        )
        .await
        .expect("create durable order");

    // Cold reader: fresh actor registry/index, same durable journal.
    let reader = build_state_with_sim("test-ensure-or-error-reader", store.clone());
    assert!(
        !reader.entity_exists(&tenant, "Order", entity_id),
        "precondition: reader must not already have this entity resident"
    );

    let persistence_id = format!("{tenant}:Order:{entity_id}");
    store.fail_next_reads(&persistence_id, 1);

    let outcome = reader
        .ensure_entity_loaded_or_error(&tenant, "Order", entity_id)
        .await;
    assert!(
        outcome.is_err(),
        "a journal read failure for an entity that exists must not report Ok(false): {outcome:?}"
    );
    assert!(
        !reader.entity_exists(&tenant, "Order", entity_id),
        "a dependency failure must not fabricate residency either"
    );
}

/// Control: the legacy `bool` form keeps today's behavior exactly — every
/// existing caller that only distinguishes "loaded" vs "not loaded" must see
/// `false` for a journal read failure, identical to before this change.
#[tokio::test]
async fn ensure_entity_loaded_still_collapses_journal_failure_to_false_for_legacy_callers() {
    let store = SimEventStore::no_faults(529_102);
    let tenant = TenantId::new("tenant-a");
    let entity_id = "ord-journal-fail-legacy";

    let writer = build_state_with_sim("test-ensure-legacy-writer", store.clone());
    writer
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            entity_id,
            serde_json::json!({"Title": "x"}),
        )
        .await
        .expect("create durable order");

    let reader = build_state_with_sim("test-ensure-legacy-reader", store.clone());
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    store.fail_next_reads(&persistence_id, 1);

    let loaded = reader
        .ensure_entity_loaded(&tenant, "Order", entity_id)
        .await;
    assert!(
        !loaded,
        "legacy bool callers must keep collapsing a journal read failure to false"
    );
}

/// Control: a never-created id is still confirmed absence (`Ok(false)`), not
/// an error, when the journal itself is healthy.
#[tokio::test]
async fn ensure_entity_loaded_or_error_returns_ok_false_for_confirmed_absence() {
    let store = SimEventStore::no_faults(529_103);
    let state = build_state_with_sim("test-ensure-or-error-absent", store);
    let tenant = TenantId::new("tenant-a");

    let outcome = state
        .ensure_entity_loaded_or_error(&tenant, "Order", "ord-never-created")
        .await;
    assert_eq!(
        outcome,
        Ok(false),
        "a never-created id must stay confirmed-absent, not an error: {outcome:?}"
    );
}

/// Control: an entity already resident in the index is unaffected by a
/// journal fault here — the read is only a tombstone double-check for data
/// already held in memory, so a transient fault must not turn a live,
/// in-memory entity into a dependency error.
#[tokio::test]
async fn ensure_entity_loaded_or_error_resident_entity_unaffected_by_journal_fault() {
    let store = SimEventStore::no_faults(529_104);
    let state = build_state_with_sim("test-ensure-or-error-resident", store.clone());
    let tenant = TenantId::new("tenant-a");
    let entity_id = "ord-resident";

    state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            entity_id,
            serde_json::json!({"Title": "x"}),
        )
        .await
        .expect("create durable order");
    assert!(state.entity_exists(&tenant, "Order", entity_id));

    let persistence_id = format!("{tenant}:Order:{entity_id}");
    store.fail_next_reads(&persistence_id, 1);

    let outcome = state
        .ensure_entity_loaded_or_error(&tenant, "Order", entity_id)
        .await;
    assert_eq!(
        outcome,
        Ok(true),
        "a resident entity must not become a dependency error on a journal fault: {outcome:?}"
    );
}
