//! Migration coverage uses the real worker and native SQLite engine.
use std::time::Duration;

use super::super::first_event::BATCH_HOLD;
use super::*;

async fn complete(store: &TursoEventStore) -> (i64, i64) {
    for _ in 0..1000 {
        let conn = store.connection().await.unwrap();
        let mut rows = conn
            .query(
                "SELECT records, null_records FROM first_event_backfill WHERE complete = 1",
                (),
            )
            .await
            .unwrap();
        if let Some(row) = rows.next().await.unwrap() {
            return (row.get(0).unwrap(), row.get(1).unwrap());
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("creation migration did not finish");
}

#[tokio::test]
async fn first_event_backfill_is_bounded_resumable_and_never_uses_a_retained_tail() {
    let url = sqlite_test_url("first-event-backfill");
    let store = TursoEventStore::new(&url, None).await.unwrap();
    assert_eq!(complete(&store).await, (0, 0));
    let conn = store
        .write_connection("test", super::super::WritePriority::High)
        .await
        .unwrap();
    // Two full pages and a partial one, including separate tenants and types.
    for n in 0..259 {
        let tenant = if n < 200 { "a" } else { "b" };
        let entity_type = if n % 2 == 0 { "Job" } else { "Ticket" };
        let id = format!("{n:04}");
        conn.execute("INSERT INTO entity_catalog (tenant, entity_type, entity_id, status, fields, state, updated_at, sequence_nr) VALUES (?1, ?2, ?3, 'Done', '{}', '{}', '2099-01-01', 10)", params![tenant, entity_type, id.clone()]).await.unwrap();
        // Every third record lost its first segment. Its oldest retained event
        // is sequence 2: it must remain unknown, including after later writes.
        let sequence = if n % 3 == 0 { 2 } else { 1 };
        conn.execute("INSERT INTO events (tenant, entity_type, entity_id, sequence_nr, event_type, payload) VALUES (?1, ?2, ?3, ?4, 'Created', ?5)",
            params![tenant, entity_type, id, sequence, r#"{"timestamp":"2026-01-01T01:00:00.123+01:00"}"#]).await.unwrap();
    }
    conn.execute("DELETE FROM first_event_backfill", ())
        .await
        .unwrap();
    drop(conn);
    // Rows this small fit many in one hold: the page stops at its row count.
    assert_eq!(
        store
            .backfill_first_event_batch(Duration::MAX)
            .await
            .unwrap(),
        None
    );
    let conn = store.connection().await.unwrap();
    let mut rows = conn
        .query("SELECT records, complete FROM first_event_backfill", ())
        .await
        .unwrap();
    let row = rows.next().await.unwrap().unwrap();
    assert_eq!(row.get::<i64>(0).unwrap(), 128);
    assert_eq!(row.get::<i64>(1).unwrap(), 0);
    drop(rows);
    drop(conn);
    drop(store);
    // Reopening resumes the durable cursor, rather than repeating the first page.
    let store = TursoEventStore::new(&url, None).await.unwrap();
    assert_eq!(complete(&store).await, (259, 87));
    let rows = store
        .load_entity_catalog_rows("a", "Ticket", &["0001".into(), "0003".into()])
        .await
        .unwrap();
    assert_eq!(
        rows[0].state.as_ref().unwrap()["first_event_at"],
        "2026-01-01T00:00:00.123000000Z"
    );
    assert_eq!(
        rows[1].state.as_ref().unwrap()["first_event_at"],
        serde_json::Value::Null
    );
    let conn = store
        .write_connection("test", super::super::WritePriority::High)
        .await
        .unwrap();
    conn.execute("DELETE FROM events INDEXED BY idx_events_entity WHERE tenant=?1 AND entity_type=?2 AND entity_id=?3 AND sequence_nr=1", params!["a", "Ticket", "0001"]).await.unwrap();
    drop(conn);
    assert_eq!(
        store
            .first_event_at("a:Ticket:0001")
            .await
            .unwrap()
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        "2026-01-01T00:00:00.123000000Z"
    );
    assert_eq!(store.first_event_at("a:Ticket:0003").await.unwrap(), None);
    // The watermark covers null records too: completing again is a no-op.
    assert_eq!(
        store.backfill_first_event_batch(BATCH_HOLD).await.unwrap(),
        Some((259, 87))
    );
    let conn = store.connection().await.unwrap();
    let mut rows = conn.query("SELECT first_event_at, updated_at FROM entity_catalog WHERE tenant='a' AND entity_type='Ticket' AND entity_id='0001'", ()).await.unwrap();
    let row = rows.next().await.unwrap().unwrap();
    assert_eq!(
        row.get::<String>(0).unwrap(),
        "2026-01-01T00:00:00.123000000Z"
    );
    assert_eq!(
        row.get::<String>(1).unwrap(),
        "2099-01-01",
        "backfill is not a move"
    );
}

#[tokio::test]
async fn first_event_column_survives_single_and_batched_projection_writes() {
    let store = make_store("first-event-projections").await;
    let first = "2026-01-01T00:00:00.123000000Z";
    let state = serde_json::json!({"first_event_at": first});
    store
        .upsert_query_projection_with_state(
            "default",
            "Job",
            "one",
            "Idle",
            &serde_json::json!({}),
            &state,
            1,
        )
        .await
        .unwrap();
    store
        .upsert_query_projection_with_state(
            "default",
            "Job",
            "one",
            "Done",
            &serde_json::json!({}),
            &serde_json::json!({"first_event_at": "2099-01-01T00:00:00Z"}),
            2,
        )
        .await
        .unwrap();
    store
        .upsert_query_projections(
            "default",
            &[
                QueryProjectionUpsert {
                    entity_type: "Job".into(),
                    entity_id: "one".into(),
                    status: "Moved".into(),
                    fields: serde_json::json!({}),
                    state: serde_json::json!({}),
                    indexed_fields: serde_json::json!({}),
                    known_new: false,
                    sequence_nr: 3,
                },
                QueryProjectionUpsert {
                    entity_type: "Job".into(),
                    entity_id: "two".into(),
                    status: "Idle".into(),
                    fields: serde_json::json!({}),
                    state,
                    indexed_fields: serde_json::json!({}),
                    known_new: true,
                    sequence_nr: 1,
                },
            ],
        )
        .await
        .unwrap();
    for row in store
        .load_entity_catalog_rows("default", "Job", &["one".into(), "two".into()])
        .await
        .unwrap()
    {
        assert_eq!(row.state.unwrap()["first_event_at"], first);
    }
}

#[tokio::test]
async fn a_first_event_batch_commits_once_it_has_held_the_gate_for_its_hold() {
    let url = sqlite_test_url("first-event-backfill-hold");
    let store = TursoEventStore::new(&url, None).await.unwrap();
    let conn = store
        .write_connection("test", super::super::WritePriority::High)
        .await
        .unwrap();
    for n in 0..3 {
        let id = format!("{n:04}");
        conn.execute("INSERT INTO entity_catalog (tenant, entity_type, entity_id, status, fields, state, updated_at, sequence_nr) VALUES ('a', 'Job', ?1, 'Done', '{}', '{}', '2099-01-01', 1)", params![id.clone()]).await.unwrap();
        conn.execute("INSERT INTO events (tenant, entity_type, entity_id, sequence_nr, event_type, payload) VALUES ('a', 'Job', ?1, 1, 'Created', ?2)",
            params![id, r#"{"timestamp":"2026-01-01T00:00:00Z"}"#]).await.unwrap();
    }
    conn.execute("DELETE FROM first_event_backfill", ())
        .await
        .unwrap();
    drop(conn);
    // A batch past its hold after its first row commits that row: a move
    // waiting for the gate waits for one row, never for a page.
    for written in 1..=3 {
        assert_eq!(
            store
                .backfill_first_event_batch(Duration::ZERO)
                .await
                .unwrap(),
            None
        );
        let conn = store.connection().await.unwrap();
        let mut rows = conn
            .query("SELECT records FROM first_event_backfill", ())
            .await
            .unwrap();
        let row = rows.next().await.unwrap().unwrap();
        assert_eq!(row.get::<i64>(0).unwrap(), written);
    }
    assert_eq!(
        store
            .backfill_first_event_batch(Duration::ZERO)
            .await
            .unwrap(),
        Some((3, 0))
    );
    let rows = store
        .load_entity_catalog_rows("a", "Job", &["0000".into(), "0002".into()])
        .await
        .unwrap();
    for row in rows {
        assert_eq!(
            row.state.as_ref().unwrap()["first_event_at"],
            "2026-01-01T00:00:00.000000000Z"
        );
    }
}
