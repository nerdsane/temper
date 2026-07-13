//! DST: the entity_vector_index kNN reproducibility invariant (ADR-0155).
//!
//! Property: seeded writes of declared vectors, followed by a `Temper.Nearest`
//! ranking, produce the **same order under every seed**. The kernel ranks a
//! store-supplied, id-ordered candidate list with f32 accumulation in that fixed
//! order and an entity-id tiebreak — so the sim seed (which drives fault injection
//! and scheduling) cannot change the result. This is what makes kernel-side
//! similarity admissible where app-side similarity never was.
//!
//! Real `EntityActor` + `SimEventStore` co-commit path, all seeds.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::install_deterministic_context;
use temper_server::storage::{BackendLabel, BoxedEventStore};
use temper_server::vector_index::{VectorMetric, rank_nearest};
use temper_server::{EntityActor, EntityMsg, EntityResponse};
use temper_store_sim::SimEventStore;

const ITEM_IOA: &str = include_str!("../../../test-fixtures/specs/vectored_item.ioa.toml");
const NUM_SEEDS: u64 = 100;

fn item_table() -> Arc<RwLock<TransitionTable>> {
    Arc::new(RwLock::new(TransitionTable::from_ioa_source(ITEM_IOA)))
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

/// Create one Item with a JSON-string embedding and a model tag.
async fn create_item(
    system: &ActorSystem,
    table: &Arc<RwLock<TransitionTable>>,
    store: &BoxedEventStore,
    entity_id: &str,
    embedding: &[f32],
    model: &str,
) {
    let actor = EntityActor::with_persistence(
        "Item",
        entity_id,
        table.clone(),
        serde_json::json!({}),
        store.clone(),
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let actor_ref = system.spawn(actor, entity_id);
    let embedding_json = serde_json::to_string(embedding).expect("serialize embedding");
    let r = dispatch(
        &actor_ref,
        "Create",
        serde_json::json!({ "Embedding": embedding_json, "EmbeddingModel": model }),
    )
    .await;
    assert!(r.success, "Create failed: {:?}", r.error);
}

/// The fixed corpus every seed writes. Cosine nearest to [1,0,0,0] is `a`
/// (identical direction), then `b` (near), then `c`/`d` (orthogonal, score 0,
/// broken to id order c < d).
fn corpus() -> Vec<(&'static str, [f32; 4], &'static str)> {
    vec![
        ("item-a", [1.0, 0.0, 0.0, 0.0], "m1"),
        ("item-b", [0.9, 0.1, 0.0, 0.0], "m1"),
        ("item-c", [0.0, 1.0, 0.0, 0.0], "m1"),
        ("item-d", [0.0, 0.0, 1.0, 0.0], "m1"),
        // A different model tag: must never appear in an m1 ranking.
        ("item-e", [1.0, 0.0, 0.0, 0.0], "m2"),
    ]
}

#[tokio::test]
async fn dst_nearest_ranking_is_reproducible_across_seeds() {
    let query = [1.0f32, 0.0, 0.0, 0.0];
    let mut reference_order: Option<Vec<String>> = None;

    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id) = install_deterministic_context(seed);
        let store: BoxedEventStore = BoxedEventStore::new(SimEventStore::no_faults(seed));
        let table = item_table();
        let system = ActorSystem::new("dst-vector");

        for (id, embedding, model) in corpus() {
            create_item(&system, &table, &store, id, &embedding, model).await;
        }

        // Co-commit: the m1 partition holds exactly the four m1 items right after
        // the writes (no async projection lag on the co-committing sim store).
        let candidates = store
            .vector_candidates("default", "Item", "embed", "m1", 1000)
            .await
            .expect("vector candidates");
        assert_eq!(
            candidates.len(),
            4,
            "seed {seed}: m1 partition must hold the four m1 items (model partitioning)"
        );

        let ranked = rank_nearest(VectorMetric::Cosine, &query, &candidates, 10, None);
        let order: Vec<String> = ranked.iter().map(|s| s.entity_id.clone()).collect();

        assert_eq!(
            order,
            vec![
                "item-a".to_string(),
                "item-b".to_string(),
                "item-c".to_string(),
                "item-d".to_string()
            ],
            "seed {seed}: ranking order (score desc, id tiebreak) must be exact"
        );
        // item-e (model m2) must never leak into the m1 ranking.
        assert!(
            !order.iter().any(|id| id == "item-e"),
            "seed {seed}: a different model tag must never be ranked"
        );

        match &reference_order {
            None => reference_order = Some(order),
            Some(reference) => assert_eq!(
                &order, reference,
                "seed {seed}: ranking must be identical to seed 0 (seed-independent)"
            ),
        }
    }
}

