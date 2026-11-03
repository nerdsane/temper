//! A record's catalog row holds each field once, in its `fields` column and
//! not again inside its `state`.
//!
//! Turso's change log keeps the whole row before and after every UPDATE, plus
//! the value of each column the UPDATE sets. The triggers below record those
//! same sizes, so each test reads what a move adds to that log. A move that
//! rewrites the row logs its fields three times: in the row before, in the row
//! after, and as the value set; a row that held them twice logged six.

use serde_json::{Value, json};

use super::{QueryProjectionUpsert, TursoEventStore, make_store};
use crate::driver::params;

const ENTITY_TYPE: &str = "Note";
const BODY_BYTES: usize = 100_000;

/// A note whose `Body` is large and whose other state is small.
fn note(entity_id: &str, title: &str, status: &str, sequence_nr: u64) -> (Value, Value) {
    let fields = json!({"Title": title, "Body": "b".repeat(BODY_BYTES)});
    let state = json!({
        "entity_type": ENTITY_TYPE,
        "entity_id": entity_id,
        "status": status,
        "item_count": 0,
        "counters": {"edits": sequence_nr},
        "booleans": {},
        "lists": {},
        "fields": fields,
        "events": [],
        "total_event_count": sequence_nr,
        "sequence_nr": sequence_nr,
    });
    (fields, state)
}

/// Record, for every UPDATE of `entity_catalog`, the size of the row before
/// and after it and the size of each large column it sets.
async fn log_catalog_writes(store: &TursoEventStore) {
    let conn = store
        .write_connection("test", super::super::WritePriority::High)
        .await
        .expect("connection");
    conn.execute(
        "CREATE TABLE catalog_write_bytes (bytes INTEGER NOT NULL)",
        (),
    )
    .await
    .expect("create write log");
    for trigger in [
        "CREATE TRIGGER catalog_row_images AFTER UPDATE ON entity_catalog BEGIN \
           INSERT INTO catalog_write_bytes VALUES ( \
             length(old.fields) + ifnull(length(old.state), 0) \
             + length(new.fields) + ifnull(length(new.state), 0)); \
         END",
        "CREATE TRIGGER catalog_fields_set AFTER UPDATE OF fields ON entity_catalog BEGIN \
           INSERT INTO catalog_write_bytes VALUES (length(new.fields)); \
         END",
        "CREATE TRIGGER catalog_state_set AFTER UPDATE OF state ON entity_catalog BEGIN \
           INSERT INTO catalog_write_bytes VALUES (ifnull(length(new.state), 0)); \
         END",
    ] {
        conn.execute(trigger, ()).await.expect("create trigger");
    }
}

/// Bytes the change log would have gained since the last call.
async fn take_logged_bytes(store: &TursoEventStore) -> i64 {
    let conn = store.connection().await.expect("connection");
    let mut rows = conn
        .query("SELECT ifnull(sum(bytes), 0) FROM catalog_write_bytes", ())
        .await
        .expect("read write log");
    let bytes = rows
        .next()
        .await
        .expect("read write log row")
        .expect("sum row")
        .get::<i64>(0)
        .expect("logged bytes");
    drop(rows);
    let conn = store
        .write_connection("test", super::super::WritePriority::High)
        .await
        .expect("connection");
    conn.execute("DELETE FROM catalog_write_bytes", ())
        .await
        .expect("clear write log");
    bytes
}

/// `(length(fields), length(state))` of one stored catalog row.
async fn stored_lengths(store: &TursoEventStore, tenant: &str, entity_id: &str) -> (i64, i64) {
    let conn = store.connection().await.expect("connection");
    let mut rows = conn
        .query(
            "SELECT length(fields), ifnull(length(state), 0) FROM entity_catalog \
             INDEXED BY sqlite_autoindex_entity_catalog_1 \
             WHERE tenant = ?1 AND entity_type = ?2 AND entity_id = ?3",
            params![tenant, ENTITY_TYPE, entity_id],
        )
        .await
        .expect("read catalog row");
    let row = rows
        .next()
        .await
        .expect("read catalog row")
        .expect("catalog row exists");
    (
        row.get::<i64>(0).expect("fields length"),
        row.get::<i64>(1).expect("state length"),
    )
}

async fn upsert(
    store: &TursoEventStore,
    tenant: &str,
    entity_id: &str,
    fields: &Value,
    state: &Value,
    sequence_nr: u64,
) {
    store
        .upsert_query_projection_with_state(
            tenant,
            ENTITY_TYPE,
            entity_id,
            state["status"].as_str().expect("status"),
            fields,
            state,
            sequence_nr,
        )
        .await
        .expect("upsert projection");
}

