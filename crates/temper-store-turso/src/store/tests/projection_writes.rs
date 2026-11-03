//! When a projection write changes the catalog row, and when it leaves it alone.
//!
//! Every action moves an entity's sequence and event counters, including an
//! action that changes nothing else (a poll). Rewriting the row for those is
//! what a synced database's change log records in full, so a projection whose
//! status, fields and state are the same as the stored row's writes nothing.

use crate::driver::params;

use super::super::{QueryProjectionUpsert, TursoEventStore};
use super::make_store;

const ENTITY_TYPE: &str = "Run";

/// The projected state the server writes for an entity: what the entity holds,
/// plus the counters every action moves (the server leaves the actor's
/// idempotency keys out).
fn actor_state(
    entity_id: &str,
    status: &str,
    fields: &serde_json::Value,
    seq: u64,
) -> serde_json::Value {
    serde_json::json!({
        "entity_type": ENTITY_TYPE,
        "entity_id": entity_id,
        "status": status,
        "item_count": 0,
        "counters": { "polls": 0 },
        "booleans": {},
        "lists": { "steps": ["a", "b"] },
        "fields": fields,
        "events": [],
        "total_event_count": seq,
        "events_since_snapshot": seq % 100,
        "last_snapshot_sequence_nr": seq - seq % 100,
        "sequence_nr": seq,
    })
}

async fn project(
    store: &TursoEventStore,
    tenant: &str,
    entity_id: &str,
    status: &str,
    fields: &serde_json::Value,
    seq: u64,
) {
    store
        .upsert_query_projection_with_state(
            tenant,
            ENTITY_TYPE,
            entity_id,
            status,
            fields,
            &actor_state(entity_id, status, fields, seq),
            seq,
        )
        .await
        .expect("projection upsert");
}

fn batch_entry(
    entity_id: &str,
    status: &str,
    fields: &serde_json::Value,
    seq: u64,
) -> QueryProjectionUpsert {
    QueryProjectionUpsert {
        entity_type: ENTITY_TYPE.to_string(),
        entity_id: entity_id.to_string(),
        status: status.to_string(),
        fields: fields.clone(),
        state: actor_state(entity_id, status, fields, seq),
        indexed_fields: fields.clone(),
        sequence_nr: seq,
        known_new: false,
    }
}

/// The stored row as `(sequence_nr, updated_at, status, fields, state)`.
async fn stored(
    store: &TursoEventStore,
    tenant: &str,
    entity_id: &str,
) -> (i64, String, String, serde_json::Value, serde_json::Value) {
    let conn = store.connection().await.expect("connection");
    let mut rows = conn
        .query(
            "SELECT sequence_nr, updated_at, status, fields, state FROM entity_catalog \
             INDEXED BY sqlite_autoindex_entity_catalog_1 \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, ENTITY_TYPE, entity_id],
        )
        .await
        .expect("query catalog row");
    let row = rows
        .next()
        .await
        .expect("read catalog row")
        .expect("catalog row exists");
    let json = |index: usize| -> serde_json::Value {
        serde_json::from_str(&row.get::<String>(index).expect("json column")).expect("json")
    };
    (
        row.get(0).expect("sequence_nr"),
        row.get(1).expect("updated_at"),
        row.get(2).expect("status"),
        json(3),
        json(4),
    )
}

async fn title_index_rowid(store: &TursoEventStore, tenant: &str, entity_id: &str) -> i64 {
    let conn = store.connection().await.expect("connection");
    let mut rows = conn
        .query(
            "SELECT rowid FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3 AND field_name = 'Title'",
            params![tenant, ENTITY_TYPE, entity_id],
        )
        .await
        .expect("query title row");
    rows.next()
        .await
        .expect("read title row")
        .expect("title row exists")
        .get(0)
        .expect("rowid")
}

fn fields(title: &str, log: &[&str]) -> serde_json::Value {
    serde_json::json!({ "Title": title, "Owner": "alice", "Log": log })
}

#[tokio::test]
async fn an_action_that_changes_nothing_leaves_the_catalog_row_untouched() {
    let store = make_store("projection-unchanged-single").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let run = "run-polled";
    let content = fields("Waiting", &["started"]);

    project(&store, &tenant, run, "Waiting", &content, 7).await;
    let written = stored(&store, &tenant, run).await;
    let index_row = title_index_rowid(&store, &tenant, run).await;

    for seq in 8..=20 {
        project(&store, &tenant, run, "Waiting", &content, seq).await;
    }

    assert_eq!(
        stored(&store, &tenant, run).await,
        written,
        "polls that change nothing must not rewrite the row"
    );
    assert_eq!(title_index_rowid(&store, &tenant, run).await, index_row);
}

