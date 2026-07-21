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

fn encoded_entity_ref(entity_type: &str, entity_id: &str) -> String {
    serde_json::to_string(&EntityRef {
        entity_type: entity_type.to_string(),
        entity_id: entity_id.to_string(),
    })
    .expect("encode entity reference")
}

async fn append_legacy_event(
    store: &RedisEventStore,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    sequence_nr: u64,
    mut event: PersistenceEnvelope,
) {
    event.sequence_nr = sequence_nr;
    let encoded = serde_json::to_string(&event).expect("encode legacy event");
    let _: i64 = store
        .client
        .rpush(
            RedisEventStore::events_key(tenant, entity_type, entity_id),
            encoded,
        )
        .await
        .expect("append legacy event");
    let _: () = store
        .client
        .set(
            RedisEventStore::seq_key(tenant, entity_type, entity_id),
            sequence_nr.to_string(),
            None,
            None,
            false,
        )
        .await
        .expect("advance legacy sequence");
    let _: i64 = store
        .client
        .sadd(
            RedisEventStore::tenant_entities_key(tenant),
            encoded_entity_ref(entity_type, entity_id),
        )
        .await
        .expect("retain historical entity reference");
}

async fn seed_completed_entity_then_append_legacy_tombstone(
    store: &RedisEventStore,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
) {
    store
        .append(
            &format!("{tenant}:{entity_type}:{entity_id}"),
            0,
            &[test_envelope("Created", serde_json::json!({}))],
        )
        .await
        .expect("seed current indexed writer");
    assert_eq!(
        store.list_entity_ids(tenant).await.unwrap(),
        vec![(entity_type.to_string(), entity_id.to_string())]
    );
    assert!(store.entity_index_is_complete(tenant).await.unwrap());
    let entity_ref = encoded_entity_ref(entity_type, entity_id);
    let classified_sequence: String = store
        .client
        .get(RedisEventStore::entity_index_event_cursor_key(
            tenant,
            &entity_ref,
        ))
        .await
        .expect("read current writer classification cursor");
    assert_eq!(classified_sequence, "1");

    // Reproduce the old append script during a rolling upgrade: it advances
    // the existing journal and historical set, but knows nothing about the
    // live or tombstone indexes introduced by the new writer.
    append_legacy_event(
        store,
        tenant,
        entity_type,
        entity_id,
        2,
        test_envelope("Deleted", serde_json::Value::Null),
    )
    .await;
    let classified_sequence: String = store
        .client
        .get(RedisEventStore::entity_index_event_cursor_key(
            tenant,
            &entity_ref,
        ))
        .await
        .expect("read stale classification cursor");
    assert_eq!(
        classified_sequence, "1",
        "the legacy append must leave the classified head behind its sequence"
    );
}

#[tokio::test]
async fn current_writer_preserves_a_missing_legacy_classification_cursor() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let tenant = format!("legacy-cursorless-update-{}", uuid::Uuid::new_v4());
    let entity_type = "Order";
    let entity_id = "cursorless-before-current-update";
    let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
    store
        .append(
            &persistence_id,
            0,
            &[test_envelope("Created", serde_json::json!({}))],
        )
        .await
        .expect("seed current live index");
    let cursor_key = RedisEventStore::entity_index_event_cursor_key(
        &tenant,
        &encoded_entity_ref(entity_type, entity_id),
    );
    let _: i64 = store
        .client
        .del(cursor_key.clone())
        .await
        .expect("simulate a journal written before classified cursors");

    append_legacy_event(
        &store,
        &tenant,
        entity_type,
        entity_id,
        2,
        test_envelope("Deleted", serde_json::Value::Null),
    )
    .await;
    store
        .append(
            &persistence_id,
            2,
            &[test_envelope("Touched", serde_json::json!({}))],
        )
        .await
        .expect("current writer races before legacy history is reclassified");

    let classified_sequence: Option<String> = store
        .client
        .get(&cursor_key)
        .await
        .expect("read classification cursor after current append");
    assert_eq!(
        classified_sequence, None,
        "an existing cursorless journal must remain eligible for full legacy reclassification"
    );
    assert_eq!(
        store.list_entity_ids(&tenant).await.unwrap(),
        Vec::<(String, String)>::new(),
        "the later current append cannot hide the earlier legacy tombstone"
    );
}

#[tokio::test]
async fn current_writer_preserves_a_stale_legacy_classification_cursor() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let tenant = format!("legacy-stale-cursor-update-{}", uuid::Uuid::new_v4());
    let entity_type = "Order";
    let entity_id = "stale-cursor-before-current-update";
    let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
    store
        .append(
            &persistence_id,
            0,
            &[test_envelope("Created", serde_json::json!({}))],
        )
        .await
        .expect("seed classified live index");
    let cursor_key = RedisEventStore::entity_index_event_cursor_key(
        &tenant,
        &encoded_entity_ref(entity_type, entity_id),
    );

    append_legacy_event(
        &store,
        &tenant,
        entity_type,
        entity_id,
        2,
        test_envelope("Deleted", serde_json::Value::Null),
    )
    .await;
    store
        .append(
            &persistence_id,
            2,
            &[test_envelope("Touched", serde_json::json!({}))],
        )
        .await
        .expect("current writer races before stale legacy suffix is reclassified");

    let classified_sequence: Option<String> = store
        .client
        .get(&cursor_key)
        .await
        .expect("read classification cursor after current append");
    assert_eq!(
        classified_sequence.as_deref(),
        Some("1"),
        "a stale cursor must remain behind every unclassified legacy event"
    );
    assert_eq!(
        store.list_entity_ids(&tenant).await.unwrap(),
        Vec::<(String, String)>::new(),
        "the current append cannot skip the legacy tombstone ahead of its stale cursor"
    );
}

#[tokio::test]
async fn completed_index_reclassifies_a_mixed_version_tombstone() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let entity_type = "Order";

    let global_tenant = format!("legacy-tombstone-global-{}", uuid::Uuid::new_v4());
    seed_completed_entity_then_append_legacy_tombstone(
        &store,
        &global_tenant,
        entity_type,
        "global-list",
    )
    .await;

    assert_eq!(
        store.list_entity_ids(&global_tenant).await.unwrap(),
        Vec::<(String, String)>::new(),
        "a completed index must not trust cardinality after a legacy writer tombstones a known journal"
    );

    let typed_tenant = format!("legacy-tombstone-typed-{}", uuid::Uuid::new_v4());
    seed_completed_entity_then_append_legacy_tombstone(
        &store,
        &typed_tenant,
        entity_type,
        "typed-list",
    )
    .await;
    assert_eq!(
        store
            .list_entity_ids_by_type(&typed_tenant, entity_type)
            .await
            .unwrap(),
        Vec::<String>::new(),
        "typed listings must remove the stale live member too"
    );

    let bounded_tenant = format!("legacy-tombstone-bounded-{}", uuid::Uuid::new_v4());
    seed_completed_entity_then_append_legacy_tombstone(
        &store,
        &bounded_tenant,
        entity_type,
        "bounded-list",
    )
    .await;
    assert_eq!(
        store
            .list_entity_ids_limited(&bounded_tenant, Some(entity_type), 1)
            .await
            .unwrap(),
        Vec::<(String, String)>::new(),
        "bounded listings must reclassify before returning a live member"
    );
}
