//! A batch commits or refuses all its rows.

use super::*;

/// Each record's catalog and field-index rows, by record.
async fn record_rows(
    store: &PostgresEventStore,
    pool: &PgPool,
) -> Vec<(String, i64, Option<String>)> {
    sqlx::query_as(&store.schema().qualify_sql(
        "SELECT c.entity_id, c.sequence_nr, f.field_value FROM {schema}entity_catalog c \
         LEFT JOIN {schema}entity_field_index f USING (tenant, entity_type, entity_id) \
         WHERE c.tenant = 't' ORDER BY c.entity_id COLLATE \"C\"",
    ))
    .fetch_all(pool)
    .await
    .unwrap()
}

/// A batch (a composite move, a batch delete) commits every record's rows
/// with its events, as single appends do; a stale member writes nothing of
/// the batch. On main a batch appended only the events and marked each record
/// pending, and its rows were written after the commit.
#[test]
fn a_batch_commits_each_records_rows_with_its_events() {
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        store
            .append_batch(&[
                batched("t:Panel:p2", 0, kept("a2")),
                batched("t:Panel:p1", 0, kept("a1")),
            ])
            .await
            .unwrap();
        let rows = record_rows(&store, &pool).await;
        assert_eq!(
            rows,
            vec![
                ("p1".to_string(), 1, Some("a1".to_string())),
                ("p2".to_string(), 1, Some("a2".to_string())),
            ]
        );
        assert_eq!(count(&store, &pool, "entity_projection_delta").await, 0);

        // p1 is current, p2 is stale: nothing of the batch lands.
        let before = snapshot(&pool, &schema).await;
        let refused = store
            .append_batch(&[
                batched("t:Panel:p1", 1, kept("moved")),
                batched("t:Panel:p2", 0, kept("moved")),
            ])
            .await;
        assert!(
            matches!(
                refused,
                Err(PersistenceError::ConcurrencyViolation {
                    expected: 0,
                    actual: 1
                })
            ),
            "{refused:?}"
        );
        assert_eq!(snapshot(&pool, &schema).await, before);

        // A batch delete removes each record's rows in its commit.
        let mut deleted = batched("t:Panel:p1", 1, Some(AppendProjection::Remove));
        deleted.events = vec![tombstone()];
        store.append_batch(&[deleted]).await.unwrap();
        assert_eq!(
            record_rows(&store, &pool).await,
            vec![("p2".to_string(), 1, Some("a2".to_string()))]
        );
        drop_schema(&pool, &schema).await;
    });
}

/// The journal, projection and reset arm are indivisible even if a later
/// stream refuses the batch. Ending a timer uses the same statement too.
#[test]
fn a_batch_commits_timer_resets_and_ends_or_none_of_its_rows() {
    use temper_runtime::persistence::AppendStateTimer;
    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    sqlx::test_block_on(async {
        let (pool, schema, store) = isolated_store(&url).await;
        let arm = |id: &str, seq: u64| {
            let mut row = Rows::at(seq, serde_json::json!({})).timer;
            row.entity_id = id.into();
            row.started_at = format!("2026-11-02T00:00:{seq:02}Z");
            row.params = format!("{{\"version\":{seq}}}");
            row.armed_by = format!("{{\"agent_id\":\"caller-{seq}\"}}");
            row
        };
        let append = |id: &str, expected, timer| {
            let mut append = batched(&format!("t:Panel:{id}"), expected, kept("app"));
            append.state_timer = timer;
            append
        };
        let a = arm("p1", 1);
        let b = arm("p2", 1);
        store
            .append_batch(&[
                append("p1", 0, Some(AppendStateTimer::Arm(Box::new(a.clone())))),
                append("p2", 0, Some(AppendStateTimer::Arm(Box::new(b.clone())))),
            ])
            .await
            .unwrap();
        assert_eq!(
            {
                let mut rows = store.list_state_timers(TENANT).await.unwrap();
                rows.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));
                rows
            },
            vec![a.clone(), b.clone()]
        );
        let reset = arm("p1", 2);
        let before = snapshot(&pool, &schema).await;
        let refused = store
            .append_batch(&[
                append("p1", 1, Some(AppendStateTimer::Arm(Box::new(reset.clone())))),
                append("p2", 0, Some(AppendStateTimer::End)),
            ])
            .await;
        assert!(matches!(
            refused,
            Err(PersistenceError::ConcurrencyViolation { .. })
        ));
        assert_eq!(snapshot(&pool, &schema).await, before);
        store
            .append_batch(&[
                append("p1", 1, Some(AppendStateTimer::Arm(Box::new(reset.clone())))),
                append("p2", 1, Some(AppendStateTimer::End)),
            ])
            .await
            .unwrap();
        assert_eq!(
            store.list_state_timers(TENANT).await.unwrap(),
            vec![reset.clone()]
        );
        // A subsequent non-resetting append advances the journal, preserving
        // the reset's sequence, deadline, arguments and caller.
        store.append_batch(&[append("p1", 2, None)]).await.unwrap();
        assert_eq!(store.list_state_timers(TENANT).await.unwrap(), vec![reset]);
        store
            .append_batch(&[append("p1", 3, Some(AppendStateTimer::End))])
            .await
            .unwrap();
        assert!(store.list_state_timers(TENANT).await.unwrap().is_empty());
        drop_schema(&pool, &schema).await;
    });
}
