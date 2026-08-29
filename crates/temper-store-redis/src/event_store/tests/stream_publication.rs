use std::collections::BTreeMap;

use temper_runtime::persistence::schema_deployment::{
    SchemaExecutionPin, SchemaScope, SchemaScopeKind, StreamPublicationFence,
    UnscopedStreamPublicationBinding, scoped_journal_entity_id,
};
use temper_runtime::persistence::{
    EventStore, KernelEventMetadata, PersistenceAppend, PersistenceError, StreamDescriptorInputV1,
    StreamDescriptorV1, StreamEntityRef, StreamMutability, StreamStorageRefV1,
};

use super::*;

#[tokio::test]
async fn historical_index_and_publication_fence_are_bounded_and_atomic() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let unique = uuid::Uuid::new_v4();
    let inventory_tenant = format!("redis-stream-inventory-{unique}");
    let entity_type = "File";
    let pin = SchemaExecutionPin {
        scope: SchemaScope {
            kind: SchemaScopeKind::Task,
            id: "historical".into(),
        },
        bundle_digest: format!("sha256:{}", "1".repeat(64)),
    };
    for index in 0..257 {
        let entity_id = scoped_journal_entity_id(&format!("scoped-{index:03}"), &pin);
        store
            .append(
                &format!("{inventory_tenant}:{entity_type}:{entity_id}"),
                0,
                &[test_envelope("Created", serde_json::json!({}))],
            )
            .await
            .unwrap();
    }
    store
        .append(
            &format!("{inventory_tenant}:{entity_type}:legacy-unscoped"),
            0,
            &[test_envelope("Created", serde_json::json!({}))],
        )
        .await
        .unwrap();
    let _: i64 = store
        .client()
        .del(vec![
            RedisEventStore::unscoped_journals_key(&inventory_tenant, entity_type),
            RedisEventStore::unscoped_index_cursor_key(&inventory_tenant, entity_type),
            RedisEventStore::unscoped_index_complete_key(&inventory_tenant, entity_type),
        ])
        .await
        .unwrap();
    let mut pending_pages = 0;
    let indexed = loop {
        match store
            .list_unscoped_entity_ids_page(&inventory_tenant, entity_type, None, 16)
            .await
        {
            Ok(page) => break page,
            Err(PersistenceError::Storage(message))
                if message.contains("index backfill is pending") =>
            {
                pending_pages += 1;
                assert!(pending_pages <= 3, "bounded backfill did not converge");
            }
            Err(error) => panic!("unexpected index backfill error: {error}"),
        }
    };
    assert!(
        pending_pages >= 1,
        "historical indexing must yield between raw pages"
    );
    assert_eq!(indexed, vec!["legacy-unscoped"]);

    let tenant = format!("redis-stream-fence-{unique}");
    let persistence_id = format!("{tenant}:File:file-1");
    store
        .append(
            &persistence_id,
            0,
            &[test_envelope("Created", serde_json::json!({}))],
        )
        .await
        .unwrap();
    let capability_digest = format!("sha256:{}", "2".repeat(64));
    let fence = StreamPublicationFence::InstalledApplication {
        application_id: "temper-fs".into(),
        semantic_digest: format!("sha256:{}", "a".repeat(64)),
        bindings: BTreeMap::from([(
            "File".into(),
            UnscopedStreamPublicationBinding {
                publication_action: "StreamUpdated".into(),
                capability_digest: capability_digest.clone(),
                expected_write_version: 1,
            },
        )]),
    };
    let mut stale = fence.clone();
    let StreamPublicationFence::InstalledApplication { bindings, .. } = &mut stale else {
        unreachable!();
    };
    bindings.get_mut("File").unwrap().expected_write_version = 0;
    assert!(matches!(
        store
            .activate_unscoped_stream_publication_fence(&tenant, &stale)
            .await,
        Err(PersistenceError::ConcurrencyViolation { .. })
    ));
    store
        .activate_unscoped_stream_publication_fence(&tenant, &fence)
        .await
        .unwrap();
    assert_eq!(
        store
            .get_unscoped_stream_publication_fence(&tenant, "temper-fs")
            .await
            .unwrap(),
        Some(fence.clone())
    );

    let mut descriptorless = test_envelope("StreamUpdated", serde_json::json!({}));
    assert!(matches!(
        store.append(&persistence_id, 1, &[descriptorless.clone()]).await,
        Err(PersistenceError::Storage(message)) if message.contains("publication fence")
    ));
    let descriptor = StreamDescriptorV1::new(StreamDescriptorInputV1 {
        subject: StreamEntityRef::new("File", "file-1").unwrap(),
        authorization_parent: None,
        content_hash: "sha256:abc".into(),
        storage: StreamStorageRefV1::new("temper-fs/sha256:abc").unwrap(),
        byte_length: 3,
        content_type: None,
        content_event_sequence: 2,
        descriptor_event_sequence: 2,
        mutability: StreamMutability::Mutable,
    })
    .unwrap();
    descriptorless.metadata.kernel = Some(KernelEventMetadata::V1 {
        stream_descriptor: descriptor,
    });
    store
        .append(&persistence_id, 1, &[descriptorless])
        .await
        .unwrap();
    store
        .append(
            &persistence_id,
            2,
            &[test_envelope("Touch", serde_json::json!({}))],
        )
        .await
        .unwrap();
    assert!(matches!(
        store
            .append_batch(&[
                PersistenceAppend {
                    persistence_id: persistence_id.clone(),
                    expected_sequence: 3,
                    events: vec![test_envelope("StreamUpdated", serde_json::json!({}))],
                    key_rows: Vec::new(),
                    vector_rows: Vec::new(),
                    reconcile_vectors: false,
                },
                PersistenceAppend {
                    persistence_id: format!("{tenant}:Other:other-1"),
                    expected_sequence: 0,
                    events: vec![test_envelope("Touch", serde_json::json!({}))],
                    key_rows: Vec::new(),
                    vector_rows: Vec::new(),
                    reconcile_vectors: false,
                },
            ])
            .await,
        Err(PersistenceError::Storage(message)) if message.contains("publication fence")
    ));
    assert_eq!(
        store.read_events(&persistence_id, 0).await.unwrap().len(),
        3
    );
    assert!(
        store
            .read_events(&format!("{tenant}:Other:other-1"), 0)
            .await
            .unwrap()
            .is_empty()
    );
    store
        .append(
            &format!("other-{tenant}:File:file-1"),
            0,
            &[test_envelope("StreamUpdated", serde_json::json!({}))],
        )
        .await
        .unwrap();

    let replacement = StreamPublicationFence::InstalledApplication {
        application_id: "temper-fs".into(),
        semantic_digest: format!("sha256:{}", "b".repeat(64)),
        bindings: BTreeMap::from([(
            "File".into(),
            UnscopedStreamPublicationBinding {
                publication_action: "StreamUpdated".into(),
                capability_digest: format!("sha256:{}", "3".repeat(64)),
                expected_write_version: 3,
            },
        )]),
    };
    store
        .activate_unscoped_stream_publication_fence(&tenant, &replacement)
        .await
        .unwrap();
    store
        .restore_unscoped_stream_publication_fence(
            &tenant,
            &format!("sha256:{}", "b".repeat(64)),
            &fence,
        )
        .await
        .unwrap();
    assert!(
        store
            .unscoped_stream_publication_fence_active(
                &tenant,
                "File",
                "StreamUpdated",
                &capability_digest,
            )
            .await
            .unwrap()
    );
    store
        .deactivate_unscoped_stream_publication_fence(
            &tenant,
            "temper-fs",
            Some(&format!("sha256:{}", "a".repeat(64))),
        )
        .await
        .unwrap();
    assert!(
        store
            .get_unscoped_stream_publication_fence(&tenant, "temper-fs")
            .await
            .unwrap()
            .is_none()
    );
}
