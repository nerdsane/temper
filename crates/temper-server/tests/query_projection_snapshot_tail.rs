//! Boot recovery must publish snapshot + committed tail, without GET repair.
//! The focused all-entity recovery approach follows PR #350 (rita-aga),
//! head 8da9853f91adf8927bf1bb38c5340d50a7106d3e; this live fixture deliberately
//! does not rely on that PR's changed tombstone enumeration.

#[path = "query_projection_snapshot_tail/fixtures.rs"]
mod fixtures;
#[path = "query_projection_snapshot_tail/postgres.rs"]
mod postgres;

use fixtures::{ENTITY_ID, build_state, projected_state, run_isolated, write_snapshot_and_tail};
use temper_runtime::scheduler::install_deterministic_context;
use temper_server::EntityState;
use temper_server::storage::{BoxedEventStore, StorageStack};
use temper_store_turso::TursoEventStore;

#[derive(Clone, Copy, Debug)]
enum CatalogSeed {
    Missing,
    Stale,
    Newer,
}

#[test]
fn cold_backfill_leaves_a_missing_catalog_row_to_boot_repair() {
    cold_backfill(CatalogSeed::Missing);
}

#[test]
fn cold_backfill_recovers_snapshot_tail_over_stale_catalog() {
    cold_backfill(CatalogSeed::Stale);
}

#[test]
fn cold_backfill_does_not_regress_newer_catalog() {
    cold_backfill(CatalogSeed::Newer);
}

fn cold_backfill(catalog_seed: CatalogSeed) {
    for seed in [519, 520, 521] {
        let directory = tempfile::tempdir().unwrap();
        let url = format!("file:{}", directory.path().join("journal.db").display());
        let checkpoints = run_isolated(async {
            let (_guard, _clock, _ids) = install_deterministic_context(seed);
            let store = TursoEventStore::new(&url, None).await.unwrap();
            let journal = BoxedEventStore::new(store.clone());
            let writer = build_state(StorageStack::from_turso(store));
            let mut checkpoints = Vec::new();
            for (offset, tenant) in ["tenant-a", "tenant-b"].iter().enumerate() {
                checkpoints.push(
                    write_snapshot_and_tail(
                        &writer,
                        &journal,
                        tenant,
                        1 + (seed as usize % 3) + offset,
                    )
                    .await,
                );
            }
            checkpoints
        }); // Writer runtime (including all detached tasks) is now destroyed.

        run_isolated(async {
            let (_guard, _clock, _ids) = install_deterministic_context(seed);
            let store = TursoEventStore::new(&url, None).await.unwrap();
            let journal = BoxedEventStore::new(store.clone());
            let a = &checkpoints[0];
            let b = &checkpoints[1];
            for checkpoint in &checkpoints {
                checkpoint.assert_durable_tail(&journal).await;
                store
                    .remove_query_projection(checkpoint.tenant.as_str(), "Order", ENTITY_ID)
                    .await
                    .unwrap();
            }
            let mut expected = a.current.clone();
            match catalog_seed {
                CatalogSeed::Missing => {
                    assert!(catalog(&store, a.tenant.as_str()).await.is_empty());
                }
                CatalogSeed::Stale => {
                    seed_catalog(&store, a.tenant.as_str(), &a.snapshot).await;
                    assert_projection(&store, a.tenant.as_str(), &a.snapshot).await;
                }
                CatalogSeed::Newer => {
                    expected.sequence_nr += 10;
                    expected.fields["ProductId"] = serde_json::json!("newer-catalog-product");
                    expected.counters.insert("items".into(), 99);
                    expected.item_count = 99;
                    seed_catalog(&store, a.tenant.as_str(), &expected).await;
                }
            }
            seed_catalog(&store, b.tenant.as_str(), &b.snapshot).await;
            let b_before = catalog(&store, b.tenant.as_str()).await;

            let reader = build_state(StorageStack::from_turso(store.clone()));
            reader.populate_index_from_store(&a.tenant).await;
            assert!(reader.actor_registry.read().unwrap().is_empty());
            reader.populate_field_index_from_snapshots(&a.tenant).await;
            assert!(reader.actor_registry.read().unwrap().is_empty());
            eprintln!(
                "seed={seed} catalog={catalog_seed:?} S={} T={}",
                a.snapshot.sequence_nr, a.current.sequence_nr
            );
            assert_a(&store, a.tenant.as_str(), catalog_seed, &expected).await;
            assert_eq!(
                catalog(&store, b.tenant.as_str()).await,
                b_before,
                "tenant-a backfill must not publish tenant-b's same-ID entity"
            );

            reader.populate_index_from_store(&b.tenant).await;
            reader.populate_field_index_from_snapshots(&b.tenant).await;
            assert_projection(&store, b.tenant.as_str(), &b.current).await;
            assert_a(&store, a.tenant.as_str(), catalog_seed, &expected).await;
            // Explicit reruns converge; they must not regress a newer row either.
            reader.populate_field_index_from_snapshots(&a.tenant).await;
            assert_a(&store, a.tenant.as_str(), catalog_seed, &expected).await;
            assert!(reader.actor_registry.read().unwrap().is_empty());
        });
    }
}

