//! Upgrade regression for exact key/vector projection reconciliation.
//!
//! Older binaries could watermark a projection while a deleted journal or an
//! indexed-only phantom still retained derived rows. A restart on the fixed binary
//! must invalidate that legacy watermark, enumerate both authoritative journals and
//! projection rows, and purge everything that replay says is absent.

use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EntityKeyRow, EntityVectorRow, EventStore};
use temper_runtime::tenant::TenantId;
use temper_server::key_index::canonical_key_hash;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_store_sim::SimEventStore;

#[path = "support/projection_legacy.rs"]
mod projection_legacy_support;
use projection_legacy_support::{ITEM_IOA, build_registry, build_registry_from_source};

fn build_registry_without_projections() -> temper_server::registry::SpecRegistry {
    let source = ITEM_IOA
        .replace(
            "[[key]]\nname = \"slug\"\nproperties = [\"Slug\"]\n\n",
            "",
        )
        .replace(
            "[[vector]]\nname = \"embed\"\nproperty = \"Embedding\"\nmodel_property = \"EmbeddingModel\"\ndims = 4\nmetric = \"cosine\"\n\n",
            "",
        );
    build_registry_from_source(&source)
}

fn build_state_with_store(store: SimEventStore, actor_system_name: &str) -> ServerState {
    let registry = build_registry();
    let mut state = ServerState::from_registry(ActorSystem::new(actor_system_name), registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state
}

fn build_state() -> (ServerState, SimEventStore) {
    let store = SimEventStore::no_faults(189);
    let state = build_state_with_store(store.clone(), "legacy-projection");
    (state, store)
}

async fn seed_live_item(state: &ServerState, id: &str, slug: &str) {
    let tenant = TenantId::default();
    let context = AgentContext::for_service("legacy-projection-test");
    let create = state
        .dispatch_tenant_action(
            &tenant,
            "Item",
            id,
            "Create",
            serde_json::json!({
                "Slug": slug,
                "Embedding": "[1,0,0,0]",
                "EmbeddingModel": "m1"
            }),
            &context,
        )
        .await
        .expect("create dispatch");
    assert!(create.success, "create failed: {:?}", create.error);
}

async fn seed_deleted_item(state: &ServerState) {
    seed_live_item(state, "dead", "dead-slug").await;
    let tenant = TenantId::default();
    let context = AgentContext::for_service("legacy-projection-test");
    let delete = state
        .dispatch_tenant_action(
            &tenant,
            "Item",
            "dead",
            "Delete",
            serde_json::json!({}),
            &context,
        )
        .await
        .expect("delete dispatch");
    assert!(delete.success, "delete failed: {:?}", delete.error);
}

fn slug_hash(slug: &str) -> String {
    let fields = serde_json::Map::from_iter([("Slug".to_string(), serde_json::json!(slug))]);
    canonical_key_hash("slug", &["Slug".to_string()], &fields).expect("complete slug key")
}

async fn seed_stale_keys(store: &SimEventStore) {
    for (entity_id, slug) in [("dead", "dead-slug"), ("phantom", "phantom-slug")] {
        store
            .backfill_entity_keys(
                "default",
                "Item",
                entity_id,
                &[EntityKeyRow {
                    key_name: "slug".to_string(),
                    key_hash: slug_hash(slug),
                }],
            )
            .await
            .expect("seed historical key row");
    }
    // Exact signature written by pre-fix binaries.
    store
        .mark_key_index_backfilled("default", "Item", "slug")
        .await
        .expect("seed legacy key watermark");
}

async fn seed_stale_vectors(store: &SimEventStore) {
    for entity_id in ["dead", "phantom"] {
        store
            .backfill_entity_vectors(
                "default",
                "Item",
                entity_id,
                0,
                &[EntityVectorRow {
                    decl_name: "embed".to_string(),
                    model_tag: "m1".to_string(),
                    vector: vec![1.0, 0.0, 0.0, 0.0],
                }],
            )
            .await
            .expect("seed historical vector row");
    }
    // Exact signature written by pre-fix binaries.
    store
        .mark_vector_index_backfilled("default", "Item", "embed:Embedding:EmbeddingModel:4:cosine")
        .await
        .expect("seed legacy vector watermark");
}

#[tokio::test]
async fn removed_declarations_retire_watermarks_before_identical_readd() {
    let store = SimEventStore::no_faults(189);
    let tenant = TenantId::default();
    let declared = build_state_with_store(store.clone(), "projection-declared");
    seed_live_item(&declared, "live", "old-slug").await;
    declared.populate_key_index_from_snapshots(&tenant).await;
    declared.populate_vector_index_from_snapshots(&tenant).await;

    let mut absent = ServerState::from_registry(
        ActorSystem::new("projection-absent"),
        build_registry_without_projections(),
    );
    absent.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    absent.populate_key_index_from_snapshots(&tenant).await;
    absent.populate_vector_index_from_snapshots(&tenant).await;
    assert_eq!(
        store
            .key_index_backfilled_types("default")
            .await
            .expect("retired key watermark"),
        vec![("Item".to_string(), "v2|[]".to_string())]
    );
    assert_eq!(
        store
            .vector_index_backfilled_types("default")
            .await
            .expect("retired vector watermark"),
        vec![("Item".to_string(), "v2|[]".to_string())]
    );

    let context = AgentContext::for_service("projection-declaration-gap-test");
    let changed = absent
        .dispatch_tenant_action(
            &tenant,
            "Item",
            "live",
            "Change",
            serde_json::json!({
                "Slug": "new-slug",
                "Embedding": "[0,1,0,0]",
                "EmbeddingModel": "m1"
            }),
            &context,
        )
        .await
        .expect("write while projections are undeclared");
    assert!(changed.success, "gap write failed: {:?}", changed.error);
    assert_eq!(
        store
            .lookup_by_key("default", "Item", "slug", &slug_hash("old-slug"))
            .await
            .expect("stale key during declaration gap")
            .as_deref(),
        Some("live"),
        "undeclared writes intentionally do not maintain a removed projection"
    );

    let readded = build_state_with_store(store.clone(), "projection-readded");
    readded.populate_key_index_from_snapshots(&tenant).await;
    readded.populate_vector_index_from_snapshots(&tenant).await;

    assert_eq!(
        store
            .lookup_by_key("default", "Item", "slug", &slug_hash("old-slug"))
            .await
            .expect("old key after re-add"),
        None
    );
    assert_eq!(
        store
            .lookup_by_key("default", "Item", "slug", &slug_hash("new-slug"))
            .await
            .expect("new key after re-add")
            .as_deref(),
        Some("live")
    );
    let candidates = store
        .vector_candidates("default", "Item", "embed", "m1", 10)
        .await
        .expect("vectors after re-add");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].entity_id, "live");
    assert_eq!(candidates[0].vector, vec![0.0, 1.0, 0.0, 0.0]);
    assert!(
        store
            .key_index_backfilled_types("default")
            .await
            .expect("re-added key watermark")[0]
            .1
            != "v2|[]"
    );
    assert!(
        store
            .vector_index_backfilled_types("default")
            .await
            .expect("re-added vector watermark")[0]
            .1
            != "v2|[]"
    );
}