#[tokio::test]
async fn a_change_to_a_nested_field_or_the_state_still_lands() {
    let store = make_store("projection-nested-change").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let run = "run-nested";

    project(
        &store,
        &tenant,
        run,
        "Waiting",
        &fields("Waiting", &["started"]),
        7,
    )
    .await;
    project(
        &store,
        &tenant,
        run,
        "Waiting",
        &fields("Waiting", &["started"]),
        8,
    )
    .await;
    // Only a list inside the fields changes; the scalar fields do not.
    let grown = fields("Waiting", &["started", "checked"]);
    project(&store, &tenant, run, "Waiting", &grown, 9).await;

    let (seq, _, status, stored_fields, state) = stored(&store, &tenant, run).await;
    assert_eq!(seq, 9);
    assert_eq!(status, "Waiting");
    assert_eq!(stored_fields, grown);
    assert_eq!(
        state.get("fields"),
        None,
        "the stored state does not repeat the fields the fields column holds"
    );
    assert_eq!(state["sequence_nr"], 9);
}

#[tokio::test]
async fn an_older_projection_arriving_after_an_unchanged_newer_one_is_refused() {
    // Projection writes for one entity come from several places (the queue,
    // read repair, backfill) and can land out of order. A newer write that
    // changed nothing is still newer: an older one with other content must
    // not replace the row after it.
    let store = make_store("projection-unchanged-ordering").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let run = "run-out-of-order";
    let back = fields("Waiting", &["started"]);
    let between = fields("Checking", &["started"]);

    project(&store, &tenant, run, "Waiting", &back, 10).await;
    project(&store, &tenant, run, "Waiting", &back, 12).await;
    project(&store, &tenant, run, "Checking", &between, 11).await;

    let (_, _, status, stored_fields, _) = stored(&store, &tenant, run).await;
    assert_eq!(status, "Waiting", "the row holds the newest content");
    assert_eq!(stored_fields, back);

    // The same through the batch path.
    let batched = "run-out-of-order-batch";
    store
        .upsert_query_projections(&tenant, &[batch_entry(batched, "Waiting", &back, 10)])
        .await
        .expect("batch upsert");
    store
        .upsert_query_projections(&tenant, &[batch_entry(batched, "Waiting", &back, 12)])
        .await
        .expect("batch upsert");
    store
        .upsert_query_projections(&tenant, &[batch_entry(batched, "Checking", &between, 11)])
        .await
        .expect("batch upsert");
    let (_, _, status, stored_fields, _) = stored(&store, &tenant, batched).await;
    assert_eq!(status, "Waiting");
    assert_eq!(stored_fields, back);

    // A newer change after the unchanged ones still lands.
    project(&store, &tenant, run, "Done", &between, 13).await;
    let (seq, _, status, _, _) = stored(&store, &tenant, run).await;
    assert_eq!((seq, status.as_str()), (13, "Done"));
}

#[tokio::test]
async fn a_batch_of_unchanged_projections_leaves_the_catalog_rows_untouched() {
    let store = make_store("projection-unchanged-batch").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let content = fields("Waiting", &["started"]);

    store
        .upsert_query_projections(
            &tenant,
            &[
                batch_entry("run-a", "Waiting", &content, 3),
                batch_entry("run-b", "Waiting", &content, 5),
            ],
        )
        .await
        .expect("batch upsert");
    let a = stored(&store, &tenant, "run-a").await;
    let b = stored(&store, &tenant, "run-b").await;

    for step in 1..=5 {
        store
            .upsert_query_projections(
                &tenant,
                &[
                    batch_entry("run-a", "Waiting", &content, 3 + step),
                    batch_entry("run-b", "Waiting", &content, 5 + step),
                ],
            )
            .await
            .expect("batch upsert");
    }

    assert_eq!(stored(&store, &tenant, "run-a").await, a);
    assert_eq!(stored(&store, &tenant, "run-b").await, b);
}