#[tokio::test]
async fn dst_nearest_by_reference_excludes_self() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id) = install_deterministic_context(seed);
        let store: BoxedEventStore = BoxedEventStore::new(SimEventStore::no_faults(seed));
        let table = item_table();
        let system = ActorSystem::new("dst-vector-self");

        for (id, embedding, model) in corpus() {
            create_item(&system, &table, &store, id, &embedding, model).await;
        }

        // Rank against item-a's own vector, excluding item-a (the "related to X"
        // shape) — its nearest neighbour is item-b, and item-a is absent.
        let candidates = store
            .vector_candidates("default", "Item", "embed", "m1", 1000)
            .await
            .expect("vector candidates");
        let a_vector = candidates
            .iter()
            .find(|c| c.entity_id == "item-a")
            .expect("item-a present")
            .vector
            .clone();
        let ranked = rank_nearest(
            VectorMetric::Cosine,
            &a_vector,
            &candidates,
            10,
            Some("item-a"),
        );
        let order: Vec<String> = ranked.iter().map(|s| s.entity_id.clone()).collect();
        assert!(
            !order.contains(&"item-a".to_string()),
            "seed {seed}: the reference entity must be excluded from its own results"
        );
        assert_eq!(
            order.first().map(String::as_str),
            Some("item-b"),
            "seed {seed}: item-a's nearest neighbour is item-b"
        );
    }
}

/// ARN-216: the vector backfill must not overwrite a NEWER live co-commit
/// with rows built from a stale load.
///
/// The backfill is two store calls with nothing spanning them: (1) load the
/// entity's state (snapshot/replay at some sequence), (2) reconcile the
/// index to rows parsed from that load. A live write landing between them
/// co-commits the new embedding — and step (2) then clobbers it with the
/// stale one, after which the completion watermark declares the index
/// authoritative. This test executes exactly that interleave.
#[tokio::test]
async fn dst_vector_backfill_must_not_overwrite_newer_live_write() {
    for seed in 0..NUM_SEEDS {
        let (_guard, _clock, _id) = install_deterministic_context(seed);
        let store: BoxedEventStore = BoxedEventStore::new(SimEventStore::no_faults(seed));
        let table = item_table();
        let system = ActorSystem::new("dst-vector-race");
        let entity_id = format!("item-race-{seed}");

        // The entity exists with embedding E1.
        create_item(
            &system,
            &table,
            &store,
            &entity_id,
            &[1.0, 0.0, 0.0, 0.0],
            "m1",
        )
        .await;

        // BACKFILL step 1 (stale load): the rows the backfill would build
        // from a load taken NOW — i.e. E1. (The production code parses these
        // from the replayed state; the parse result is exactly this row.)
        let stale_rows = vec![temper_runtime::persistence::EntityVectorRow {
            decl_name: "embed".to_string(),
            model_tag: "m1".to_string(),
            vector: vec![1.0, 0.0, 0.0, 0.0],
        }];

        // LIVE WRITE lands between the backfill's load and its reconcile:
        // the co-commit updates the index to E2.
        let actor = EntityActor::with_persistence(
            "Item",
            &entity_id,
            table.clone(),
            serde_json::json!({}),
            store.clone(),
            BackendLabel::Sim,
        )
        .with_tenant("default");
        let actor_ref = system.spawn(actor, &format!("{entity_id}-live"));
        let e2 = serde_json::to_string(&[0.0f32, 1.0, 0.0, 0.0]).unwrap();
        let r = dispatch(
            &actor_ref,
            "Reembed",
            serde_json::json!({ "Embedding": e2, "EmbeddingModel": "m1" }),
        )
        .await;
        assert!(r.success, "seed {seed}: Reembed failed: {:?}", r.error);

        // BACKFILL step 2: reconcile with the STALE rows.
        store
            .backfill_entity_vectors("default", "Item", &entity_id, &stale_rows)
            .await
            .expect("backfill reconcile");

        // The index must still hold the newer live embedding E2.
        let candidates = store
            .vector_candidates("default", "Item", "embed", "m1", 1000)
            .await
            .expect("vector candidates");
        let row = candidates
            .iter()
            .find(|c| c.entity_id == entity_id)
            .expect("entity must be indexed");
        assert_eq!(
            row.vector,
            vec![0.0, 1.0, 0.0, 0.0],
            "seed {seed}: a stale backfill reconcile must not overwrite a newer live co-commit"
        );
    }
}