#[tokio::test]
async fn legacy_key_watermark_reconciles_tombstone_and_index_only_rows() {
    let (state, store) = build_state();
    let tenant = TenantId::default();
    seed_deleted_item(&state).await;
    seed_stale_keys(&store).await;

    state.populate_key_index_from_snapshots(&tenant).await;

    for slug in ["dead-slug", "phantom-slug"] {
        assert_eq!(
            store
                .lookup_by_key("default", "Item", "slug", &slug_hash(slug))
                .await
                .expect("key lookup"),
            None,
            "replay-absent entity must not retain key {slug} after upgrade"
        );
    }

    seed_live_item(&state, "replacement", "dead-slug").await;
    assert_eq!(
        store
            .lookup_by_key("default", "Item", "slug", &slug_hash("dead-slug"))
            .await
            .expect("replacement key lookup")
            .as_deref(),
        Some("replacement"),
        "a live entity must be able to reclaim the historical tombstone's released key"
    );
}

#[tokio::test]
async fn legacy_key_reconciliation_releases_stale_holder_before_assigning_live_key() {
    let (state, store) = build_state();
    let tenant = TenantId::default();
    let hash = slug_hash("shared");
    seed_live_item(&state, "a-live", "shared").await;

    // Recreate the pre-upgrade conflict: the live entity has no row while a
    // projection-only phantom holds the key replay says belongs to `a-live`.
    store
        .backfill_entity_keys("default", "Item", "a-live", &[])
        .await
        .expect("remove live row");
    store
        .backfill_entity_keys(
            "default",
            "Item",
            "z-phantom",
            &[EntityKeyRow {
                key_name: "slug".to_string(),
                key_hash: hash.clone(),
            }],
        )
        .await
        .expect("seed stale holder");
    store
        .mark_key_index_backfilled("default", "Item", "slug")
        .await
        .expect("seed legacy key watermark");

    state.populate_key_index_from_snapshots(&tenant).await;

    assert_eq!(
        store
            .lookup_by_key("default", "Item", "slug", &hash)
            .await
            .expect("key lookup")
            .as_deref(),
        Some("a-live"),
        "stale holders must be purged before current live keys are assigned"
    );
}

