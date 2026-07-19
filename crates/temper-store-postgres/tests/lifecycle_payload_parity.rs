//! PostgreSQL lifecycle classification must match the Rust event predicate.

use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_store_postgres::PostgresEventStore;

#[test]
fn legacy_deleted_name_in_array_payload_remains_terminal() {
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
        let tenant = format!("arn238-lifecycle-array-{}", sim_uuid());
        let persistence_id = format!("{tenant}:Doc:legacy-array-delete");
        let timestamp = sim_now();

        store
            .append(
                &persistence_id,
                0,
                &[PersistenceEnvelope {
                    sequence_nr: 0,
                    event_type: "Deleted".to_string(),
                    payload: serde_json::json!(["to_status"]),
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
            .expect("append legacy tombstone with array payload");

        let entity_ids = store
            .list_entity_ids_by_type(&tenant, "Doc")
            .await
            .expect("list live entities");
        assert!(
            entity_ids.is_empty(),
            "top-level array membership is not structured to_status metadata"
        );
    });
}
