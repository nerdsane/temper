//! Focused Redis event-store regression group.

use super::*;

#[tokio::test]
async fn bounded_listing_migrates_legacy_entity_metadata() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let tenant = format!("legacy-index-migration-{}", uuid::Uuid::new_v4());
    seed_legacy_entity(
        &store,
        &tenant,
        "Order",
        "active",
        test_envelope("Created", serde_json::json!({})),
    )
    .await;
    seed_legacy_entity(
        &store,
        &tenant,
        "Order",
        "deleted-at-sequence-one",
        test_envelope("Deleted", serde_json::json!({})),
    )
    .await;

    let first = store
        .list_entity_ids_limited(&tenant, Some("Order"), 1)
        .await
        .expect_err("a partial migration must not look authoritative");
    assert!(first.to_string().contains("migration incomplete"));
    let mut bounded = None;
    for _ in 0..8 {
        match store
            .list_entity_ids_limited(&tenant, Some("Order"), 1)
            .await
        {
            Ok(result) => {
                bounded = Some(result);
                break;
            }
            Err(PersistenceError::Storage(message)) if message.contains("migration incomplete") => {
            }
            Err(error) => panic!("unexpected bounded migration error: {error}"),
        }
    }
    assert_eq!(
        bounded.expect("bounded migration completes within its retry budget"),
        vec![("Order".to_string(), "active".to_string())]
    );
    assert!(store.entity_index_is_complete(&tenant).await.unwrap());
    assert_eq!(
        store.list_entity_ids(&tenant).await.unwrap(),
        vec![("Order".to_string(), "active".to_string())]
    );
}

#[tokio::test]
async fn bounded_legacy_migration_resumes_long_journal_and_accepts_scalar_payloads() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let tenant = format!("legacy-long-journal-{}", uuid::Uuid::new_v4());
    let entity_type = "Order";
    let entity_id = "deleted-after-page";
    seed_legacy_entity(
        &store,
        &tenant,
        entity_type,
        entity_id,
        test_envelope("Created", serde_json::Value::Null),
    )
    .await;

    let mut encoded_tail = Vec::new();
    for sequence_nr in 2..=64 {
        let mut event = test_envelope("Transitioned", serde_json::json!(sequence_nr));
        event.sequence_nr = sequence_nr;
        encoded_tail.push(serde_json::to_string(&event).expect("encode scalar event"));
    }
    let mut deleted = test_envelope("Deleted", serde_json::Value::Bool(false));
    deleted.sequence_nr = 65;
    encoded_tail.push(serde_json::to_string(&deleted).expect("encode terminal event"));
    let _: i64 = store
        .client
        .rpush(
            RedisEventStore::events_key(&tenant, entity_type, entity_id),
            encoded_tail,
        )
        .await
        .expect("extend legacy journal");
    let _: () = store
        .client
        .set(
            RedisEventStore::seq_key(&tenant, entity_type, entity_id),
            "65",
            None,
            None,
            false,
        )
        .await
        .expect("advance legacy sequence");

    let first = store
        .list_entity_ids_limited(&tenant, Some(entity_type), 1)
        .await
        .expect_err("the first 64-event chunk is not authoritative");
    assert!(first.to_string().contains("migration incomplete"));
    assert_eq!(
        store
            .list_entity_ids_limited(&tenant, Some(entity_type), 1)
            .await
            .expect("the second chunk reaches the terminal event"),
        Vec::<(String, String)>::new()
    );
    assert!(store.entity_index_is_complete(&tenant).await.unwrap());
}

#[tokio::test]
async fn completion_marker_revalidates_a_mixed_version_new_entity() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let tenant = format!("legacy-marker-revalidation-{}", uuid::Uuid::new_v4());
    store
        .append(
            &format!("{tenant}:Order:current"),
            0,
            &[test_envelope("Created", serde_json::json!({}))],
        )
        .await
        .expect("seed current indexed writer");
    assert!(store.entity_index_is_complete(&tenant).await.unwrap());

    seed_legacy_entity(
        &store,
        &tenant,
        "Order",
        "legacy-new",
        test_envelope("Created", serde_json::Value::Null),
    )
    .await;
    assert!(
        !store.entity_index_is_complete(&tenant).await.unwrap(),
        "historical/index cardinality drift must invalidate completion"
    );

    let mut result = None;
    for _ in 0..8 {
        match store.list_entity_ids_limited(&tenant, None, 10).await {
            Ok(entities) => {
                result = Some(entities);
                break;
            }
            Err(PersistenceError::Storage(message)) if message.contains("migration incomplete") => {
            }
            Err(error) => panic!("unexpected marker migration error: {error}"),
        }
    }
    assert_eq!(
        result.expect("mixed-version migration completes"),
        vec![
            ("Order".to_string(), "current".to_string()),
            ("Order".to_string(), "legacy-new".to_string()),
        ]
    );
}

#[tokio::test]
async fn completed_index_reclassifies_a_mixed_version_tombstone() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let tenant = format!("legacy-tombstone-revalidation-{}", uuid::Uuid::new_v4());
    let entity_type = "Order";
    let entity_id = "known-before-legacy-delete";
    store
        .append(
            &format!("{tenant}:{entity_type}:{entity_id}"),
            0,
            &[test_envelope("Created", serde_json::json!({}))],
        )
        .await
        .expect("seed current indexed writer");
    assert_eq!(
        store.list_entity_ids(&tenant).await.unwrap(),
        vec![(entity_type.to_string(), entity_id.to_string())]
    );
    assert!(store.entity_index_is_complete(&tenant).await.unwrap());

    // Reproduce the old append script during a rolling upgrade: it advances
    // the existing journal and historical set, but knows nothing about the
    // live or tombstone indexes introduced by the new writer.
    let mut deleted = test_envelope("Deleted", serde_json::Value::Null);
    deleted.sequence_nr = 2;
    let encoded = serde_json::to_string(&deleted).expect("encode legacy tombstone");
    let _: i64 = store
        .client
        .rpush(
            RedisEventStore::events_key(&tenant, entity_type, entity_id),
            encoded,
        )
        .await
        .expect("append legacy tombstone");
    let _: () = store
        .client
        .set(
            RedisEventStore::seq_key(&tenant, entity_type, entity_id),
            "2",
            None,
            None,
            false,
        )
        .await
        .expect("advance legacy sequence");
    let entity_ref = serde_json::to_string(&EntityRef {
        entity_type: entity_type.to_string(),
        entity_id: entity_id.to_string(),
    })
    .expect("encode entity reference");
    let _: i64 = store
        .client
        .sadd(RedisEventStore::tenant_entities_key(&tenant), entity_ref)
        .await
        .expect("retain historical entity reference");

    assert_eq!(
        store.list_entity_ids(&tenant).await.unwrap(),
        Vec::<(String, String)>::new(),
        "a completed index must not trust cardinality after a legacy writer tombstones a known journal"
    );
    assert_eq!(
        store
            .list_entity_ids_by_type(&tenant, entity_type)
            .await
            .unwrap(),
        Vec::<String>::new(),
        "typed listings must remove the stale live member too"
    );
}
