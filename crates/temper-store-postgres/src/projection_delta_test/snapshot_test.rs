//! A snapshot waiting on projection retirement must recheck coverage.
use super::*;
use std::time::Duration;

#[test]
fn snapshot_does_not_restore_a_mark_retired_while_it_waits() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        create(&store, "snapshot-race").await;
        project(&store, "snapshot-race", "other", 1).await;
        store
            .append("t:Payment:snapshot-race", 1, &[created()])
            .await
            .unwrap();
        assert_eq!(candidates(&store, "t").await.1, vec!["snapshot-race"]);

        // Hold the projection's catalog update and retirement uncommitted.
        // mark_snapshot sees sequence 1, then waits on the deleted delta row.
        let mut projection = pool.begin().await.unwrap();
        crate::dbm::postgres_query!(
            &schema,
            "UPDATE {schema}entity_catalog SET sequence_nr = 2 \
             WHERE tenant = 't' AND entity_type = 'Payment' AND entity_id = 'snapshot-race'",
        )
        .execute(&mut *projection)
        .await
        .unwrap();
        crate::projection_delta::applied(
            &schema,
            &mut projection,
            ["t", "Payment", "snapshot-race"],
            2,
            &serde_json::json!({"Id": "snapshot-race", "app": "other"}),
        )
        .await
        .unwrap();

        let mut snapshot = pool.begin().await.unwrap();
        let (pid,): (i32,) = sqlx::query_as("SELECT pg_backend_pid()")
            .fetch_one(&mut *snapshot)
            .await
            .unwrap();
        let snapshot_schema = schema.clone();
        let saving = tokio::spawn(async move {
            crate::projection_delta::mark_snapshot(
                &snapshot_schema,
                &mut snapshot,
                ["t", "Payment", "snapshot-race"],
                2,
            )
            .await
            .unwrap();
            snapshot.commit().await.unwrap();
        });
        // Observe the actual lock wait, rather than guessing with a sleep.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let (blocked,): (bool,) =
                    sqlx::query_as("SELECT cardinality(pg_blocking_pids($1)) > 0")
                        .bind(pid)
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                if blocked {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("snapshot must reach its wait on projection retirement");
        projection.commit().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), saving)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            candidates(&store, "t").await,
            (Vec::new(), Vec::new()),
            "a caught-up nonmatch must not remain pending after the snapshot commits"
        );
        drop_schema(&pool, &schema).await;
    });
}

#[test]
fn snapshot_keeps_marks_that_the_field_index_cannot_vouch_for() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        for id in ["newer", "terminal", "unindexed", "wrapped"] {
            create(&store, id).await;
            project(&store, id, "other", 1).await;
        }
        store
            .append("t:Payment:newer", 1, &[created()])
            .await
            .unwrap();
        store
            .append("t:Payment:terminal", 1, &[tombstone()])
            .await
            .unwrap();
        store
            .upsert_query_projection(
                "t",
                "Payment",
                "wrapped",
                "Draft",
                &serde_json::json!({"fields": {"app": "needle"}}),
                1,
            )
            .await
            .unwrap();
        crate::dbm::postgres_query!(
            &schema,
            "DELETE FROM {schema}entity_field_index WHERE entity_id = 'unindexed'",
        )
        .execute(&pool)
        .await
        .unwrap();
        crate::dbm::postgres_query!(
            &schema,
            "INSERT INTO {schema}entity_projection_delta \
             (tenant, entity_type, entity_id, sequence_nr) VALUES ('t', 'Payment', 'unindexed', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        for id in ["newer", "terminal", "unindexed", "wrapped"] {
            store
                .save_snapshot(&format!("t:Payment:{id}"), 1, b"{}")
                .await
                .unwrap();
        }
        assert_eq!(
            candidates(&store, "t").await.1,
            vec!["newer", "unindexed", "wrapped"]
        );
        project(&store, "terminal", "needle", 1).await;
        assert!(!candidates(&store, "t").await.0.contains(&"terminal".into()));
        drop_schema(&pool, &schema).await;
    });
}
