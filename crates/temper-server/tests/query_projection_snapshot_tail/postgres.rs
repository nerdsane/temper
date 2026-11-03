//! Optional real-PostgreSQL confirmation of the same boot recovery path.

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use temper_runtime::scheduler::{install_deterministic_context, sim_uuid};
use temper_server::EntityState;
use temper_server::storage::{BoxedEventStore, QueryPlaneStore, StorageStack};
use temper_store_postgres::{PostgresEventStore, PostgresSchema, migration};

use super::fixtures::{
    ENTITY_ID, build_state, projected_state, run_isolated, write_snapshot_and_tail,
};

#[test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
fn cold_postgres_backfill_recovers_snapshot_tail() {
    let url = std::env::var("TEMPER_TEST_POSTGRES_URL").expect("disposable PostgreSQL");
    // Isolate every run, including a failed run, from previous smoke fixtures.
    let schema = PostgresSchema::new(format!("snapshot_tail_{}", sim_uuid().simple())).unwrap();
    let checkpoints = run_isolated(async {
        let (_guard, _clock, _ids) = install_deterministic_context(519);
        let store = connect(&url, &schema).await;
        migration::run_migrations_in_schema(store.pool(), &schema)
            .await
            .unwrap();
        let writer = build_state(StorageStack::from_postgres(store.clone()));
        let journal = BoxedEventStore::new(store);
        let a = write_snapshot_and_tail(&writer, &journal, "tenant-a", 2).await;
        let b = write_snapshot_and_tail(&writer, &journal, "tenant-b", 3).await;
        [a, b]
    }); // Destroy all writer actors and background tasks before injecting drift.

    run_isolated(async {
        let (_guard, _clock, _ids) = install_deterministic_context(519);
        let store = connect(&url, &schema).await;
        let journal = BoxedEventStore::new(store.clone());
        for checkpoint in &checkpoints {
            checkpoint.assert_durable_tail(&journal).await;
            store
                .remove_projection(checkpoint.tenant.as_str(), "Order", ENTITY_ID)
                .await
                .unwrap();
        }
        let a = &checkpoints[0];
        let b = &checkpoints[1];
        let snapshot = &a.snapshot;
        store
            .upsert_projection(
                a.tenant.as_str(),
                "Order",
                ENTITY_ID,
                &snapshot.status,
                &snapshot.fields,
                &projected_state(snapshot),
                snapshot.sequence_nr,
            )
            .await
            .unwrap();
        assert_projection(&store, a.tenant.as_str(), snapshot).await;
        let missing = store
            .load_entity_catalog_rows(b.tenant.as_str(), "Order", &[ENTITY_ID.into()])
            .await
            .unwrap()
            .unwrap();
        assert!(missing.is_empty());

        let reader = build_state(StorageStack::from_postgres(store.clone()));
        reader.populate_index_from_store(&a.tenant).await;
        assert!(reader.actor_registry.read().unwrap().is_empty());
        reader.populate_field_index_from_snapshots(&a.tenant).await;
        assert_projection(&store, a.tenant.as_str(), &a.current).await;
        assert_eq!(
            store
                .load_entity_catalog_rows(b.tenant.as_str(), "Order", &[ENTITY_ID.into()])
                .await
                .unwrap()
                .unwrap(),
            missing,
            "tenant-a recovery must not publish tenant-b's same-ID entity"
        );
        reader.populate_index_from_store(&b.tenant).await;
        reader.populate_field_index_from_snapshots(&b.tenant).await;
        assert_eq!(
            store
                .load_entity_catalog_rows(b.tenant.as_str(), "Order", &[ENTITY_ID.into()])
                .await
                .unwrap()
                .unwrap(),
            missing,
            "while serving, the backfill leaves a missing catalog row to boot repair"
        );
        assert_projection(&store, a.tenant.as_str(), &a.current).await;
        assert!(reader.actor_registry.read().unwrap().is_empty());
        eprintln!(
            "PostgreSQL seed=519: stale catalog S={} -> T={}, missing catalog left missing",
            a.snapshot.sequence_nr, a.current.sequence_nr,
        );
        drop(reader);
        sqlx::query(&format!(
            "DROP SCHEMA {} CASCADE",
            schema.qualify_sql("{schema}").trim_end_matches('.')
        ))
        .execute(store.pool())
        .await
        .unwrap();
        store.pool().close().await;
    });
}

async fn connect(url: &str, schema: &PostgresSchema) -> PostgresEventStore {
    let options: PgConnectOptions = url.parse().unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(3)
        .connect_with(options.options([("search_path", "pg_catalog")]))
        .await
        .unwrap();
    PostgresEventStore::with_schema(pool, schema.clone())
}

async fn assert_projection(store: &PostgresEventStore, tenant: &str, expected: &EntityState) {
    let ids = [ENTITY_ID.to_string()];
    let rows = store
        .load_entity_catalog_rows(tenant, "Order", &ids)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(
        row.sequence_nr, expected.sequence_nr,
        "{tenant}: publish T, not S"
    );
    assert_eq!(row.status, expected.status);
    assert_eq!(row.fields, expected.fields);
    let state = row.state.as_ref().expect("full projected state");
    assert_eq!(state["sequence_nr"], expected.sequence_nr);
    assert_eq!(state["fields"], expected.fields);
    assert_eq!(state["counters"]["items"], expected.counters["items"]);
    assert_eq!(state["item_count"], expected.item_count);
    let indexed = store
        .load_query_projection_fields_many(tenant, "Order", &ids, &["ProductId", "Quantity"])
        .await
        .unwrap();
    assert_eq!(indexed.len(), 1);
    assert_eq!(indexed[0].entity_id, ENTITY_ID);
    assert_eq!(
        indexed[0].fields["ProductId"].as_deref(),
        expected.fields["ProductId"].as_str()
    );
    assert_eq!(
        indexed[0].fields["Quantity"],
        Some(expected.fields["Quantity"].to_string())
    );
}
