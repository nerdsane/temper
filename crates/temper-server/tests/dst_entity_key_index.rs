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
use temper_runtime::persistence::{EntityKeyRow, EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
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
                expected_sequence: None,
                reaction_context: None,
                expected_authorization_precondition: None,
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

/// Backfill (ADR-0153): an entity written BEFORE its key was declared has no key
/// row, so a keyed read misses (would fall back to scan). After
/// `backfill_entity_keys`, the keyed read resolves it — the path that lets a keyed
/// miss become authoritative absence post-backfill. Idempotent. All seeds.
#[tokio::test]
async fn dst_backfill_makes_pre_existing_entity_keyed_findable() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);

        // Pre-existing entity: journal append WITHOUT keys (as if written before
        // the [[key]] declaration existed).
        let pid = format!("default:Doc:doc-pre-{seed}");
        store.append(&pid, 0, &[test_envelope()]).await.unwrap();

        let key_hash = doc_key_hash("ws1", "/pre.md");
        // Before backfill: keyed miss.
        assert_eq!(
            store
                .lookup_by_key("default", "Doc", "path", &key_hash)
                .await
                .unwrap(),
            None,
            "seed {seed}: pre-backfill entity has no key row (keyed miss)"
        );

        // Backfill the key row (no new journal event).
        let rows = [EntityKeyRow {
            key_name: "path".to_string(),
            key_hash: key_hash.clone(),
        }];
        store
            .backfill_entity_keys("default", "Doc", &format!("doc-pre-{seed}"), &rows)
            .await
            .unwrap();
        // Idempotent: a second backfill is a no-op-equivalent.
        store
            .backfill_entity_keys("default", "Doc", &format!("doc-pre-{seed}"), &rows)
            .await
            .unwrap();

        // After backfill: keyed read resolves it.
        assert_eq!(
            store
                .lookup_by_key("default", "Doc", "path", &key_hash)
                .await
                .unwrap(),
            Some(format!("doc-pre-{seed}")),
            "seed {seed}: after backfill the entity is keyed-findable"
        );
    }
}

fn test_envelope() -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 1,
        event_type: "Create".to_string(),
        payload: serde_json::json!({}),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: "test".to_string(),
        },
    }
}

/// Atomicity: a co-commit that hits a declared-key uniqueness reject must leave
/// the journal UNCHANGED — present iff the journal committed. A reject that still
/// advanced the journal would replay the rejected transition. Real SimEventStore
/// co-commit path, all seeds.
#[tokio::test]
async fn dst_co_commit_atomic_on_uniqueness_reject() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id) = install_deterministic_context(seed);
        let store = SimEventStore::no_faults(seed);
        let key = EntityKeyRow {
            key_name: "path".to_string(),
            key_hash: "k-collision".to_string(),
        };

        // A claims the key.
        store
            .append_with_keys(
                "default:Doc:doc-a",
                0,
                &[test_envelope()],
                std::slice::from_ref(&key),
            )
            .await
            .unwrap_or_else(|e| panic!("seed {seed}: A should claim the key: {e:?}"));

        // B tries the SAME key -> must be rejected AND must not advance B's journal.
        let res = store
            .append_with_keys(
                "default:Doc:doc-b",
                0,
                &[test_envelope()],
                std::slice::from_ref(&key),
            )
            .await;
        assert!(
            res.is_err(),
            "seed {seed}: a duplicate declared key must be rejected"
        );

        let b_events = store
            .read_events("default:Doc:doc-b", 0)
            .await
            .expect("read B journal");
        assert!(
            b_events.is_empty(),
            "seed {seed}: a rejected co-commit must leave the journal unchanged (atomic); got {} event(s)",
            b_events.len()
        );

        let holder = store
            .lookup_by_key("default", "Doc", "path", "k-collision")
            .await
            .expect("lookup");
        assert_eq!(
            holder,
            Some("doc-a".to_string()),
            "seed {seed}: the key must still be held by A"
        );
    }
}
