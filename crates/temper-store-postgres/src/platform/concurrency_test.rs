use sqlx::PgPool;
use temper_runtime::persistence::PersistenceError;

use super::{PostgresEventStore, PostgresPolicySnapshotEntry};
use crate::migration::run_migrations;

fn database_url() -> Option<String> {
    match std::env::var("DATABASE_URL") {
        Ok(url) => Some(url),
        Err(_) => {
            eprintln!("skipping Postgres integration test: DATABASE_URL is not set");
            None
        }
    }
}

fn policy(policy_id: &str, cedar_text: &str) -> PostgresPolicySnapshotEntry {
    PostgresPolicySnapshotEntry {
        policy_id: policy_id.to_string(),
        cedar_text: cedar_text.to_string(),
        created_at: "2026-07-11T16:00:00Z".to_string(),
        created_by: "reviewer".to_string(),
        enabled: true,
    }
}

#[test]
fn independent_instances_cannot_publish_same_policy_version() {
    let Some(database_url) = database_url() else {
        return;
    };
    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.expect("connect");
        run_migrations(&pool).await.expect("migrate");
        let first = PostgresEventStore::new(pool.clone());
        let second = PostgresEventStore::new(pool);
        let tenant = format!("policy-race-{}", uuid::Uuid::new_v4());

        let (left, right) = tokio::join!(
            first.replace_policy_snapshot(
                &tenant,
                0,
                vec![policy("left", "permit(principal, action, resource);")],
            ),
            second.replace_policy_snapshot(
                &tenant,
                0,
                vec![policy("right", "forbid(principal, action, resource);")],
            ),
        );
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
            .load_policy_snapshot(&tenant)
            .await
            .expect("load committed snapshot");
        assert_eq!(committed.version, 1);
        assert_eq!(committed.rows.len(), 1);

        let empty_version = first
            .replace_policy_snapshot(&tenant, 1, vec![])
            .await
            .expect("publish authoritative empty snapshot");
        assert_eq!(empty_version, 2);
        let empty = second
            .load_policy_snapshot(&tenant)
            .await
            .expect("load empty snapshot");
        assert_eq!(empty.version, 2);
        assert!(empty.rows.is_empty());
    });
}

#[test]
fn decision_resolution_claim_release_and_terminal_update_are_owner_exact() {
    let Some(database_url) = database_url() else {
        return;
    };
    sqlx::test_block_on(async {
        let pool = PgPool::connect(&database_url).await.expect("connect");
        run_migrations(&pool).await.expect("migrate");
        let first = PostgresEventStore::new(pool.clone());
        let second = PostgresEventStore::new(pool);
        let decision_id = format!("decision-race-{}", uuid::Uuid::new_v4());
        let tenant = format!("decision-tenant-{}", uuid::Uuid::new_v4());
        let pending = serde_json::json!({
            "id": decision_id,
            "tenant": tenant,
            "status": "pending"
        })
        .to_string();
        first
            .upsert_pending_decision(&decision_id, &tenant, "pending", &pending)
            .await
            .expect("seed decision");
        let approve = serde_json::json!({
            "id": decision_id,
            "tenant": tenant,
            "status": "pending",
            "resolution_owner": "approve-owner",
            "resolution_kind": "approve"
        })
        .to_string();
        let deny = serde_json::json!({
            "id": decision_id,
            "tenant": tenant,
            "status": "pending",
            "resolution_owner": "deny-owner",
            "resolution_kind": "deny"
        })
        .to_string();

        let (approve_won, deny_won) = tokio::join!(
            first.claim_decision_resolution(&tenant, &decision_id, &approve),
            second.claim_decision_resolution(&tenant, &decision_id, &deny),
        );
        let approve_won = approve_won.expect("approve claim");
        let deny_won = deny_won.expect("deny claim");
        assert_ne!(approve_won, deny_won);
        let (winner_store, loser_store, winner, loser, winner_json) = if approve_won {
            (&first, &second, "approve-owner", "deny-owner", &approve)
        } else {
            (&second, &first, "deny-owner", "approve-owner", &deny)
        };
        assert!(
            !loser_store
                .release_decision_resolution(&tenant, &decision_id, loser, &pending)
                .await
                .expect("loser release rejected")
        );
        assert!(
            winner_store
                .update_decision_resolution(
                    &tenant,
                    &decision_id,
                    winner,
                    if approve_won { "approved" } else { "denied" },
                    winner_json,
                )
                .await
                .expect("winner completes")
        );
        assert!(
            !loser_store
                .update_decision_resolution(
                    &tenant,
                    &decision_id,
                    loser,
                    if approve_won { "denied" } else { "approved" },
                    winner_json,
                )
                .await
                .expect("loser terminal update rejected")
        );
    });
}
