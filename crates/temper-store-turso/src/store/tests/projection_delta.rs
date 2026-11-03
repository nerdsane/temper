//! The delta is co-committed, sequence guarded, durable, and tenant scoped.
use super::*;

async fn candidates(store: &TursoEventStore, tenant: &str) -> (Vec<String>, Vec<String>) {
    store
        .reconciled_equality_candidates(
            tenant,
            "Payment",
            &[("app".into(), "needle".into())],
            Some(100),
        )
        .await
        .unwrap()
}

async fn project(store: &TursoEventStore, id: &str, app: &str, sequence: u64) {
    store
        .upsert_query_projection(
            "t",
            "Payment",
            id,
            "Draft",
            &serde_json::json!({"Id": id, "app": app}),
            sequence,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn projection_delta_tracks_commits_and_only_retires_applied_sequences() {
    let store = make_store("projection-delta").await;
    for id in ["changed", "match", "missing", "noise"] {
        store
            .append(
                &format!("t:Payment:{id}"),
                0,
                &[test_envelope("Create", serde_json::json!({}))],
            )
            .await
            .unwrap();
    }
    project(&store, "changed", "old", 1).await;
    project(&store, "match", "needle", 1).await;
    project(&store, "noise", "other", 1).await;
    assert_eq!(
        candidates(&store, "t").await,
        (
            vec!["match".into(), "missing".into()],
            vec!["missing".into()]
        )
    );
    store
        .append(
            "t:Payment:changed",
            1,
            &[test_envelope("Update", serde_json::json!({}))],
        )
        .await
        .unwrap();
    // A projection arriving late must not erase a newer commit's mark.
    project(&store, "changed", "old", 1).await;
    assert_eq!(candidates(&store, "t").await.1, vec!["changed", "missing"]);
    project(&store, "changed", "needle", 2).await;
    assert_eq!(
        candidates(&store, "t").await.0,
        vec!["changed", "match", "missing"]
    );
    assert_eq!(candidates(&store, "t").await.1, vec!["missing"]);
    // Unchanged projections advance only counters, and still retire the mark.
    store
        .append(
            "t:Payment:changed",
            2,
            &[test_envelope("Touch", serde_json::json!({}))],
        )
        .await
        .unwrap();
    project(&store, "changed", "needle", 3).await;
    assert_eq!(candidates(&store, "t").await.1, vec!["missing"]);
    assert!(candidates(&store, "other").await.0.is_empty());
    // An optimistic concurrency rejection must not leave a phantom mark.
    assert!(
        store
            .append(
                "t:Payment:noise",
                0,
                &[test_envelope("Touch", serde_json::json!({}))]
            )
            .await
            .is_err()
    );
    assert_eq!(candidates(&store, "t").await.1, vec!["missing"]);
    // Pending ids must be prioritized even when their id sorts after all
    // projected matches. Otherwise an empty bounded gap could falsely vouch
    // for a broader native page.
    let bounded = store
        .reconciled_equality_candidates("t", "Payment", &[("app".into(), "needle".into())], Some(1))
        .await
        .unwrap();
    assert_eq!(bounded.0.len(), 2);
    assert_eq!(bounded.1, vec!["missing"]);
    let complete = store
        .reconciled_equality_candidates("t", "Payment", &[("app".into(), "needle".into())], None)
        .await
        .unwrap();
    assert_eq!(complete.0, vec!["changed", "match", "missing"]);
    assert_eq!(complete.1, vec!["missing"]);
}

#[tokio::test]
async fn projection_delta_covers_batch_snapshot_wrapped_and_removed_rows() {
    let store = make_store("projection-delta-batch").await;
    store
        .append_batch(&[PersistenceAppend {
            persistence_id: "t:Payment:batch".into(),
            expected_sequence: 0,
            events: vec![test_envelope("Create", serde_json::json!({}))],
            state_timer: None,
            projection: None,
        }])
        .await
        .unwrap();
    store
        .save_snapshot("t:Payment:snapshot", 7, b"{}")
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
    project(&store, "removed", "other", 1).await;
    store
        .remove_query_projection("t", "Payment", "removed")
        .await
        .unwrap();
    assert_eq!(
        candidates(&store, "t").await.1,
        vec!["batch", "removed", "snapshot", "wrapped"]
    );
    project(&store, "snapshot", "other", 7).await;
    store
        .save_snapshot("t:Payment:snapshot", 7, b"{}")
        .await
        .unwrap();
    assert_eq!(
        candidates(&store, "t").await.1,
        vec!["batch", "removed", "wrapped"]
    );
}

#[tokio::test]
async fn projection_delta_backfill_is_atomic_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("file:{}", dir.path().join("old.db").display());
    let store = TursoEventStore::new(&url, None).await.unwrap();
    for id in ["behind", "current", "missing", "unindexed", "wrapped"] {
        store
            .append(
                &format!("t:Payment:{id}"),
                0,
                &[test_envelope("Create", serde_json::json!({}))],
            )
            .await
            .unwrap();
        if id != "missing" {
            project(&store, id, "other", 1).await;
        }
    }
    store
        .append(
            "t:Payment:behind",
            1,
            &[test_envelope("Touch", serde_json::json!({}))],
        )
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
    // Recreate a pre-feature database, including a row from before the field index.
    {
        let conn = store
            .write_connection("test", super::super::WritePriority::High)
            .await
            .unwrap();
        conn.execute("DROP TABLE entity_projection_delta", ())
            .await
            .unwrap();
        conn.execute(
            "DELETE FROM entity_field_index WHERE entity_id = 'unindexed'",
            (),
        )
        .await
        .unwrap();
    }
    drop(store);
    let store = TursoEventStore::new(&url, None).await.unwrap();
    assert_eq!(
        candidates(&store, "t").await.1,
        vec!["behind", "missing", "unindexed", "wrapped"]
    );
    project(&store, "behind", "other", 2).await;
    project(&store, "unindexed", "other", 1).await;
    assert!(
        candidates(&store, "t")
            .await
            .1
            .contains(&"unindexed".to_string()),
        "an unchanged projection must not vouch for a missing legacy index"
    );
    drop(store);
    let store = TursoEventStore::new(&url, None).await.unwrap();
    assert_eq!(
        candidates(&store, "t").await.1,
        vec!["missing", "unindexed", "wrapped"]
    );
}

#[tokio::test]
async fn projection_delta_never_resurrects_a_tombstone_or_scans_deleted_rows() {
    let store = make_store("projection-delta-deleted").await;
    store
        .append(
            "t:Payment:gone",
            0,
            &[test_envelope("Create", serde_json::json!({}))],
        )
        .await
        .unwrap();
    project(&store, "gone", "needle", 1).await;
    store
        .append("t:Payment:gone", 1, &[tombstone_envelope()])
        .await
        .unwrap();
    assert!(
        candidates(&store, "t").await.0.is_empty(),
        "the stale index must not return a deleted row"
    );
    store
        .remove_query_projection("t", "Payment", "gone")
        .await
        .unwrap();
    // A queued projection can arrive after removal; the terminal marker survives
    // it and is excluded through the pending index, not hydrated on each read.
    project(&store, "gone", "needle", 1).await;
    assert!(candidates(&store, "t").await.0.is_empty());
    assert!(candidates(&store, "t").await.1.is_empty());
}

/// A batch that carries a record's projection commits its catalog and index
/// rows with its events and leaves no gap, as a single append does; a batch
/// delete removes them. On main a batch appended only the events.
#[tokio::test]
async fn a_batch_commits_each_records_projection_with_its_events() {
    let store = make_store("projection-delta-batch-rows").await;
    let upsert = |id: &str| PersistenceAppend {
        persistence_id: format!("t:Payment:{id}"),
        expected_sequence: 0,
        events: vec![test_envelope("Create", serde_json::json!({}))],
        state_timer: None,
        projection: Some(temper_runtime::persistence::AppendProjection::Upsert {
            status: "Draft".into(),
            fields: serde_json::json!({"Id": id, "app": "needle"}),
            state: serde_json::json!({}),
        }),
    };
    store
        .append_batch(&[upsert("one"), upsert("two")])
        .await
        .unwrap();
    assert_eq!(
        candidates(&store, "t").await,
        (vec!["one".to_string(), "two".to_string()], Vec::new())
    );
    store
        .append_batch(&[PersistenceAppend {
            persistence_id: "t:Payment:one".into(),
            expected_sequence: 1,
            events: vec![test_envelope(
                "Deleted",
                serde_json::json!({"kind": "tombstone"}),
            )],
            state_timer: None,
            projection: Some(temper_runtime::persistence::AppendProjection::Remove),
        }])
        .await
        .unwrap();
    assert_eq!(candidates(&store, "t").await.0, vec!["two".to_string()]);
}

#[tokio::test]
async fn a_batch_commits_timer_resets_and_ends_or_none_of_its_rows() {
    use temper_runtime::persistence::{AppendStateTimer, StateTimerRecord};
    let store = make_store("projection-delta-batch-timers").await;
    let arm = |id: &str, seq: u64| StateTimerRecord {
        tenant: "t".into(),
        entity_type: "Payment".into(),
        entity_id: id.into(),
        state: "Draft".into(),
        arm_id: format!("Draft-{seq}"),
        sequence_nr: seq,
        started_at: format!("2026-11-02T00:00:{seq:02}Z"),
        params: format!("{{\"version\":{seq}}}"),
        armed_by: format!("{{\"agent_id\":\"caller-{seq}\"}}"),
    };
    let append = |id: &str, expected, timer| PersistenceAppend {
        persistence_id: format!("t:Payment:{id}"),
        expected_sequence: expected,
        events: vec![test_envelope("Move", serde_json::json!({}))],
        state_timer: timer,
        projection: Some(temper_runtime::persistence::AppendProjection::Upsert {
            status: "Draft".into(),
            fields: serde_json::json!({"Id": id}),
            state: serde_json::json!({}),
        }),
    };
    let a = arm("one", 1);
    let b = arm("two", 1);
    store
        .append_batch(&[
            append("one", 0, Some(AppendStateTimer::Arm(a.clone()))),
            append("two", 0, Some(AppendStateTimer::Arm(b.clone()))),
        ])
        .await
        .unwrap();
    assert_eq!(
        {
            let mut rows = store.list_state_timers("t").await.unwrap();
            rows.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));
            rows
        },
        vec![a.clone(), b.clone()]
    );
    let reset = arm("one", 2);
    let refused = store
        .append_batch(&[
            append("one", 1, Some(AppendStateTimer::Arm(reset.clone()))),
            append("two", 0, Some(AppendStateTimer::End)),
        ])
        .await;
    assert!(matches!(
        refused,
        Err(PersistenceError::ConcurrencyViolation { .. })
    ));
    assert_eq!(
        store.read_events("t:Payment:one", 0).await.unwrap().len(),
        1
    );
    assert_eq!(
        {
            let mut rows = store.list_state_timers("t").await.unwrap();
            rows.sort_by(|a, b| a.entity_id.cmp(&b.entity_id));
            rows
        },
        vec![a, b]
    );
    store
        .append_batch(&[
            append("one", 1, Some(AppendStateTimer::Arm(reset.clone()))),
            append("two", 1, Some(AppendStateTimer::End)),
        ])
        .await
        .unwrap();
    assert_eq!(
        store.list_state_timers("t").await.unwrap(),
        vec![reset.clone()]
    );
    store.append_batch(&[append("one", 2, None)]).await.unwrap();
    assert_eq!(store.list_state_timers("t").await.unwrap(), vec![reset]);
    store
        .append_batch(&[append("one", 3, Some(AppendStateTimer::End))])
        .await
        .unwrap();
    assert!(store.list_state_timers("t").await.unwrap().is_empty());
}
