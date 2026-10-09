//! Legacy-format fixtures: journals and snapshots written before request
//! binding existed.
//!
//! The "old format" is today's format: events carry `idempotency_key` but no
//! `idempotency_binding`, and snapshots carry `processed_idempotency_keys`.
//! `legacy_copy` strips the (planned) `idempotency_binding` payload field so the
//! fixture stays legacy even after the fix starts writing it. Before the fix
//! the strip is a no-op.

use serde_json::Value;
use temper_runtime::persistence::{EventStore, PersistenceEnvelope};
use temper_store_sim::SimEventStore;

use super::harness::persistence_id;

/// Payload field name the fix writes; removed to fabricate legacy history.
pub const BINDING_FIELD: &str = "idempotency_binding";

/// Copy `tenant:Order:id`'s journal into a fresh store with the binding field
/// stripped from every payload.
pub async fn legacy_copy(
    source: &SimEventStore,
    tenant: &str,
    id: &str,
    seed: u64,
) -> SimEventStore {
    let pid = persistence_id(tenant, id);
    let target = SimEventStore::no_faults(seed);
    let events: Vec<PersistenceEnvelope> = source
        .dump_journal(&pid)
        .into_iter()
        .map(|mut env| {
            if let Some(obj) = env.payload.as_object_mut() {
                obj.remove(BINDING_FIELD);
                obj.remove("idempotency_reply");
            }
            env
        })
        .collect();
    assert!(
        !events.is_empty(),
        "harness setup: source journal {pid} is empty"
    );
    target
        .append(&pid, 0, &events)
        .await
        .unwrap_or_else(|e| panic!("harness setup: legacy journal append: {e}"));
    target
}

/// Write an old-format snapshot for `tenant:Order:id` at the journal head,
/// built from an entity response body, optionally adding processed keys that
/// have no journal event.
pub async fn save_legacy_snapshot(
    store: &SimEventStore,
    tenant: &str,
    id: &str,
    entity_body: &Value,
    extra_processed_keys: &[&str],
) {
    let pid = persistence_id(tenant, id);
    let head = store.dump_journal(&pid).len() as u64;
    let mut snapshot = entity_body.clone();
    let obj = snapshot
        .as_object_mut()
        .unwrap_or_else(|| panic!("harness setup: entity body is not an object"));
    obj.retain(|k, _| !k.starts_with("@odata."));
    obj.insert("events".to_string(), Value::Array(vec![]));
    if let Some(Value::Object(keys)) = obj.get_mut("processed_idempotency_keys") {
        for key in extra_processed_keys {
            keys.insert((*key).to_string(), Value::from(head));
        }
    } else {
        panic!("harness setup: entity body has no processed_idempotency_keys map");
    }
    let bytes = serde_json::to_vec(&snapshot)
        .unwrap_or_else(|e| panic!("harness setup: snapshot encode: {e}"));
    store
        .save_snapshot(&pid, head, &bytes)
        .await
        .unwrap_or_else(|e| panic!("harness setup: snapshot save: {e}"));
}
