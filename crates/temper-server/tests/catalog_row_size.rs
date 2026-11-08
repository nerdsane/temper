//! A record's catalog row stays the same size as it takes moves, while the
//! record still answers a retried move from its own durable key set.
//!
//! Every move rewrites the record's catalog row, and the store's change log
//! keeps the row before and after. A row that grew with each keyed move made
//! a busy record's change log grow by its whole key set on every move.

use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::persistence::EventStore;
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_server::idempotency::IdempotencyKey;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;

const CSDL: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");
const ENTITY_ID: &str = "o-busy";
/// One more than the actor's durable key budget, so the key set is full.
const MOVES: usize = 1_001;
/// The actor's default snapshot interval, in moves.
const SNAPSHOT_INTERVAL: u64 = 100;

fn order_state(store: TursoEventStore) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).expect("fixture CSDL"),
        CSDL.to_string(),
        &[("Order", ORDER)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("catalog-row-size"), registry);
    state.set_storage_stack(StorageStack::from_turso(store));
    state
}

/// A key as long as a client's UUID-based keys.
fn key(n: usize) -> AgentContext {
    AgentContext {
        idempotency_key: Some(IdempotencyKey::Request(format!(
            "3f2a9c4e-0b1d-4e8f-a7c6-{n:012}"
        ))),
        ..AgentContext::default()
    }
}

async fn add_item(state: &ServerState, n: usize) -> temper_server::EntityState {
    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Order",
            ENTITY_ID,
            "AddItem",
            serde_json::json!({"ProductId": "p-1", "Quantity": 1}),
            &key(n),
        )
        .await
        .expect("AddItem dispatches");
    assert!(response.success, "AddItem failed: {:?}", response.error);
    response.state
}

/// Serialized bytes of the record's catalog state once it holds `sequence_nr`.
async fn catalog_state_bytes(store: &TursoEventStore, sequence_nr: u64) -> usize {
    for _ in 0..500 {
        let rows = store
            .load_entity_catalog_rows("default", "Order", &[ENTITY_ID.to_string()])
            .await
            .expect("load catalog row");
        if let Some(row) = rows.into_iter().find(|row| row.sequence_nr >= sequence_nr) {
            let state = row.state.expect("catalog row has a state");
            return serde_json::to_string(&state).expect("state json").len();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("catalog row never reached sequence {sequence_nr}");
}

/// The record's stored snapshot as `(sequence_nr, bytes)`, once the snapshot
/// due at or before `sequence_nr` has been written.
async fn settled_snapshot(store: &TursoEventStore, sequence_nr: u64) -> (u64, usize) {
    let due = sequence_nr / SNAPSHOT_INTERVAL * SNAPSHOT_INTERVAL;
    for _ in 0..500 {
        if let Some((snapshot_seq, bytes)) = store
            .load_snapshot(&format!("default:Order:{ENTITY_ID}"))
            .await
            .expect("load snapshot")
            && snapshot_seq >= due
        {
            return (snapshot_seq, bytes.len());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the snapshot due by sequence {sequence_nr} was never written");
}

#[tokio::test]
async fn a_record_that_takes_a_thousand_keyed_moves_keeps_a_small_catalog_row() {
    let (_guard, _clock, _ids) = install_deterministic_context(1_001);
    let dir = tempfile::tempdir().expect("temp dir");
    let url = format!("file:{}", dir.path().join("journal.db").display());
    let store = TursoEventStore::new(&url, None).await.expect("open store");
    let state = order_state(store.clone());
    let tenant = TenantId::default();
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            ENTITY_ID,
            serde_json::json!({"Title": "busy order"}),
        )
        .await
        .expect("create order");

    let mut early_bytes = 0;
    let mut current = None;
    for n in 0..MOVES {
        let moved = add_item(&state, n).await;
        if n == 9 {
            early_bytes = catalog_state_bytes(&store, moved.sequence_nr).await;
        }
        current = Some(moved);
    }
    let current = current.expect("moves ran");
    assert_eq!(
        current.processed_idempotency_keys.len(),
        1_000,
        "the actor keeps its full durable key set"
    );

    let late_bytes = catalog_state_bytes(&store, current.sequence_nr).await;
    assert!(
        late_bytes < early_bytes + 256,
        "the catalog state grew from {early_bytes} to {late_bytes} bytes over {MOVES} moves"
    );

    // One more move changes the catalog row by a few bytes. It is not on a
    // snapshot boundary, so it leaves the snapshot row, which carries the key
    // set once per snapshot interval, as it was.
    let snapshot = settled_snapshot(&store, current.sequence_nr).await;
    let one_more = add_item(&state, MOVES).await;
    let one_more_bytes = catalog_state_bytes(&store, one_more.sequence_nr).await;
    assert!(
        one_more_bytes.abs_diff(late_bytes) < 256,
        "one more move changed the catalog state from {late_bytes} to {one_more_bytes} bytes"
    );
    assert_eq!(
        settled_snapshot(&store, one_more.sequence_nr).await,
        snapshot,
        "a move between snapshots leaves the snapshot row as it was"
    );
    let current = one_more;

    // After a restart, with no reply cache and no actor in memory, a retry of
    // a recent move is answered from the key set the actor restores from its
    // snapshot and journal, and is not applied again.
    drop(state);
    let restarted = order_state(store.clone());
    let retried = add_item(&restarted, MOVES - 50).await;
    assert_eq!(
        (retried.counters["items"], retried.sequence_nr),
        (current.counters["items"], current.sequence_nr),
        "a retried move after a restart is not applied twice"
    );
    assert_eq!(
        retried.processed_idempotency_keys.len(),
        1_000,
        "the restored actor holds its full durable key set"
    );
}
