//! Hard-work-budget regressions for Redis legacy-index migration.

use super::*;

#[tokio::test]
async fn bounded_migration_discovers_at_most_the_requested_compact_set_members() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let tenant = format!("legacy-compact-set-budget-{}", uuid::Uuid::new_v4());
    const LEGACY_ENTITY_COUNT: usize = 16;

    for index in 0..LEGACY_ENTITY_COUNT {
        seed_legacy_entity(
            &store,
            &tenant,
            "Order",
            &format!("compact-{index:02}"),
            test_envelope("Created", serde_json::json!({})),
        )
        .await;
    }

    assert!(
        !store.migrate_entity_index_page(&tenant, 1).await.unwrap(),
        "one bounded call cannot classify the whole legacy tenant"
    );

    let pending: i64 = store
        .client
        .zcard(RedisEventStore::entity_index_pending_key(&tenant))
        .await
        .expect("count parked migration references");
    let live: i64 = store
        .client
        .zcard(RedisEventStore::tenant_live_entities_key(&tenant))
        .await
        .expect("count classified live references");
    let terminal: i64 = store
        .client
        .scard(RedisEventStore::tenant_tombstones_key(&tenant))
        .await
        .expect("count classified terminal references");

    assert!(
        pending + live + terminal <= 1,
        "a one-reference migration budget discovered {} compact-set members",
        pending + live + terminal
    );
}
