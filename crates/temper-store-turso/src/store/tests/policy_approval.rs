use super::make_store;

fn decision_json(status: &str) -> String {
    serde_json::json!({
        "id": "decision-1",
        "tenant": "tenant-a",
        "status": status,
    })
    .to_string()
}

#[tokio::test]
async fn pending_decision_upsert_cannot_move_ownership() {
    let store = make_store("pending-decision-owner").await;
    let pending = decision_json("pending");
    store
        .upsert_pending_decision("decision-1", "tenant-a", "pending", &pending)
        .await
        .unwrap();

    let error = store
        .upsert_pending_decision("decision-1", "tenant-b", "pending", &pending)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("owned by another tenant"));
    assert!(store
        .get_pending_decision("tenant-b", "decision-1")
        .await
        .unwrap()
        .is_none());
    assert!(store
        .get_pending_decision("tenant-a", "decision-1")
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn policy_and_decision_commit_and_rollback_together() {
    let store = make_store("policy-approval-transaction").await;
    let pending = decision_json("pending");
    let approved = decision_json("approved");
    store
        .upsert_pending_decision("decision-1", "tenant-a", "pending", &pending)
        .await
        .unwrap();

    store
        .commit_policy_approval(crate::TursoPolicyApprovalCommit {
            tenant: "tenant-a",
            decision_id: "decision-1",
            approved_decision_json: &approved,
            policy_id: "decision:decision-1",
            cedar_text: "permit(principal, action, resource);",
            created_by: "reviewer",
        })
        .await
        .unwrap();

    let policies = store.load_policies_for_tenant("tenant-a").await.unwrap();
    assert_eq!(policies.len(), 1);
    assert_eq!(
        store
            .get_pending_decision("tenant-a", "decision-1")
            .await
            .unwrap()
            .unwrap(),
        approved
    );

    store
        .rollback_policy_approval("tenant-a", "decision-1", &pending, "decision:decision-1")
        .await
        .unwrap();
    assert!(store
        .load_policies_for_tenant("tenant-a")
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .get_pending_decision("tenant-a", "decision-1")
            .await
            .unwrap()
            .unwrap(),
        pending
    );
}

#[tokio::test]
async fn policy_conflict_leaves_decision_pending() {
    let store = make_store("policy-approval-conflict").await;
    let pending = decision_json("pending");
    store
        .upsert_pending_decision("decision-1", "tenant-a", "pending", &pending)
        .await
        .unwrap();
    store
        .save_policy(
            "tenant-a",
            "decision:decision-1",
            "forbid(principal, action, resource);",
            "existing",
        )
        .await
        .unwrap();

    let error = store
        .commit_policy_approval(crate::TursoPolicyApprovalCommit {
            tenant: "tenant-a",
            decision_id: "decision-1",
            approved_decision_json: &decision_json("approved"),
            policy_id: "decision:decision-1",
            cedar_text: "permit(principal, action, resource);",
            created_by: "reviewer",
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("already exists"));
    assert_eq!(
        store
            .get_pending_decision("tenant-a", "decision-1")
            .await
            .unwrap()
            .unwrap(),
        pending
    );
    let policies = store.load_policies_for_tenant("tenant-a").await.unwrap();
    assert_eq!(policies.len(), 1);
    assert!(policies[0].cedar_text.starts_with("forbid"));
}

#[tokio::test]
async fn save_policy_skips_identical_enabled_cedar_text() {
    let store = make_store("policy-identical-enabled-text").await;
    let cedar = "permit(principal, action == Action::\"read\", resource);";

    let first = store
        .save_policy("tenant-a", "paw-patrol-patrol", cedar, "os-app:paw-patrol")
        .await
        .unwrap();
    assert!(first, "first insert of a new enabled policy must write");

    let second = store
        .save_policy("tenant-a", "primary", cedar, "api")
        .await
        .unwrap();
    assert!(
        !second,
        "identical enabled cedar_text must not create a second row"
    );

    let policies = store.load_policies_for_tenant("tenant-a").await.unwrap();
    assert_eq!(policies.len(), 1);
    assert_eq!(policies[0].policy_id, "paw-patrol-patrol");
}

#[tokio::test]
async fn save_policy_still_inserts_when_only_disabled_duplicate_exists() {
    let store = make_store("policy-disabled-duplicate-text").await;
    let cedar = "permit(principal, action == Action::\"read\", resource);";
    store
        .save_policy("tenant-a", "old-copy", cedar, "system")
        .await
        .unwrap();
    assert!(store
        .toggle_policy_enabled("tenant-a", "old-copy", false)
        .await
        .unwrap());

    let inserted = store
        .save_policy("tenant-a", "new-copy", cedar, "system")
        .await
        .unwrap();
    assert!(
        inserted,
        "a disabled row must not block inserting the same text as a new enabled policy"
    );

    let policies = store.load_policies_for_tenant("tenant-a").await.unwrap();
    assert_eq!(policies.len(), 2);
}
