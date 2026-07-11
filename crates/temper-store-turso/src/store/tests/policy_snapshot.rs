use temper_runtime::persistence::PersistenceError;

use super::super::PolicySnapshotEntry;
use super::{TursoEventStore, sqlite_test_url};

fn entry(policy_id: &str, cedar_text: &str) -> PolicySnapshotEntry {
    PolicySnapshotEntry {
        policy_id: policy_id.to_string(),
        cedar_text: cedar_text.to_string(),
        created_at: "2026-07-11T16:00:00Z".to_string(),
        created_by: "reviewer".to_string(),
        enabled: true,
    }
}

#[tokio::test]
async fn snapshot_cas_publishes_authoritative_empty_set() {
    let store = TursoEventStore::new(&sqlite_test_url("policy-snapshot-empty"), None)
        .await
        .expect("create store");
    let initial = store
        .load_policy_snapshot("tenant-a")
        .await
        .expect("initial snapshot");
    assert_eq!(initial.version, 0);
    assert!(initial.rows.is_empty());

    let version_one = store
        .replace_policy_snapshot(
            "tenant-a",
            0,
            vec![entry("primary", "permit(principal, action, resource);")],
        )
        .await
        .expect("publish policy");
    assert_eq!(version_one, 1);

    let version_two = store
        .replace_policy_snapshot("tenant-a", version_one, vec![])
        .await
        .expect("publish empty policy set");
    assert_eq!(version_two, 2);
    let empty = store
        .load_policy_snapshot("tenant-a")
        .await
        .expect("empty snapshot");
    assert_eq!(empty.version, 2);
    assert!(empty.rows.is_empty());
}

#[tokio::test]
async fn independent_instances_cannot_publish_from_same_version() {
    let url = sqlite_test_url("policy-snapshot-race");
    let first = TursoEventStore::new(&url, None).await.expect("first store");
    let second = TursoEventStore::new(&url, None)
        .await
        .expect("second store");
    let left = first.replace_policy_snapshot(
        "tenant-a",
        0,
        vec![entry("left", "permit(principal, action, resource);")],
    );
    let right = second.replace_policy_snapshot(
        "tenant-a",
        0,
        vec![entry("right", "forbid(principal, action, resource);")],
    );
    let (left, right) = tokio::join!(left, right);
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    let loser = if left.is_err() { left } else { right };
    assert!(matches!(
        loser,
        Err(PersistenceError::ConcurrencyViolation {
            expected: 0,
            actual: 1
        })
    ));

    let committed = first
        .load_policy_snapshot("tenant-a")
        .await
        .expect("committed snapshot");
    assert_eq!(committed.version, 1);
    assert_eq!(committed.rows.len(), 1);
}
