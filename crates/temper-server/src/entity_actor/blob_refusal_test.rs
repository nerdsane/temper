//! A blob store that does not take an action's field-overflow blob fails the
//! action as a store that does not take its event does: the store is
//! unavailable, so the caller, or a write the kernel owes, tries it again.

use super::*;
use std::collections::BTreeMap;
use std::time::Duration;
use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_store_sim::SimEventStore;

use crate::entity_actor::effects::DEFAULT_FIELD_INLINE_MAX;
use crate::storage::{BackendLabel, BoxedEventStore};

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

#[tokio::test]
async fn a_blob_the_blob_store_does_not_take_leaves_the_store_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    // A file where the blob directory goes, so no blob can be stored.
    let blobs = dir.path().join("blobs");
    std::fs::write(&blobs, b"").unwrap();
    let store = Arc::new(SimEventStore::no_faults(59));
    let actor = EntityActor::with_persistence(
        "Order",
        "large",
        Arc::new(RwLock::new(TransitionTable::from_ioa_source(ORDER_IOA))),
        serde_json::json!({}),
        BoxedEventStore::from_arc(store.clone()),
        BackendLabel::Sim,
    )
    .with_blob_store(Some(crate::blob_store::BlobStore::local_fs(&blobs)));
    let actor = ActorSystem::new("blob-refusal").spawn(actor, "large");

    let reply: EntityResponse = actor
        .ask(
            EntityMsg::Action {
                caller: None,
                name: "AddItem".into(),
                params: serde_json::json!({ "ProductId": "p".repeat(DEFAULT_FIELD_INLINE_MAX + 1) }),
                related: BTreeMap::new(),
                idempotency_key: None,
                expected_authorization_precondition: None,
            },
            Duration::from_secs(5),
        )
        .await
        .unwrap();

    assert!(!reply.success, "the action must not land without its blob");
    assert!(
        reply.store_unavailable,
        "a blob the store did not take is the store's failure: {:?}",
        reply.error
    );
    assert_eq!(
        reply.state.sequence_nr, 1,
        "only the record's Created event is written"
    );
}
