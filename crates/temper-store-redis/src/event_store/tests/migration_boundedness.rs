//! Hard-work-budget regressions for Redis legacy-index migration.

use super::*;

#[tokio::test]
async fn bounded_migration_drains_a_compact_scan_page_monotonically() {
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
    let discovered: i64 = store
        .client
        .scard(RedisEventStore::entity_index_discovered_key(&tenant))
        .await
        .expect("count durably discovered references");
    let historical: i64 = store
        .client
        .scard(RedisEventStore::tenant_entities_key(&tenant))
        .await
        .expect("count legacy-visible historical references");

    assert_eq!(
        pending + live + terminal,
        1,
        "one call must make exactly one bounded unit of classification progress"
    );
    assert_eq!(
        discovered, 1,
        "the durable discovery frontier must advance by exactly the budget"
    );
    assert_eq!(
        historical, LEGACY_ENTITY_COUNT as i64,
        "bounded scanning must preserve the complete legacy SMEMBERS view"
    );

    for _ in 1..LEGACY_ENTITY_COUNT {
        store
            .migrate_entity_index_page(&tenant, 1)
            .await
            .expect("drain one durably parked reference");
    }
    assert!(
        store.entity_index_is_complete(&tenant).await.unwrap(),
        "the durable spill must complete in exactly one call per compact member"
    );
}

#[tokio::test]
async fn oversized_compact_scan_page_is_rejected_before_it_is_parked() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let tenant = format!("legacy-compact-set-cap-{}", uuid::Uuid::new_v4());

    for index in 0..16 {
        seed_legacy_entity(
            &store,
            &tenant,
            "Order",
            &format!("compact-{index:02}"),
            test_envelope("Created", serde_json::json!({})),
        )
        .await;
    }

    let result: Result<Vec<String>, _> = store
        .migrate_index_page_script
        .evalsha_with_reload(
            &store.client,
            vec![
                RedisEventStore::tenant_entities_key(&tenant),
                RedisEventStore::entity_index_pending_key(&tenant),
                RedisEventStore::entity_index_cursor_key(&tenant),
                RedisEventStore::entity_index_scan_complete_key(&tenant),
                RedisEventStore::entity_index_discovered_key(&tenant),
                RedisEventStore::entity_index_scan_spill_key(&tenant),
            ],
            vec!["1", "8"],
        )
        .await;
    let error = result.expect_err("the compact encoding exceeds the hard page cap");
    assert!(
        error.to_string().contains(
            "compact entity set exceeds migration page budget; offline migration required"
        )
    );

    let discovered: i64 = store
        .client
        .scard(RedisEventStore::entity_index_discovered_key(&tenant))
        .await
        .expect("count discovered references after rejection");
    let spilled: i64 = store
        .client
        .llen(RedisEventStore::entity_index_scan_spill_key(&tenant))
        .await
        .expect("count parked references after rejection");
    let cursor: Option<String> = store
        .client
        .get(RedisEventStore::entity_index_cursor_key(&tenant))
        .await
        .expect("read cursor after rejection");
    let scan_complete: i64 = store
        .client
        .exists(RedisEventStore::entity_index_scan_complete_key(&tenant))
        .await
        .expect("read scan completion after rejection");
    assert_eq!(
        (discovered, spilled, cursor, scan_complete),
        (0, 0, None, 0)
    );
}