#[tokio::test]
async fn a_catalog_read_reports_the_newest_unchanged_projection() {
    // The row is not rewritten, but what a read serves (the sequence and the
    // event counters in the state) is what the skipped write would have
    // stored, so a client comparing or ordering by them sees the actor's.
    let store = make_store("projection-unchanged-read").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let run = "run-read";
    let content = fields("Waiting", &["started"]);
    project(&store, &tenant, run, "Waiting", &content, 7).await;
    for seq in 8..=20 {
        project(&store, &tenant, run, "Waiting", &content, seq).await;
    }
    let batched = "run-read-batch";
    for seq in 3..=9 {
        store
            .upsert_query_projections(&tenant, &[batch_entry(batched, "Waiting", &content, seq)])
            .await
            .expect("batch upsert");
    }

    let rows = store
        .load_entity_catalog_rows(
            &tenant,
            ENTITY_TYPE,
            &[run.to_string(), batched.to_string()],
        )
        .await
        .expect("catalog rows");
    assert_eq!(rows.len(), 2);
    // Ordered by entity id.
    let (row, batched_row) = (&rows[0], &rows[1]);
    assert_eq!(row.entity_id, run);
    assert_eq!(row.sequence_nr, 20);
    assert_eq!(row.fields, content);
    assert_eq!(
        row.state.as_ref().expect("state"),
        &actor_state(run, "Waiting", &content, 20)
    );
    assert_eq!(batched_row.sequence_nr, 9);
    assert_eq!(
        batched_row.state.as_ref().expect("state"),
        &actor_state(batched, "Waiting", &content, 9)
    );

    let exported = store
        .export_query_projections(Some(&tenant))
        .await
        .expect("export");
    let sequences = exported
        .iter()
        .map(|row| (row.entity_id.as_str(), row.sequence_nr))
        .collect::<Vec<_>>();
    assert_eq!(sequences, [(run, 20), (batched, 9)]);
}

#[tokio::test]
async fn a_batch_keeps_only_each_entitys_newest_projection() {
    let store = make_store("projection-repeated-batch").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let original = fields("Waiting", &["started"]);
    let intermediate = fields("Checking", &["started"]);

    for (run, reverse) in [("forward", false), ("reverse", true)] {
        project(&store, &tenant, run, "Waiting", &original, 10).await;
        let written = stored(&store, &tenant, run).await;
        let index_row = title_index_rowid(&store, &tenant, run).await;
        let mut batch = vec![
            batch_entry(run, "Checking", &intermediate, 11),
            batch_entry(run, "Waiting", &original, 12),
        ];
        if reverse {
            batch.reverse();
        }
        store
            .upsert_query_projections(&tenant, &batch)
            .await
            .expect("batch upsert");

        assert_eq!(stored(&store, &tenant, run).await, written);
        assert_eq!(title_index_rowid(&store, &tenant, run).await, index_row);
        let rows = store
            .load_entity_catalog_rows(&tenant, ENTITY_TYPE, &[run.to_string()])
            .await
            .expect("catalog rows");
        assert_eq!(rows[0].sequence_nr, 12);
        assert_eq!(rows[0].fields, original);
        assert_eq!(
            rows[0].state.as_ref().expect("state"),
            &actor_state(run, "Waiting", &original, 12)
        );
    }
}

#[tokio::test]
async fn committed_projections_keep_unchanged_counters_and_remove_them_with_the_row() {
    use temper_runtime::persistence::{CommitRows, EventStore, ProjectionRow};

    let store = make_store("committed-projection-unchanged").await;
    let tenant = "tenant-committed";
    let run = "run-polled";
    let journal = format!("{tenant}:{ENTITY_TYPE}:{run}");
    let content = fields("Waiting", &["started"]);
    let mut first = None;
    for seq in 1..=2 {
        let state = actor_state(run, "Waiting", &content, seq);
        store
            .append_with_rows(
                &journal,
                seq - 1,
                &[super::test_envelope("Poll", serde_json::json!({}))],
                CommitRows {
                    projection: Some(ProjectionRow::Upsert {
                        status: "Waiting",
                        fields: &content,
                        state: &state,
                    }),
                    ..CommitRows::default()
                },
            )
            .await
            .expect("commit event and projection");
        if seq == 1 {
            first = Some(stored(&store, tenant, run).await);
        }
    }
    assert_eq!(
        stored(&store, tenant, run).await,
        first.unwrap(),
        "unchanged committed projection leaves the catalog untouched"
    );
    let rows = store
        .load_entity_catalog_rows(tenant, ENTITY_TYPE, &[run.to_string()])
        .await
        .unwrap();
    assert_eq!(rows[0].sequence_nr, 2);
    assert_eq!(rows[0].state.as_ref().unwrap()["sequence_nr"], 2);
    store
        .append_with_rows(
            &journal,
            2,
            &[super::test_envelope("Delete", serde_json::json!({}))],
            CommitRows {
                projection: Some(ProjectionRow::Remove),
                ..CommitRows::default()
            },
        )
        .await
        .expect("commit removal");
    let conn = store.connection().await.unwrap();
    let mut rows = conn
        .query(
            "SELECT COUNT(*) FROM entity_catalog_counters WHERE tenant = ?1",
            params![tenant],
        )
        .await
        .unwrap();
    assert_eq!(
        rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
        0
    );
    assert!(
        store
            .load_entity_catalog_rows(tenant, ENTITY_TYPE, &[run.to_string()])
            .await
            .unwrap()
            .is_empty()
    );
}