async fn loaded_state(store: &TursoEventStore, tenant: &str, entity_id: &str) -> (Value, Value) {
    let rows = store
        .load_entity_catalog_rows(tenant, ENTITY_TYPE, &[entity_id.to_string()])
        .await
        .expect("load catalog row");
    assert_eq!(rows.len(), 1);
    let row = rows.into_iter().next().expect("one row");
    (row.fields, row.state.expect("catalog row has a state"))
}

/// A move that rewrote the row logged the body three times, not six.
fn assert_logged_body_three_times(logged: i64, what: &str) {
    let body = BODY_BYTES as i64;
    assert!(
        (2 * body..3 * body + body / 10).contains(&logged),
        "{what} logged {logged} bytes for a {body}-byte body"
    );
}

fn assert_holds_body_once(lengths: (i64, i64)) {
    let (fields_len, state_len) = lengths;
    let body = BODY_BYTES as i64;
    assert!(fields_len >= body, "fields hold the body: {fields_len}");
    assert!(
        state_len < body / 10,
        "state must not hold a second copy of the fields: {state_len} bytes"
    );
}

#[tokio::test]
async fn a_move_writes_each_field_once() {
    let store = make_store("catalog-row-writes").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let id = "note-large";
    log_catalog_writes(&store).await;

    let (fields, created) = note(id, "Plan", "Draft", 1);
    upsert(&store, &tenant, id, &fields, &created, 1).await;
    assert_holds_body_once(stored_lengths(&store, &tenant, id).await);
    take_logged_bytes(&store).await;

    // A move that changes the status and counters but no field.
    let (_, moved) = note(id, "Plan", "Active", 2);
    upsert(&store, &tenant, id, &fields, &moved, 2).await;
    assert_logged_body_three_times(take_logged_bytes(&store).await, "a status move");
    assert_eq!(
        loaded_state(&store, &tenant, id).await,
        (fields.clone(), moved.clone())
    );

    // The same projection written again, as a replay or read repair does.
    upsert(&store, &tenant, id, &fields, &moved, 2).await;
    assert_eq!(
        take_logged_bytes(&store).await,
        0,
        "a write that changes nothing writes nothing"
    );

    // A move that changes a small field.
    let (edited_fields, edited) = note(id, "Plan v2", "Active", 3);
    upsert(&store, &tenant, id, &edited_fields, &edited, 3).await;
    assert_logged_body_three_times(take_logged_bytes(&store).await, "a field edit");
    assert_holds_body_once(stored_lengths(&store, &tenant, id).await);
    assert_eq!(
        loaded_state(&store, &tenant, id).await,
        (edited_fields, edited)
    );
}

#[tokio::test]
async fn a_row_that_holds_its_fields_twice_reads_unchanged_and_shrinks_on_its_next_move() {
    let store = make_store("catalog-row-legacy").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let id = "note-legacy";
    let (fields, created) = note(id, "Plan", "Draft", 1);
    // The connection holds the write gate's permit; it is dropped before the
    // store writes again below.
    store
        .write_connection("test", super::super::WritePriority::High)
        .await
        .expect("connection")
        .execute(
            "INSERT INTO entity_catalog \
             (tenant, entity_type, entity_id, status, fields, state, updated_at, sequence_nr, projection_version, projection_hash) \
             VALUES (?1, ?2, ?3, 'Draft', ?4, ?5, '2026-01-01T00:00:00Z', 1, 2, 'written-before')",
            params![
                tenant.as_str(),
                ENTITY_TYPE,
                id,
                fields.to_string(),
                created.to_string()
            ],
        )
        .await
        .expect("insert a row that holds its fields twice");

    assert_eq!(
        loaded_state(&store, &tenant, id).await,
        (fields.clone(), created)
    );

    let (_, moved) = note(id, "Plan", "Active", 2);
    upsert(&store, &tenant, id, &fields, &moved, 2).await;
    assert_holds_body_once(stored_lengths(&store, &tenant, id).await);
    assert_eq!(loaded_state(&store, &tenant, id).await, (fields, moved));
}

#[tokio::test]
async fn fields_kept_out_of_the_query_plane_stay_in_the_stored_state() {
    let store = make_store("catalog-row-unindexed").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let id = "note-unindexed";
    let (indexed, mut state) = note(id, "Plan", "Draft", 1);
    state["fields"]["Heartbeat"] = json!("beat-1");

    upsert(&store, &tenant, id, &indexed, &state, 1).await;

    assert_holds_body_once(stored_lengths(&store, &tenant, id).await);
    let (loaded_fields, loaded) = loaded_state(&store, &tenant, id).await;
    assert_eq!(loaded_fields, indexed);
    assert_eq!(loaded, state);
}

