use super::{TursoEventStore, sqlite_test_url};

#[tokio::test]
async fn independent_instances_choose_one_resolution_owner() {
    let url = sqlite_test_url("decision-resolution-race");
    let first = TursoEventStore::new(&url, None).await.expect("first store");
    let second = TursoEventStore::new(&url, None)
        .await
        .expect("second store");
    let pending = serde_json::json!({
        "id": "pd-1",
        "tenant": "tenant-a",
        "status": "pending"
    })
    .to_string();
    first
        .upsert_pending_decision("pd-1", "tenant-a", "pending", &pending)
        .await
        .expect("seed decision");
    let approve = serde_json::json!({
        "id": "pd-1",
        "tenant": "tenant-a",
        "status": "pending",
        "resolution_owner": "approve-owner",
        "resolution_kind": "approve"
    })
    .to_string();
    let deny = serde_json::json!({
        "id": "pd-1",
        "tenant": "tenant-a",
        "status": "pending",
        "resolution_owner": "deny-owner",
        "resolution_kind": "deny"
    })
    .to_string();

    let (approve_won, deny_won) = tokio::join!(
        first.claim_decision_resolution("tenant-a", "pd-1", &approve),
        second.claim_decision_resolution("tenant-a", "pd-1", &deny),
    );
    let approve_won = approve_won.expect("approve claim");
    let deny_won = deny_won.expect("deny claim");
    assert_ne!(approve_won, deny_won);

    let (winner, loser, winner_data) = if approve_won {
        ("approve-owner", "deny-owner", approve)
    } else {
        ("deny-owner", "approve-owner", deny)
    };
    assert!(
        first
            .update_decision_resolution(
                "tenant-a",
                "pd-1",
                winner,
                if approve_won { "approved" } else { "denied" },
                &winner_data,
            )
            .await
            .expect("winner completes")
    );
    assert!(
        !second
            .update_decision_resolution(
                "tenant-a",
                "pd-1",
                loser,
                if approve_won { "denied" } else { "approved" },
                &winner_data,
            )
            .await
            .expect("loser rejected")
    );
}

#[tokio::test]
async fn only_exact_owner_can_release_and_reopen_resolution() {
    let url = sqlite_test_url("decision-resolution-release");
    let first = TursoEventStore::new(&url, None).await.expect("first store");
    let second = TursoEventStore::new(&url, None)
        .await
        .expect("second store");
    let pending = serde_json::json!({
        "id": "pd-release",
        "tenant": "tenant-a",
        "status": "pending"
    })
    .to_string();
    first
        .upsert_pending_decision("pd-release", "tenant-a", "pending", &pending)
        .await
        .expect("seed decision");
    let claimed = serde_json::json!({
        "id": "pd-release",
        "tenant": "tenant-a",
        "status": "pending",
        "resolution_owner": "first-owner",
        "resolution_kind": "approve"
    })
    .to_string();
    assert!(
        first
            .claim_decision_resolution("tenant-a", "pd-release", &claimed)
            .await
            .expect("first claim")
    );

    assert!(
        !second
            .release_decision_resolution("tenant-a", "pd-release", "wrong-owner", &pending)
            .await
            .expect("wrong-owner release")
    );
    assert!(
        first
            .release_decision_resolution("tenant-a", "pd-release", "first-owner", &pending)
            .await
            .expect("exact-owner release")
    );
    assert!(
        second
            .claim_decision_resolution("tenant-a", "pd-release", &claimed)
            .await
            .expect("claim after release")
    );
}
