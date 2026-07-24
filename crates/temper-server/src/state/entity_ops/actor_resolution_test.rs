use std::sync::{Arc, RwLock};

use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::actor::ActorRef;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

use crate::entity_actor::{EntityActor, EntityMsg};
use crate::storage::StorageStack;

use super::ServerState;

const CSDL_XML: &str = include_str!("../../../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../../../test-fixtures/specs/order.ioa.toml");

fn test_state(seed: u64) -> (ServerState, SimEventStore) {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parses");
    let mut state = ServerState::new(
        ActorSystem::new("actor-resolution-cleanup"),
        csdl,
        CSDL_XML.to_string(),
    );
    let store = SimEventStore::no_faults(seed);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    (state, store)
}

fn insert_actor(state: &ServerState, tenant: &TenantId, entity_id: &str) -> ActorRef<EntityMsg> {
    let actor_key = format!("{tenant}:Order:{entity_id}");
    let actor_ref = state.actor_system.spawn(
        EntityActor::new(
            "Order",
            entity_id,
            Arc::new(RwLock::new(TransitionTable::from_ioa_source(ORDER_IOA))),
            serde_json::json!({}),
        )
        .with_tenant(tenant.as_str()),
        &actor_key,
    );
    state
        .actor_registry
        .write()
        .expect("actor registry lock")
        .insert(actor_key.clone(), actor_ref.clone());
    state
        .last_accessed
        .write()
        .expect("actor access lock")
        .insert(actor_key, sim_now());
    state
        .entity_index
        .write()
        .expect("entity index lock")
        .entry(format!("{tenant}:Order"))
        .or_default()
        .insert(entity_id.to_string());
    actor_ref
}

fn assert_actor_is_indexed(
    state: &ServerState,
    tenant: &TenantId,
    entity_id: &str,
    expected: &ActorRef<EntityMsg>,
) {
    let actor_key = format!("{tenant}:Order:{entity_id}");
    assert_eq!(
        state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .get(&actor_key)
            .map(ActorRef::id),
        Some(expected.id())
    );
    assert!(
        state
            .entity_index
            .read()
            .expect("entity index lock")
            .get(&format!("{tenant}:Order"))
            .is_some_and(|ids| ids.contains(entity_id))
    );
}

fn assert_terminal_actor_evicted_but_indexed(
    state: &ServerState,
    tenant: &TenantId,
    entity_id: &str,
) {
    let actor_key = format!("{tenant}:Order:{entity_id}");
    assert!(
        !state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&actor_key)
    );
    assert!(
        !state
            .last_accessed
            .read()
            .expect("actor access lock")
            .contains_key(&actor_key)
    );
    assert!(
        state
            .entity_index
            .read()
            .expect("entity index lock")
            .get(&format!("{tenant}:Order"))
            .is_some_and(|ids| ids.contains(entity_id))
    );
}

#[tokio::test]
async fn failed_cleanup_removes_only_the_empty_actor_incarnation() {
    let (_guard, _clock, _ids) = install_deterministic_context(237);
    let (state, _store) = test_state(237);
    let tenant = TenantId::default();
    let entity_id = "empty-history";
    let actor_key = format!("{tenant}:Order:{entity_id}");
    let actor_ref = insert_actor(&state, &tenant, entity_id);

    state
        .discard_uncommitted_spawn_after_dispatch_failure(
            &tenant,
            "Order",
            entity_id,
            actor_ref.id(),
        )
        .await;

    assert!(
        !state
            .actor_registry
            .read()
            .expect("actor registry lock")
            .contains_key(&actor_key)
    );
    assert!(
        !state
            .last_accessed
            .read()
            .expect("actor access lock")
            .contains_key(&actor_key)
    );
    assert!(
        !state
            .entity_index
            .read()
            .expect("entity index lock")
            .get(&format!("{tenant}:Order"))
            .is_some_and(|ids| ids.contains(entity_id))
    );
}

#[tokio::test]
async fn failed_cleanup_preserves_a_replacement_actor_incarnation() {
    let (_guard, _clock, _ids) = install_deterministic_context(238);
    let (state, _store) = test_state(238);
    let tenant = TenantId::default();
    let entity_id = "replacement-incarnation";
    let original = insert_actor(&state, &tenant, entity_id);
    let replacement = insert_actor(&state, &tenant, entity_id);
    assert_ne!(original.id(), replacement.id());

    state
        .discard_uncommitted_spawn_after_dispatch_failure(
            &tenant,
            "Order",
            entity_id,
            original.id(),
        )
        .await;

    assert_actor_is_indexed(&state, &tenant, entity_id, &replacement);
}

#[tokio::test]
async fn failed_cleanup_evicts_terminal_actor_but_preserves_index_when_history_read_is_ambiguous() {
    let (_guard, _clock, _ids) = install_deterministic_context(239);
    let (state, store) = test_state(239);
    let tenant = TenantId::default();
    let entity_id = "ambiguous-history";
    let actor_ref = insert_actor(&state, &tenant, entity_id);
    store.fail_next_reads(&format!("{tenant}:Order:{entity_id}"), 1);

    state
        .discard_uncommitted_spawn_after_dispatch_failure(
            &tenant,
            "Order",
            entity_id,
            actor_ref.id(),
        )
        .await;

    assert_terminal_actor_evicted_but_indexed(&state, &tenant, entity_id);
}

#[tokio::test]
async fn failed_cleanup_evicts_terminal_actor_but_preserves_index_with_durable_history() {
    let (_guard, _clock, _ids) = install_deterministic_context(240);
    let (state, store) = test_state(240);
    let tenant = TenantId::default();
    let entity_id = "durable-history";
    let persistence_id = format!("{tenant}:Order:{entity_id}");
    let actor_id = persistence_id.clone();
    let actor_ref = insert_actor(&state, &tenant, entity_id);
    store
        .append(
            &persistence_id,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 1,
                event_type: "Created".to_string(),
                payload: serde_json::json!({}),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id,
                },
            }],
        )
        .await
        .expect("seed durable history");

    state
        .discard_uncommitted_spawn_after_dispatch_failure(
            &tenant,
            "Order",
            entity_id,
            actor_ref.id(),
        )
        .await;

    assert_terminal_actor_evicted_but_indexed(&state, &tenant, entity_id);
}