#[tokio::test]
async fn a_batched_move_writes_each_field_once() {
    let store = make_store("catalog-row-batch").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let id = "note-batch";
    log_catalog_writes(&store).await;
    let batch = |state: &Value, sequence_nr: u64, known_new: bool| QueryProjectionUpsert {
        entity_type: ENTITY_TYPE.to_string(),
        entity_id: id.to_string(),
        status: state["status"].as_str().expect("status").to_string(),
        fields: state["fields"].clone(),
        state: state.clone(),
        indexed_fields: json!({"Title": state["fields"]["Title"].clone()}),
        sequence_nr,
        known_new,
    };

    let (_, created) = note(id, "Plan", "Draft", 1);
    store
        .upsert_query_projections(&tenant, &[batch(&created, 1, true)])
        .await
        .expect("batched create");
    assert_holds_body_once(stored_lengths(&store, &tenant, id).await);
    take_logged_bytes(&store).await;

    let (fields, moved) = note(id, "Plan", "Active", 2);
    store
        .upsert_query_projections(&tenant, &[batch(&moved, 2, false)])
        .await
        .expect("batched move");
    assert_logged_body_three_times(take_logged_bytes(&store).await, "a batched status move");
    assert_holds_body_once(stored_lengths(&store, &tenant, id).await);
    assert_eq!(loaded_state(&store, &tenant, id).await, (fields, moved));

    let title_matches = store
        .query_field_index(
            &tenant,
            ENTITY_TYPE,
            "entity_id IN (SELECT entity_id FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 AND field_name = ?3 AND field_value = ?4)",
            vec!["Title".to_string(), "Plan".to_string()],
        )
        .await
        .expect("query title");
    assert_eq!(title_matches, vec![id.to_string()]);
}

/// The records the field index finds with this title, in id order.
async fn titled(store: &TursoEventStore, tenant: &str, title: &str) -> Vec<String> {
    let mut ids = store
        .query_field_index(
            tenant,
            ENTITY_TYPE,
            "entity_id IN (SELECT entity_id FROM entity_field_index \
             WHERE tenant = ?1 AND entity_type = ?2 AND field_name = ?3 AND field_value = ?4)",
            vec!["Title".to_string(), title.to_string()],
        )
        .await
        .expect("query title");
    ids.sort();
    ids
}

fn batched(id: &str, title: &str, status: &str, sequence_nr: u64) -> QueryProjectionUpsert {
    let (fields, state) = note(id, title, status, sequence_nr);
    QueryProjectionUpsert {
        entity_type: ENTITY_TYPE.to_string(),
        entity_id: id.to_string(),
        status: status.to_string(),
        indexed_fields: json!({"Title": title}),
        fields,
        state,
        sequence_nr,
        known_new: false,
    }
}

#[tokio::test]
async fn a_batch_of_several_records_stores_and_indexes_each_from_its_newest_move() {
    let store = make_store("catalog-row-batch-many").await;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let created = |id: &str| QueryProjectionUpsert {
        known_new: true,
        ..batched(id, "Plan", "Draft", 1)
    };

    store
        .upsert_query_projections(&tenant, &[created("note-a"), created("note-b")])
        .await
        .expect("batched creates");
    assert_eq!(
        titled(&store, &tenant, "Plan").await,
        vec!["note-a".to_string(), "note-b".to_string()]
    );

    // One batch moves note-a twice and note-b once, and creates note-c.
    store
        .upsert_query_projections(
            &tenant,
            &[
                batched("note-a", "Plan", "Active", 2),
                batched("note-b", "Budget", "Draft", 2),
                batched("note-a", "Plan v2", "Active", 3),
                created("note-c"),
            ],
        )
        .await
        .expect("batched moves");

    assert_eq!(
        titled(&store, &tenant, "Plan").await,
        vec!["note-c".to_string()]
    );
    assert_eq!(
        titled(&store, &tenant, "Plan v2").await,
        vec!["note-a".to_string()]
    );
    assert_eq!(
        titled(&store, &tenant, "Budget").await,
        vec!["note-b".to_string()]
    );
    for (id, title, status, sequence_nr) in [
        ("note-a", "Plan v2", "Active", 3),
        ("note-b", "Budget", "Draft", 2),
        ("note-c", "Plan", "Draft", 1),
    ] {
        assert_holds_body_once(stored_lengths(&store, &tenant, id).await);
        assert_eq!(
            loaded_state(&store, &tenant, id).await,
            note(id, title, status, sequence_nr),
            "{id} reads back its newest move"
        );
    }
}
