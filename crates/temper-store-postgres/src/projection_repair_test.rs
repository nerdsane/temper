use crate::{PostgresEventStore, PostgresSchema, migration::run_migrations_in_schema};
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};

#[test]
fn boot_comparison_uses_the_selected_postgres_schema_and_only_lagging_rows() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let pool = sqlx::PgPool::connect(&url).await.unwrap();
        let schema = PostgresSchema::new(format!("repair.\"{}", uuid::Uuid::new_v4())).unwrap();
        run_migrations_in_schema(&pool, &schema).await.unwrap();
        let store = PostgresEventStore::with_schema(pool.clone(), schema.clone());
        let event = || PersistenceEnvelope {
            sequence_nr: 0,
            event_type: "Touch".to_string(),
            payload: serde_json::json!({}),
            metadata: EventMetadata {
                event_id: uuid::Uuid::new_v4(),
                causation_id: uuid::Uuid::new_v4(),
                correlation_id: uuid::Uuid::new_v4(),
                timestamp: chrono::Utc::now(),
                actor_id: "repair-test".to_string(),
            },
        };
        for tenant in ["installed-app", "another-app"] {
            for (ty, id, projected, seq) in [
                ("Order", "current", 2, 2),
                ("Order", "behind", 1, 3),
                ("Other", "missing", 0, 1),
                ("Order", "snapshot", 1, 1),
            ] {
                let pid = format!("{tenant}:{ty}:{id}");
                let events: Vec<_> = (0..seq).map(|_| event()).collect();
                store.append(&pid, 0, &events).await.unwrap();
                if projected != 0 {
                    store
                        .upsert_query_projection_with_state(
                            tenant,
                            ty,
                            id,
                            "Reviewing",
                            &serde_json::json!({}),
                            &serde_json::json!({}),
                            projected,
                        )
                        .await
                        .unwrap();
                }
            }
            store
                .save_snapshot(&format!("{tenant}:Order:snapshot"), 5, b"{}")
                .await
                .unwrap();
        }
        let gaps = store
            .lagging_query_projections("installed-app")
            .await
            .unwrap();
        let observed: Vec<_> = gaps
            .iter()
            .map(|g| {
                (
                    g.journal.entity_type.as_str(),
                    g.journal.entity_id.as_str(),
                    g.projection.as_ref().map(|p| p.sequence_nr),
                    g.journal.sequence_nr,
                )
            })
            .collect();
        assert_eq!(
            observed,
            vec![
                ("Order", "behind", Some(1), 3),
                ("Order", "snapshot", Some(1), 5),
                ("Other", "missing", None, 1)
            ]
        );
        assert_eq!(
            store
                .load_journal_heads("installed-app")
                .await
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            store
                .query_projection_checkpoints("installed-app")
                .await
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            store
                .latest_sequence_nr("installed-app:Order:snapshot")
                .await
                .unwrap(),
            5
        );
        assert_eq!(
            store
                .latest_sequence_nr("installed-app:Order:behind")
                .await
                .unwrap(),
            3
        );
        assert_eq!(
            store
                .latest_sequence_nr("installed-app:Order:absent")
                .await
                .unwrap(),
            0
        );
        sqlx::query(&format!(
            "DROP SCHEMA {} CASCADE",
            schema.prefix().trim_end_matches('.')
        ))
        .execute(&pool)
        .await
        .unwrap();
    });
}
