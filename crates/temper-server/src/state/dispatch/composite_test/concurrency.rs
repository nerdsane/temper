//! Focused composite-dispatch regression group.

use super::*;

#[cfg(feature = "sim")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn composite_ingest_pack_large_blob_sub_write_persists_overflow_fields() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = SimEventStore::no_faults(44);
    let mut state = composite_test_state_with_store(store.clone());
    state.data_dir = dir.path().to_path_buf();
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");
    let canonical_bytes = "W".repeat(512 * 1024);

    let applied = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "repo-large-blob",
            "IngestPack",
            &json!({
                "sub_writes": [{
                    "entity_type": "Blob",
                    "entity_id": "blob-large-1",
                    "action": "Create",
                    "params": {
                        "RepositoryId": "repo-large-blob",
                        "CanonicalBytes": canonical_bytes
                    }
                }]
            }),
            &agent,
        )
        .await
        .expect("large Blob sub-write should persist through field-overflow");
    assert!(applied);

    let blob = state
        .get_tenant_entity_state(&tenant, "Blob", "blob-large-1")
        .await
        .expect("large blob entity should be readable");
    let canonical_field = blob
        .state
        .fields
        .get("CanonicalBytes")
        .expect("CanonicalBytes field should be present");
    let blob_key = canonical_field
        .get(crate::blobs::FIELD_OVERFLOW_REF_KEY)
        .and_then(serde_json::Value::as_str)
        .expect("large CanonicalBytes should be stored as a field-overflow blob ref");
    let bytes = state
        .get_blob_with_legacy_fallback(&tenant, blob_key)
        .await
        .expect("field-overflow blob read should succeed")
        .expect("field-overflow blob should exist");
    let restored: serde_json::Value =
        serde_json::from_slice(&bytes).expect("field-overflow blob should contain JSON");
    assert_eq!(
        restored.as_str().map(str::len),
        Some(512 * 1024),
        "field-overflow blob should preserve the full large field"
    );

    let blob_journal = store.dump_journal("default:Blob:blob-large-1");
    assert!(
        blob_journal
            .iter()
            .any(|event| event.event_type == "Create"),
        "atomic composite batch should persist the Blob.Create event"
    );
}

#[cfg(feature = "sim")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn composite_atomic_batch_handles_concurrent_multi_entity_results() {
    const COMPOSITES: usize = 12;
    const CHILDREN_PER_COMPOSITE: usize = 3;

    let store = SimEventStore::no_faults(44);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    let mut handles = Vec::new();
    for composite_idx in 0..COMPOSITES {
        let state = state.clone();
        let tenant = tenant.clone();
        let agent = agent.clone();
        handles.push(tokio::spawn(async move {
            let parent_id = format!("parent-stress-{composite_idx}");
            let mut sub_writes = Vec::new();
            for child_idx in 0..CHILDREN_PER_COMPOSITE {
                sub_writes.push(json!({
                    "entity_type": "Child",
                    "entity_id": format!("child-stress-{composite_idx}-{child_idx}"),
                    "action": "Create",
                    "params": {
                        "Name": format!("child {composite_idx}/{child_idx}")
                    }
                }));
            }
            sub_writes.push(json!({
                "entity_type": "App",
                "entity_id": format!("app-stress-{composite_idx}"),
                "action": "Create",
                "params": {
                    "OwnerId": format!("owner-{composite_idx}"),
                    "Name": format!("app-{composite_idx}")
                }
            }));

            let applied = state
                .apply_composite_integration_result(
                    &tenant,
                    "Parent",
                    &parent_id,
                    "CreateChild",
                    &json!({ "sub_writes": sub_writes }),
                    &agent,
                )
                .await
                .map_err(|err| err.to_string())?;
            Ok::<_, String>((parent_id, applied))
        }));
    }

    let mut parent_ids = Vec::new();
    for handle in handles {
        let (parent_id, applied) = handle
            .await
            .expect("concurrent composite task should join")
            .expect("concurrent composite result should apply");
        assert!(applied);
        parent_ids.push(parent_id);
    }

    for parent_id in parent_ids {
        let composite_idx = parent_id
            .strip_prefix("parent-stress-")
            .expect("stress parent id should include numeric suffix")
            .parse::<usize>()
            .expect("stress parent suffix should parse");
        let parent_journal = store.dump_journal(&format!("default:Parent:{parent_id}"));
        assert_eq!(
            parent_journal
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            vec!["Created", COMPOSITE_EVENT_TYPE],
            "parent {parent_id} should record one replay-safe CompositeEvent"
        );
        let composite_event =
            serde_json::from_value::<CompositeEvent>(parent_journal[1].payload.clone())
                .expect("CompositeEvent payload should decode");
        assert_eq!(composite_event.sub_writes.len(), CHILDREN_PER_COMPOSITE + 1);

        for child_idx in 0..CHILDREN_PER_COMPOSITE {
            let child_id = format!("child-stress-{composite_idx}-{child_idx}");
            let child = state
                .get_tenant_entity_state(&tenant, "Child", &child_id)
                .await
                .expect("stress child should be readable");
            assert_eq!(child.state.status, "Active");
            assert_eq!(
                child.state.fields.get("Name"),
                Some(&json!(format!("child {composite_idx}/{child_idx}")))
            );
        }

        let app_id = format!("app-stress-{composite_idx}");
        let app = state
            .get_tenant_entity_state(&tenant, "App", &app_id)
            .await
            .expect("stress app should be readable");
        assert_eq!(
            app.state.fields.get("OwnerId"),
            Some(&json!(format!("owner-{composite_idx}")))
        );
        assert_eq!(
            app.state.fields.get("Name"),
            Some(&json!(format!("app-{composite_idx}")))
        );
    }
}

