//! Scheduled-action rows commit with the event that arms them and with the
//! event that fires them. Cancellation durably clears the action before cleanup.

use temper_runtime::persistence::{
    CommitRows, EventStore, PersistenceError, ScheduledActionRecord,
};

use super::{make_store, test_envelope};

fn row(tenant: &str, entity_id: &str, arm_id: &str) -> ScheduledActionRecord {
    ScheduledActionRecord {
        tenant: tenant.to_string(),
        entity_type: "Job".to_string(),
        entity_id: entity_id.to_string(),
        arm_id: arm_id.to_string(),
        action: "Fire".to_string(),
        due_at: "2026-01-01T00:10:00+00:00".to_string(),
        armed_by: "{}".to_string(),
    }
}

fn arm_ids(rows: &[ScheduledActionRecord]) -> Vec<(String, String)> {
    let mut ids: Vec<_> = rows
        .iter()
        .map(|row| (row.entity_id.clone(), row.arm_id.clone()))
        .collect();
    ids.sort();
    ids
}

#[tokio::test]
async fn scheduled_action_rows_commit_with_their_events() {
    let store = make_store("scheduled-actions").await;
    let armed = [
        row("tenant-a", "job-1", "1.0"),
        row("tenant-a", "job-1", "1.1"),
    ];

    // Arming: the rows land with the event, and only in their tenant.
    store
        .append_with_rows(
            "tenant-a:Job:job-1",
            0,
            &[test_envelope("Start", serde_json::json!({}))],
            CommitRows {
                scheduled_actions: &armed,
                ..CommitRows::default()
            },
        )
        .await
        .expect("arming append");
    store
        .append_with_rows(
            "tenant-b:Job:job-1",
            0,
            &[test_envelope("Start", serde_json::json!({}))],
            CommitRows {
                scheduled_actions: &[row("tenant-b", "job-1", "1.0")],
                ..CommitRows::default()
            },
        )
        .await
        .expect("other tenant's append");
    let rows = store.list_scheduled_actions("tenant-a").await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows.iter().find(|r| r.arm_id == "1.0"), Some(&armed[0]));
    assert_eq!(
        arm_ids(&store.list_scheduled_actions("tenant-b").await.unwrap()),
        vec![("job-1".to_string(), "1.0".to_string())]
    );

    // A refused append writes no row and removes none.
    let err = store
        .append_with_rows(
            "tenant-a:Job:job-1",
            0,
            &[test_envelope("Fire", serde_json::json!({}))],
            CommitRows {
                scheduled_actions: &[row("tenant-a", "job-1", "9.0")],
                fired_scheduled_action: Some("1.0"),
                ..CommitRows::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PersistenceError::ConcurrencyViolation { .. }));
    assert_eq!(
        arm_ids(&store.list_scheduled_actions("tenant-a").await.unwrap()),
        vec![
            ("job-1".to_string(), "1.0".to_string()),
            ("job-1".to_string(), "1.1".to_string())
        ]
    );

    // Firing: the event removes its own row in the same commit, and no
    // other record's or tenant's row with the same arm id.
    store
        .append_with_rows(
            "tenant-a:Job:job-1",
            1,
            &[test_envelope("Fire", serde_json::json!({}))],
            CommitRows {
                fired_scheduled_action: Some("1.0"),
                ..CommitRows::default()
            },
        )
        .await
        .expect("firing append");
    assert_eq!(
        arm_ids(&store.list_scheduled_actions("tenant-a").await.unwrap()),
        vec![("job-1".to_string(), "1.1".to_string())]
    );
    assert_eq!(
        store
            .list_scheduled_actions("tenant-b")
            .await
            .unwrap()
            .len(),
        1
    );

    // A refused call is durably cancelled before deletion. Repeating the
    // cancellation, including after deletion, is harmless and tenant-scoped.
    for _ in 0..2 {
        store
            .cancel_scheduled_action("tenant-a", "Job", "job-1", "1.1")
            .await
            .expect("cancellation");
    }
    let cancelled = store.list_scheduled_actions("tenant-a").await.unwrap();
    assert_eq!(cancelled.len(), 1);
    assert!(
        cancelled[0].action.is_empty(),
        "a restart sees terminal disposition"
    );
    assert_eq!(
        store.list_scheduled_actions("tenant-b").await.unwrap()[0].action,
        "Fire"
    );

    store
        .cancel_scheduled_action("tenant-a", "Job", "missing", "1.1")
        .await
        .expect("absent cancellation");
    // A removal outside a commit takes one row, and repeating it is a no-op.
    for _ in 0..2 {
        store
            .delete_scheduled_action("tenant-a", "Job", "job-1", "1.1")
            .await
            .expect("removal");
    }
    store
        .cancel_scheduled_action("tenant-a", "Job", "job-1", "1.1")
        .await
        .expect("cancellation after deletion");
    assert!(
        store
            .list_scheduled_actions("tenant-a")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .list_scheduled_actions("tenant-b")
            .await
            .unwrap()
            .len(),
        1
    );
}