#[tokio::test]
async fn overlapping_key_reconcilers_observe_the_current_watermark_under_one_fence() {
    let (state_a, store) = build_state();
    let state_b = build_state_with_store(store.clone(), "legacy-projection-peer");
    let tenant = TenantId::default();
    let hash = slug_hash("shared");
    seed_live_item(&state_a, "a-live", "shared").await;
    store
        .backfill_entity_keys("default", "Item", "a-live", &[])
        .await
        .expect("remove live row");
    store
        .backfill_entity_keys(
            "default",
            "Item",
            "z-phantom",
            &[EntityKeyRow {
                key_name: "slug".to_string(),
                key_hash: hash.clone(),
            }],
        )
        .await
        .expect("seed stale holder");
    store
        .mark_key_index_backfilled("default", "Item", "slug")
        .await
        .expect("seed legacy key watermark");

    // Both workers begin with independent empty caches. The first yields during
    // replay while holding the store fence; the peer must wait, then re-read the
    // durable v2 watermark and skip rather than acting on legacy coverage.
    tokio::join!(
        state_a.populate_key_index_from_snapshots(&tenant),
        state_b.populate_key_index_from_snapshots(&tenant),
    );

    assert_eq!(
        store
            .lookup_by_key("default", "Item", "slug", &hash)
            .await
            .expect("key lookup")
            .as_deref(),
        Some("a-live"),
        "an overlapping worker must not purge rows covered by the current watermark"
    );
    let watermarks = store
        .key_index_backfilled_types("default")
        .await
        .expect("durable key watermark");
    assert_eq!(watermarks.len(), 1);
    assert!(watermarks[0].1.starts_with("v2|"));
}

#[tokio::test]
async fn unreadable_current_key_watermark_aborts_before_projection_mutation() {
    let (state, store) = build_state();
    let tenant = TenantId::default();
    let hash = slug_hash("live");
    seed_live_item(&state, "live", "live").await;
    state.populate_key_index_from_snapshots(&tenant).await;
    let before_watermarks = store
        .key_index_backfilled_types("default")
        .await
        .expect("established key watermark");
    assert!(before_watermarks[0].1.starts_with("v2|"));

    let restarted = build_state_with_store(store.clone(), "legacy-projection-restarted");
    store.fail_next_key_watermark_reads("default", 1);
    assert!(
        !restarted.reconcile_declared_projections(&tenant).await,
        "strict generation reconciliation must surface an unreadable key watermark"
    );

    assert_eq!(
        store
            .lookup_by_key("default", "Item", "slug", &hash)
            .await
            .expect("key lookup")
            .as_deref(),
        Some("live"),
        "a watermark read error must abort before the existing projection is touched"
    );
    assert_eq!(
        store
            .key_index_backfilled_types("default")
            .await
            .expect("watermark after injected failure"),
        before_watermarks,
        "an unreadable watermark must not be replaced or reinterpreted as absent"
    );
}

#[tokio::test]
async fn unreadable_current_vector_watermark_aborts_before_projection_mutation() {
    let (state, store) = build_state();
    let tenant = TenantId::default();
    seed_live_item(&state, "live", "live").await;
    state.populate_vector_index_from_snapshots(&tenant).await;
    let before_candidates = store
        .vector_candidates("default", "Item", "embed", "m1", 10)
        .await
        .expect("established vector projection");
    let before_watermarks = store
        .vector_index_backfilled_types("default")
        .await
        .expect("established vector watermark");
    assert_eq!(before_candidates.len(), 1);
    assert!(before_watermarks[0].1.starts_with("v2|"));

    let restarted = build_state_with_store(store.clone(), "legacy-vector-restarted");
    store.fail_next_vector_watermark_reads("default", 1);
    assert!(
        !restarted.reconcile_declared_projections(&tenant).await,
        "strict generation reconciliation must surface an unreadable vector watermark"
    );

    assert_eq!(
        store
            .vector_candidates("default", "Item", "embed", "m1", 10)
            .await
            .expect("vector projection after injected failure"),
        before_candidates,
        "a watermark read error must abort before the existing vector projection is touched"
    );
    assert_eq!(
        store
            .vector_index_backfilled_types("default")
            .await
            .expect("vector watermark after injected failure"),
        before_watermarks,
        "an unreadable vector watermark must not be replaced or reinterpreted as absent"
    );
}

#[tokio::test]
async fn legacy_vector_watermark_reconciles_tombstone_and_index_only_rows() {
    let (state, store) = build_state();
    let tenant = TenantId::default();
    seed_deleted_item(&state).await;
    seed_stale_vectors(&store).await;

    state.populate_vector_index_from_snapshots(&tenant).await;

    assert!(
        store
            .vector_candidates("default", "Item", "embed", "m1", 10)
            .await
            .expect("vector candidates")
            .is_empty(),
        "replay-absent tombstone and phantom vectors must be purged after upgrade"
    );
}