#[tokio::test]
async fn commons_composite_rejects_duplicate_owner_app_name_before_dispatch() {
    let state = composite_test_state();
    state.enable_commons_guardrails("default");
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    let first = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-app-name",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "App",
                    "entity_id": "app-alice-notes",
                    "action": "Create",
                    "params": { "OwnerId": "alice", "Name": "notes" }
                }]
            }),
            &agent,
        )
        .await
        .expect("first owner/app name should apply");
    assert!(first);

    let err = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-app-name",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "App",
                    "entity_id": "app-alice-notes-copy",
                    "action": "Create",
                    "params": { "OwnerId": "Alice", "Name": "Notes" }
                }]
            }),
            &agent,
        )
        .await
        .expect_err("duplicate owner/app name should be rejected")
        .to_string();

    assert!(
        err.contains("alice/Notes") || err.contains("Alice/Notes"),
        "unexpected error: {err}"
    );
    assert!(!state.entity_exists(&tenant, "App", "app-alice-notes-copy"));
}

#[cfg(feature = "sim")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commons_composite_app_name_uniqueness_serializes_concurrent_creates() {
    let store = SimEventStore::no_faults(43);
    let state = composite_test_state_with_store(store.clone());
    state.enable_commons_guardrails("default");
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");
    let attempts = [
        ("parent-app-race-a", "app-race-a"),
        ("parent-app-race-b", "app-race-b"),
    ];

    let mut handles = Vec::new();
    for (parent_id, app_id) in attempts {
        let state = state.clone();
        let tenant = tenant.clone();
        let agent = agent.clone();
        handles.push(tokio::spawn(async move {
            let result = state
                .apply_composite_integration_result(
                    &tenant,
                    "Parent",
                    parent_id,
                    "CreateChild",
                    &json!({
                        "sub_writes": [{
                            "entity_type": "App",
                            "entity_id": app_id,
                            "action": "Create",
                            "params": { "OwnerId": "alice", "Name": "Notes" }
                        }]
                    }),
                    &agent,
                )
                .await
                .map_err(|err| err.to_string());
            (parent_id.to_string(), app_id.to_string(), result)
        }));
    }

    let mut outcomes = Vec::new();
    for handle in handles {
        outcomes.push(handle.await.expect("concurrent task should finish"));
    }

    let successes = outcomes
        .iter()
        .filter(|(_, _, result)| matches!(result, Ok(true)))
        .count();
    let conflicts = outcomes
        .iter()
        .filter(|(_, _, result)| matches!(result, Err(err) if err.contains("already registered")))
        .count();
    assert_eq!(
        successes, 1,
        "exactly one concurrent composite should create alice/Notes: {outcomes:?}"
    );
    assert_eq!(
        conflicts, 1,
        "the racing composite should fail closed with an app-name conflict: {outcomes:?}"
    );

    let persisted_apps = outcomes
        .iter()
        .filter(|(_, app_id, _)| state.entity_exists(&tenant, "App", app_id))
        .collect::<Vec<_>>();
    assert_eq!(
        persisted_apps.len(),
        1,
        "only the winning App row should exist after the race"
    );

    for (parent_id, app_id, result) in outcomes {
        let parent_journal = store.dump_journal(&format!("default:Parent:{parent_id}"));
        match result {
            Ok(true) => {
                assert_eq!(
                    parent_journal
                        .iter()
                        .map(|event| event.event_type.as_str())
                        .collect::<Vec<_>>(),
                    vec!["Created", COMPOSITE_EVENT_TYPE],
                    "winning parent should record exactly one CompositeEvent"
                );
                let app = state
                    .get_tenant_entity_state(&tenant, "App", &app_id)
                    .await
                    .expect("winning app should be readable");
                assert_eq!(app.state.fields.get("OwnerId"), Some(&json!("alice")));
                assert_eq!(app.state.fields.get("Name"), Some(&json!("Notes")));
            }
            Err(err) => {
                assert!(
                    err.contains("already registered"),
                    "unexpected losing result: {err}"
                );
                assert!(
                    parent_journal.is_empty(),
                    "losing parent journal must remain empty when uniqueness preflight rejects it"
                );
                assert!(
                    !state.entity_exists(&tenant, "App", &app_id),
                    "losing App row must not be persisted"
                );
            }
            Ok(false) => panic!("composite should not fall back for simple App.Create"),
        }
    }
}

#[tokio::test]
async fn composite_integration_result_rejects_undeclared_sub_write() {
    let state = composite_test_state();
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    let err = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-1",
            "CreateChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Parent",
                    "entity_id": "parent-2",
                    "action": "CreateChild",
                    "params": {}
                }]
            }),
            &agent,
        )
        .await
        .expect_err("undeclared sub-write should be rejected");

    let err = err.to_string();
    assert!(err.contains("is not declared"), "unexpected error: {err}");
}