async fn seed_catalog(store: &TursoEventStore, tenant: &str, state: &EntityState) {
    store
        .upsert_query_projection_with_state(
            tenant,
            "Order",
            ENTITY_ID,
            &state.status,
            &state.fields,
            &projected_state(state),
            state.sequence_nr,
        )
        .await
        .unwrap();
}

/// While the server serves, the backfill rewrites only rows that exist: a
/// missing one is created before serving by boot repair, or by the record's
/// own commit.
async fn assert_a(
    store: &TursoEventStore,
    tenant: &str,
    catalog_seed: CatalogSeed,
    expected: &EntityState,
) {
    match catalog_seed {
        CatalogSeed::Missing => assert!(
            catalog(store, tenant).await.is_empty(),
            "{tenant}: the backfill leaves a missing catalog row missing"
        ),
        CatalogSeed::Stale | CatalogSeed::Newer => assert_projection(store, tenant, expected).await,
    }
}

async fn catalog(
    store: &TursoEventStore,
    tenant: &str,
) -> Vec<temper_store_turso::store::field_index::EntityCatalogRow> {
    store
        .load_entity_catalog_rows(tenant, "Order", &[ENTITY_ID.to_string()])
        .await
        .unwrap()
}

async fn assert_projection(store: &TursoEventStore, tenant: &str, expected: &EntityState) {
    let rows = catalog(store, tenant).await;
    assert_eq!(
        rows.len(),
        1,
        "{tenant}: catalog must contain the live entity"
    );
    let row = &rows[0];
    assert_eq!(
        row.sequence_nr, expected.sequence_nr,
        "{tenant}: catalog must publish T, not S"
    );
    assert_eq!(row.status, expected.status);
    assert_eq!(row.fields, expected.fields);
    let state = row.state.as_ref().expect("full projected state");
    assert_eq!(state["sequence_nr"], expected.sequence_nr);
    assert_eq!(state["counters"]["items"], expected.counters["items"]);
    assert_eq!(state["item_count"], expected.item_count);
    assert_eq!(state["fields"], expected.fields);
    assert_eq!(state["events"], serde_json::json!([]));
    for field in ["ProductId", "Quantity", "Title"] {
        let value = &expected.fields[field];
        let text = value
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| value.to_string());
        let ids = store
            .query_field_index(
                tenant,
                "Order",
                "entity_id IN (SELECT entity_id FROM entity_field_index \
                 WHERE tenant = ?1 AND entity_type = ?2 AND field_name = ?3 AND field_value = ?4)",
                vec![field.to_string(), text],
            )
            .await
            .unwrap();
        assert_eq!(
            ids,
            [ENTITY_ID],
            "{tenant}: field index {field} must agree with catalog"
        );
    }
    if expected.fields["ProductId"] != "snapshot-product" {
        assert!(
            store
                .query_field_index(
                    tenant,
                    "Order",
                    "entity_id IN (SELECT entity_id FROM entity_field_index \
                     WHERE tenant = ?1 AND entity_type = ?2 AND field_name = ?3 AND field_value = ?4)",
                    vec!["ProductId".into(), "snapshot-product".into()],
                )
                .await
                .unwrap()
                .is_empty(),
            "stale field-index row must be removed"
        );
    }
}
