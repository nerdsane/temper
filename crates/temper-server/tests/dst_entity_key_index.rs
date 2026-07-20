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
use temper_runtime::persistence::{
    EntityKeyRow, EventMetadata, EventStore, KeyIndexBackfillFence, PersistenceEnvelope,
};
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
            },
            Duration::from_secs(5),
        )
        .await
        .expect("actor should respond")
}

async fn delete(actor_ref: &temper_runtime::actor::ActorRef<EntityMsg>) -> EntityResponse {
    actor_ref
        .ask(EntityMsg::Delete, Duration::from_secs(5))
        .await
        .expect("actor should respond")
}

async fn get_state(actor_ref: &temper_runtime::actor::ActorRef<EntityMsg>) -> EntityResponse {
    actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .expect("actor should respond")
}

fn doc_key_hash(workspace: &str, path: &str) -> String {
    doc_key_hash_values(serde_json::json!(workspace), serde_json::json!(path))
}

fn doc_key_hash_values(workspace: serde_json::Value, path: serde_json::Value) -> String {
    let mut fields = serde_json::Map::new();
    fields.insert("WorkspaceId".to_string(), workspace);
    fields.insert("Path".to_string(), path);
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

/// Deletion must release every declared key in the same atomic commit as the
/// tombstone. Otherwise the deleted entity remains the durable owner and a new
/// entity cannot reclaim the logically vacant key.
#[tokio::test]
async fn dst_delete_releases_declared_key_for_reclaim() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id) = install_deterministic_context(seed);
        let store: BoxedEventStore = BoxedEventStore::new(SimEventStore::no_faults(seed));
        let table = doc_table();
        let key_hash = doc_key_hash("ws1", "/reclaim.md");

        let system = ActorSystem::new("dst-key-reclaim");
        let first_id = format!("doc-first-{seed}");
        let first = EntityActor::with_persistence(
            "Doc",
            &first_id,
            table.clone(),
            serde_json::json!({}),
            store.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default");
        let first_ref = system.spawn(first, &first_id);

        let created = dispatch(
            &first_ref,
            "Create",
            serde_json::json!({ "WorkspaceId": "ws1", "Path": "/reclaim.md" }),
        )
        .await;
        assert!(
            created.success,
            "seed {seed}: first Create failed: {:?}",
            created.error
        );

        let deleted = delete(&first_ref).await;
        assert!(
            deleted.success,
            "seed {seed}: Delete failed: {:?}",
            deleted.error
        );
        assert_eq!(
            store
                .lookup_by_key("default", "Doc", "path", &key_hash)
                .await
                .expect("lookup after delete"),
            None,
            "seed {seed}: a tombstoned entity must release its declared key"
        );

        drop(first_ref);
        drop(system);

        // Recreate the actor system and recover the deleted stream. Tombstone state
        // and released ownership must both survive replay, not just the live actor.
        let restarted = ActorSystem::new("dst-key-reclaim-restarted");
        let recovered = EntityActor::with_persistence(
            "Doc",
            &first_id,
            table.clone(),
            serde_json::json!({}),
            store.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default");
        let recovered_ref = restarted.spawn(recovered, &first_id);
        let recovered_state = get_state(&recovered_ref).await;
        assert_eq!(recovered_state.state.status, "Deleted");
        assert_eq!(
            store
                .lookup_by_key("default", "Doc", "path", &key_hash)
                .await
                .expect("lookup after recovery"),
            None,
            "seed {seed}: replay must not restore tombstoned ownership"
        );

        // Two replacements race for the now-vacant key. The store must serialize
        // uniqueness so exactly one journal advances beyond its bootstrap event.
        let left_id = format!("doc-left-{seed}");
        let right_id = format!("doc-right-{seed}");
        let spawn_replacement = |entity_id: &str| {
            restarted.spawn(
                EntityActor::with_persistence(
                    "Doc",
                    entity_id,
                    table.clone(),
                    serde_json::json!({}),
                    store.clone(),
                    BackendLabel::Sim,
                )
                .with_tenant("default"),
                entity_id,
            )
        };
        let left = spawn_replacement(&left_id);
        let right = spawn_replacement(&right_id);
        let params = serde_json::json!({ "WorkspaceId": "ws1", "Path": "/reclaim.md" });
        let (left_result, right_result) = tokio::join!(
            dispatch(&left, "Create", params.clone()),
            dispatch(&right, "Create", params)
        );
        assert_ne!(left_result.success, right_result.success);
        let (winner, loser) = if left_result.success {
            (&left_id, &right_id)
        } else {
            (&right_id, &left_id)
        };
        assert_eq!(
            store
                .lookup_by_key("default", "Doc", "path", &key_hash)
                .await
                .expect("lookup after reclaim"),
            Some(winner.clone()),
            "seed {seed}: the reclaimed key must resolve to the live replacement"
        );
        assert_eq!(
            store
                .read_events(&format!("default:Doc:{loser}"), 0)
                .await
                .unwrap()
                .len(),
            1,
            "seed {seed}: losing journal must retain only its bootstrap event"
        );
    }
}

