//! PostgreSQL regression for snapshot baseline changes after coverage publication.

use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_store_postgres::PostgresEventStore;

#[test]
fn same_sequence_snapshot_rewrite_invalidates_published_key_coverage() {
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
        let store = PostgresEventStore::new(pool);
        let tenant = format!("arn238-snapshot-published-{}", sim_uuid());
        let entity_type = "Doc";
        let persistence_id = format!("{tenant}:{entity_type}:snapshot-rewrite");
        let signature = "v4:path";
        let timestamp = sim_now();
        store
            .append(
                &persistence_id,
                0,
                &[PersistenceEnvelope {
                    sequence_nr: 0,
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
            )
            .await
            .expect("seed journal high-water");
        store
            .save_snapshot(&persistence_id, 1, b"before")
            .await
            .expect("seed captured snapshot baseline");
        let revision = store
            .begin_key_index_backfill(&tenant, entity_type, signature)
            .await
            .expect("begin coverage epoch");
        assert!(
            store
                .mark_key_index_backfilled_if_revision(&tenant, entity_type, signature, revision,)
                .await
                .expect("publish coverage")
        );

        store
            .save_snapshot(&persistence_id, 1, b"after")
            .await
            .expect("rewrite snapshot bytes at the journal high-water");

        assert!(
            store
                .key_index_reconciliation_revision(&tenant, entity_type)
                .await
                .expect("read current coverage epoch")
                > revision,
            "changed snapshot baseline bytes must invalidate the published coverage epoch"
        );
        assert!(
            store
                .key_index_backfilled_types(&tenant)
                .await
                .expect("read coverage watermarks")
                .is_empty(),
            "stale ownership rows must not remain authoritative after a snapshot rewrite"
        );
    });
}
