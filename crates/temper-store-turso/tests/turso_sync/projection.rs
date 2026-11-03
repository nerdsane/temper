//! What a projection write costs a synced database's change log.

use temper_store_turso::QueryProjectionUpsert;

use super::support::*;

/// A projected state the size of a busy record's, with the counters every
/// action moves.
fn state(fields: &serde_json::Value, seq: u64) -> serde_json::Value {
    serde_json::json!({
        "entity_type": "Release",
        "entity_id": "rel-1",
        "status": "Observing",
        "counters": {},
        "booleans": {},
        "lists": {},
        "fields": fields,
        "events": [],
        "total_event_count": seq,
        "events_since_snapshot": seq,
        "last_snapshot_sequence_nr": 0,
        "sequence_nr": seq,
    })
}

#[tokio::test]
async fn an_action_that_changes_nothing_logs_only_its_counters() {
    let Some(server) = SyncServer::start() else {
        return;
    };
    let (_dir, path) = machine();
    let store = server.open(&path).await;
    let copy = store.sync_copy().expect("copy").clone();
    let fields = serde_json::json!({
        "Title": "release",
        "Checks": "x".repeat(70_000),
    });

    store
        .upsert_query_projection_with_state(
            "default",
            "Release",
            "rel-1",
            "Observing",
            &fields,
            &state(&fields, 1),
            1,
        )
        .await
        .expect("first projection");
    push(&store).await;
    let before = copy.last_change().await.expect("newest change");

    for step in 1..=10 {
        let seq = 2 * step;
        store
            .upsert_query_projection_with_state(
                "default",
                "Release",
                "rel-1",
                "Observing",
                &fields,
                &state(&fields, seq),
                seq,
            )
            .await
            .expect("unchanged projection");
        store
            .upsert_query_projections(
                "default",
                &[QueryProjectionUpsert {
                    entity_type: "Release".to_string(),
                    entity_id: "rel-1".to_string(),
                    status: "Observing".to_string(),
                    fields: fields.clone(),
                    state: state(&fields, seq + 1),
                    indexed_fields: serde_json::json!({ "Title": "release" }),
                    sequence_nr: seq + 1,
                    known_new: false,
                }],
            )
            .await
            .expect("unchanged batch projection");
    }

    // Rewriting the row would log it whole before and after: over 140 KB
    // each time. Twenty unchanged projections store only their counters.
    let bytes = copy
        .change_log_bytes_after(before)
        .await
        .expect("change log bytes");
    assert!(
        bytes < 20 * 1024,
        "20 unchanged projections added {bytes} bytes to the change log"
    );

    // The remote takes the counters, and a fresh machine reads the newest one.
    push(&store).await;
    let (_b_dir, b_path) = machine();
    let b = server.open(&b_path).await;
    let rows = b
        .load_entity_catalog_rows("default", "Release", &["rel-1".to_string()])
        .await
        .expect("catalog rows");
    assert_eq!(rows[0].sequence_nr, 21);
    assert_eq!(
        rows[0].state.as_ref().expect("state")["total_event_count"],
        21
    );
}