/// Retried delivery after a successful tombstone append is idempotent. A duplicate
/// terminal suffix would make replay stop below the store's durable high-water and
/// leave exact repair unable to publish coverage.
#[tokio::test]
async fn dst_retried_delete_does_not_append_a_terminal_suffix() {
    let seed = 238;
    let (_guard, _clock, _id) = install_deterministic_context(seed);
    let sim = SimEventStore::no_faults(seed);
    let store = BoxedEventStore::new(sim.clone());
    let table = doc_table();
    let entity_id = "doc-retried-delete";
    let persistence_id = format!("default:Doc:{entity_id}");
    let system = ActorSystem::new("dst-retried-delete");
    let actor_ref = system.spawn(
        EntityActor::with_persistence(
            "Doc",
            entity_id,
            table.clone(),
            serde_json::json!({}),
            store.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        entity_id,
    );
    assert!(
        dispatch(
            &actor_ref,
            "Create",
            serde_json::json!({"WorkspaceId": "ws", "Path": "/retry-delete"}),
        )
        .await
        .success
    );
    let sequence_before_delete = sim
        .dump_journal(&persistence_id)
        .last()
        .expect("created history")
        .sequence_nr;
    let first_delete = delete(&actor_ref).await;
    assert!(first_delete.success);
    assert_eq!(first_delete.state.sequence_nr, sequence_before_delete + 1);
    let second_delete = delete(&actor_ref).await;
    assert!(
        second_delete.success,
        "retry should be an idempotent success"
    );
    assert_eq!(
        sim.dump_journal(&persistence_id).len(),
        (sequence_before_delete + 1) as usize,
        "the durable prefix plus exactly one terminal event"
    );
    drop(actor_ref);
    drop(system);

    let restarted_system = ActorSystem::new("dst-retried-delete-restart");
    let restarted = restarted_system.spawn(
        EntityActor::with_persistence(
            "Doc",
            entity_id,
            table,
            serde_json::json!({}),
            store.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default"),
        "doc-retried-delete-restarted",
    );
    let recovered = get_state(&restarted).await.state;
    assert_eq!(recovered.status, "Deleted");
    assert_eq!(recovered.sequence_nr, sequence_before_delete + 1);

    let signature = "v4:path";
    let revision = store
        .begin_key_index_backfill("default", "Doc", signature)
        .await
        .expect("begin exact repair");
    store
        .backfill_entity_keys(
            "default",
            "Doc",
            entity_id,
            recovered.sequence_nr,
            KeyIndexBackfillFence {
                key_set_signature: signature,
                contract_revision: revision,
                expected_journal_sequence: recovered.sequence_nr,
                expected_entity_live: false,
            },
            &[],
        )
        .await
        .expect("deleted stream must remain repairable");
    assert!(
        store
            .mark_key_index_backfilled_if_revision("default", "Doc", signature, revision)
            .await
            .expect("publish repaired coverage")
    );
}

/// Every persisted re-key replaces the entity's complete ownership set: old
/// values disappear, partial-null composite values remain indexable, and an
/// all-null composite releases ownership rather than leaving its prior row.
#[tokio::test]
async fn dst_rekey_reconciles_rename_partial_null_and_all_null() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id) = install_deterministic_context(seed);
        let store: BoxedEventStore = BoxedEventStore::new(SimEventStore::no_faults(seed));
        let table = doc_table();
        let entity_id = format!("doc-rekey-{seed}");
        let system = ActorSystem::new("dst-key-rekey");
        let actor = EntityActor::with_persistence(
            "Doc",
            &entity_id,
            table,
            serde_json::json!({}),
            store.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default");
        let actor_ref = system.spawn(actor, &entity_id);

        let created = dispatch(
            &actor_ref,
            "Create",
            serde_json::json!({ "WorkspaceId": "ws1", "Path": "/old.md" }),
        )
        .await;
        assert!(created.success, "seed {seed}: Create failed");

        let renamed = dispatch(
            &actor_ref,
            "Rekey",
            serde_json::json!({ "WorkspaceId": "ws1", "Path": "/new.md" }),
        )
        .await;
        assert!(renamed.success, "seed {seed}: string rename failed");
        assert_eq!(
            store
                .lookup_by_key("default", "Doc", "path", &doc_key_hash("ws1", "/old.md"))
                .await
                .unwrap(),
            None,
            "seed {seed}: rename must release the old key"
        );
        assert_eq!(
            store
                .lookup_by_key("default", "Doc", "path", &doc_key_hash("ws1", "/new.md"))
                .await
                .unwrap(),
            Some(entity_id.clone()),
            "seed {seed}: rename must claim the new key"
        );

        let partial_null = dispatch(
            &actor_ref,
            "Rekey",
            serde_json::json!({ "WorkspaceId": "ws1", "Path": null }),
        )
        .await;
        assert!(
            partial_null.success,
            "seed {seed}: partial-null re-key failed"
        );
        let partial_hash = doc_key_hash_values(serde_json::json!("ws1"), serde_json::Value::Null);
        assert_eq!(
            store
                .lookup_by_key("default", "Doc", "path", &doc_key_hash("ws1", "/new.md"))
                .await
                .unwrap(),
            None,
            "seed {seed}: partial-null re-key must release the prior string key"
        );
        assert_eq!(
            store
                .lookup_by_key("default", "Doc", "path", &partial_hash)
                .await
                .unwrap(),
            Some(entity_id.clone()),
            "seed {seed}: partial-null composite key remains indexable"
        );

        let all_null = dispatch(
            &actor_ref,
            "Rekey",
            serde_json::json!({ "WorkspaceId": null, "Path": null }),
        )
        .await;
        assert!(all_null.success, "seed {seed}: all-null re-key failed");
        assert_eq!(
            store
                .lookup_by_key("default", "Doc", "path", &partial_hash)
                .await
                .unwrap(),
            None,
            "seed {seed}: all-null key must release the entity's final claim"
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
        let repair_signature = "v3:path";
        let repair_revision = store
            .begin_key_index_backfill("default", "Doc", repair_signature)
            .await
            .unwrap();
        store
            .backfill_entity_keys(
                "default",
                "Doc",
                &format!("doc-pre-{seed}"),
                1,
                KeyIndexBackfillFence {
                    key_set_signature: repair_signature,
                    contract_revision: repair_revision,
                    expected_journal_sequence: 1,
                    expected_entity_live: true,
                },
                &rows,
            )
            .await
            .unwrap();
        // Idempotent: a second backfill is a no-op-equivalent.
        store
            .backfill_entity_keys(
                "default",
                "Doc",
                &format!("doc-pre-{seed}"),
                1,
                KeyIndexBackfillFence {
                    key_set_signature: repair_signature,
                    contract_revision: repair_revision,
                    expected_journal_sequence: 1,
                    expected_entity_live: true,
                },
                &rows,
            )
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
