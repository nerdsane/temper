//! DST: the entity_key_index negative-existence invariant (ADR-0153, ARN-68).
//!
//! Property: a read by an entity's declared key returns the entity **iff** it
//! exists — present and absent in one probe, under every seed. This is the
//! access path the read plane lacks today (proving absence requires a scan,
//! which 413s at scale). Real `EntityActor` + `SimEventStore`, all seeds.
//!
//! RED until the co-commit + keyed read land: the actor must pass the declared
//! key to `append_with_keys`, and the store must write/read `entity_key_index`.
//! Today both default to no-ops, so the keyed read of an existing Doc returns
//! `None` and the `present` assertion fails — that is the missing access path.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::install_deterministic_context;
use temper_server::key_index::canonical_key_hash;
use temper_server::storage::{BackendLabel, BoxedEventStore};
use temper_server::{EntityActor, EntityMsg, EntityResponse};
use temper_store_sim::SimEventStore;

const DOC_IOA: &str = include_str!("../../../test-fixtures/specs/keyed_doc.ioa.toml");
const NUM_SEEDS: u64 = 100;

fn doc_table() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(DOC_IOA)))
}

async fn dispatch(
    actor_ref: &temper_runtime::actor::ActorRef<EntityMsg>,
    action: &str,
    params: serde_json::Value,
) -> EntityResponse {
    actor_ref
        .ask(
            EntityMsg::Action {
                name: action.to_string(),
                params,
                cross_entity_booleans: BTreeMap::new(),
                idempotency_key: None,
            },
            Duration::from_secs(5),
        )
        .await
        .expect("actor should respond")
}

fn doc_key_hash(workspace: &str, path: &str) -> String {
    let mut fields = serde_json::Map::new();
    fields.insert("WorkspaceId".to_string(), serde_json::json!(workspace));
    fields.insert("Path".to_string(), serde_json::json!(path));
    canonical_key_hash(
        "path",
        &["WorkspaceId".to_string(), "Path".to_string()],
        &fields,
    )
    .expect("complete key")
}

#[tokio::test]
async fn dst_keyed_read_is_present_iff_entity_exists() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id) = install_deterministic_context(seed);
        let store: BoxedEventStore = BoxedEventStore::new(SimEventStore::no_faults(seed));
        let table = doc_table();
        let entity_id = format!("doc-{seed}");

        let system = ActorSystem::new("dst-keyed");
        let actor = EntityActor::with_persistence(
            "Doc",
            &entity_id,
            table.clone(),
            serde_json::json!({}),
            store.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default");
        let actor_ref = system.spawn(actor, &entity_id);

        let r = dispatch(
            &actor_ref,
            "Create",
            serde_json::json!({ "WorkspaceId": "ws1", "Path": "/a.md" }),
        )
        .await;
        assert!(r.success, "seed {seed}: Create failed: {:?}", r.error);

        // PRESENT: the created Doc resolves by its declared (WorkspaceId, Path) key.
        let present = store
            .lookup_by_key("default", "Doc", "path", &doc_key_hash("ws1", "/a.md"))
            .await
            .expect("lookup ok");
        assert_eq!(
            present,
            Some(entity_id.clone()),
            "seed {seed}: keyed read must find the created Doc (present)"
        );

        // ABSENT: a key no Doc holds resolves to None in one probe (no scan).
        let absent = store
            .lookup_by_key("default", "Doc", "path", &doc_key_hash("ws1", "/nope.md"))
            .await
            .expect("lookup ok");
        assert_eq!(
            absent, None,
            "seed {seed}: keyed read of a missing key must be absent"
        );
    }
}
