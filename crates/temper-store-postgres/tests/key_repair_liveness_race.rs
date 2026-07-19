//! Regression for an orphan repair candidate becoming live after enumeration.

use temper_runtime::persistence::{
    EntityKeyRow, EventMetadata, EventStore, IndexReconciliation, KeyIndexBackfillFence,
    PersistenceEnvelope,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_store_postgres::PostgresEventStore;

#[test]
fn stale_orphan_classification_cannot_delete_a_concurrent_live_claim() {
    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };
    sqlx::test_block_on(async {
        let pool = sqlx::PgPool::connect(&database_url)
            .await
            .expect("connect to Postgres");
        temper_store_postgres::migration::run_migrations(&pool)
            .await
            .expect("run Postgres migrations");
        let store = PostgresEventStore::new(pool.clone());
        let tenant = format!("arn238-key-race-{}", sim_uuid());
        let entity_type = "Doc";
        let entity_id = "orphan-becomes-live";
        let key = EntityKeyRow {
            key_name: "path".to_string(),
            key_hash: format!("path-{}", sim_uuid()),
        };
        let key_set_signature = "v3:path";

        sqlx::query(
            "INSERT INTO entity_key_index \
         (tenant, entity_type, key_name, key_hash, entity_id, sequence_nr) \
         VALUES ($1, $2, $3, $4, $5, 0)",
        )
        .bind(&tenant)
        .bind(entity_type)
        .bind(&key.key_name)
        .bind(&key.key_hash)
        .bind(entity_id)
        .execute(&pool)
        .await
        .expect("seed key-only orphan");

        let repair_revision = store
            .begin_key_index_backfill(&tenant, entity_type, key_set_signature)
            .await
            .expect("begin repair contract");
        assert_eq!(
            store
                .list_entity_ids_for_key_reconciliation(&tenant, entity_type)
                .await
                .expect("enumerate repair candidates"),
            vec![entity_id.to_string()]
        );
        assert!(
            store
                .list_entity_ids_by_type(&tenant, entity_type)
                .await
                .expect("classify live entities")
                .is_empty(),
            "precondition: the key-only repair candidate is initially not live"
        );

        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let timestamp = sim_now();
        store
            .append_with_index_rows(
                &persistence_id,
                0,
                &[PersistenceEnvelope {
                    sequence_nr: 1,
                    event_type: "Create".to_string(),
                    payload: serde_json::json!({}),
                    metadata: EventMetadata {
                        event_id: sim_uuid(),
                        causation_id: sim_uuid(),
                        correlation_id: sim_uuid(),
                        timestamp,
                        actor_id: persistence_id.clone(),
                    },
                }],
                std::slice::from_ref(&key),
                &[],
                IndexReconciliation {
                    keys: true,
                    key_set_signature: Some(key_set_signature.to_string()),
                    vectors: false,
                },
            )
            .await
            .expect("same-contract create after repair enumeration");

        let stale_repair = store
            .backfill_entity_keys(
                &tenant,
                entity_type,
                entity_id,
                1,
                KeyIndexBackfillFence {
                    key_set_signature,
                    contract_revision: repair_revision,
                },
                &[],
            )
            .await;

        assert!(
            stale_repair.is_err(),
            "the exact repair must reject the stale non-live classification"
        );
        assert_eq!(
            store
                .lookup_by_key(&tenant, entity_type, &key.key_name, &key.key_hash)
                .await
                .expect("lookup concurrent live claim"),
            Some(entity_id.to_string()),
            "a stale repair must preserve the co-committed live key"
        );
    });
}
