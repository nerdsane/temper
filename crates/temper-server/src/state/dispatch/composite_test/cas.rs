//! Focused composite-dispatch regression group.

use super::*;

#[cfg(feature = "sim")]
#[tokio::test]
async fn parent_gated_pack_object_create_repairs_partial_existing_object() {
    let store = SimEventStore::no_faults(40);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let mut agent = AgentContext::for_service("composite-test");
    agent.idempotency_key = Some("legacy-partial-pack".to_string());
    let blob_id = "rp-test-abc123";
    let blob_pid = format!("default:Blob:{blob_id}");

    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "repo-test",
            "IngestPack",
            &json!({
                "sub_writes": [{
                    "entity_type": "Blob",
                    "entity_id": blob_id,
                    "action": "Create",
                    "params": {
                        "Id": "abc123",
                        "RepositoryId": "rp-test"
                    }
                }]
            }),
            &agent,
        )
        .await
        .expect("partial legacy pack object should stage");

    assert_eq!(store.dump_journal(&blob_pid).len(), 1);

    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "repo-test",
            "IngestPack",
            &json!({
                "sub_writes": [{
                    "entity_type": "Blob",
                    "entity_id": blob_id,
                    "action": "Create",
                    "params": {
                        "Id": "abc123",
                        "RepositoryId": "rp-test",
                        "CanonicalBytes": "YmxvYiAwAA=="
                    }
                }]
            }),
            &agent,
        )
        .await
        .expect("complete pack object should repair the partial stream");

    let blob = state
        .get_tenant_entity_state(&tenant, "Blob", blob_id)
        .await
        .expect("repaired blob should be readable");
    assert_eq!(
        blob.state.fields.get("CanonicalBytes"),
        Some(&json!("YmxvYiAwAA=="))
    );
    assert_eq!(
        store.dump_journal(&blob_pid).len(),
        2,
        "repair appends at the current sequence instead of expecting zero"
    );
}
#[tokio::test]
async fn parent_gated_pack_object_create_skips_complete_existing_object() {
    let store = SimEventStore::no_faults(40);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let mut agent = AgentContext::for_service("composite-test");
    let blob_id = "rp-test-def456";
    let blob_pid = format!("default:Blob:{blob_id}");
    let callback_params = json!({
        "sub_writes": [{
            "entity_type": "Blob",
            "entity_id": blob_id,
            "action": "Create",
            "params": {
                "Id": "def456",
                "RepositoryId": "rp-test",
                "CanonicalBytes": "YmxvYiAwAA=="
            }
        }]
    });

    agent.idempotency_key = Some("first-pack".to_string());
    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "repo-test",
            "IngestPack",
            &callback_params,
            &agent,
        )
        .await
        .expect("first complete object write should append");
    let first_len = store.dump_journal(&blob_pid).len();

    agent.idempotency_key = Some("second-pack".to_string());
    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "repo-test",
            "IngestPack",
            &callback_params,
            &agent,
        )
        .await
        .expect("complete duplicate object should no-op");

    assert_eq!(
        store.dump_journal(&blob_pid).len(),
        first_len,
        "complete pack objects should not accumulate duplicate Create events"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_ref_create_cas_rejects_existing_ref_without_pack_object_leak() {
    let store = SimEventStore::no_faults(40);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");
    let ref_id = "ref-main-create-cas";
    let old_sha = "1111111111111111111111111111111111111111";
    let new_sha = "2222222222222222222222222222222222222222";

    let created = state
        .dispatch_tenant_action(
            &tenant,
            "Ref",
            ref_id,
            "Create",
            json!({
                "RepositoryId": "repo-test",
                "Name": "refs/heads/main",
                "TargetCommitSha": old_sha,
                "Kind": "branch"
            }),
            &agent,
        )
        .await
        .expect("existing ref create should run");
    assert!(created.success);

    let err = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "repo-test",
            "IngestPack",
            &json!({
                "sub_writes": [
                    {
                        "entity_type": "Blob",
                        "entity_id": "repo-test-cas-create-blob",
                        "action": "Create",
                        "params": {
                            "Id": "cas-create-blob",
                            "RepositoryId": "repo-test",
                            "CanonicalBytes": "YmxvYiAwAA=="
                        }
                    },
                    {
                        "entity_type": "Ref",
                        "entity_id": ref_id,
                        "action": "Create",
                        "params": {
                            "RepositoryId": "repo-test",
                            "Name": "refs/heads/main",
                            "PreviousCommitSha": "0000000000000000000000000000000000000000",
                            "TargetCommitSha": new_sha,
                            "Kind": "branch"
                        }
                    }
                ]
            }),
            &agent,
        )
        .await
        .expect_err("stale ref create should fail before appending pack objects")
        .to_string();

    assert!(err.contains("stale ref"), "unexpected error: {err}");
    assert!(
        store
            .dump_journal("default:Blob:repo-test-cas-create-blob")
            .is_empty(),
        "losing pack object must not persist when the ref create CAS fails"
    );
    let ref_state = state
        .get_tenant_entity_state(&tenant, "Ref", ref_id)
        .await
        .expect("original ref should remain readable");
    assert_eq!(
        ref_state.state.fields.get("TargetCommitSha"),
        Some(&json!(old_sha))
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_ref_update_cas_rejects_stale_previous_without_pack_object_leak() {
    let store = SimEventStore::no_faults(40);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");
    let ref_id = "ref-main-update-cas";
    let current_sha = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let stale_sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let new_sha = "cccccccccccccccccccccccccccccccccccccccc";

    let created = state
        .dispatch_tenant_action(
            &tenant,
            "Ref",
            ref_id,
            "Create",
            json!({
                "RepositoryId": "repo-test",
                "Name": "refs/heads/main",
                "TargetCommitSha": current_sha,
                "Kind": "branch"
            }),
            &agent,
        )
        .await
        .expect("existing ref create should run");
    assert!(created.success);

    let err = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "repo-test",
            "IngestPack",
            &json!({
                "sub_writes": [
                    {
                        "entity_type": "Blob",
                        "entity_id": "repo-test-cas-update-blob",
                        "action": "Create",
                        "params": {
                            "Id": "cas-update-blob",
                            "RepositoryId": "repo-test",
                            "CanonicalBytes": "YmxvYiAxAA=="
                        }
                    },
                    {
                        "entity_type": "Ref",
                        "entity_id": ref_id,
                        "action": "Update",
                        "params": {
                            "PreviousCommitSha": stale_sha,
                            "NewCommitSha": new_sha,
                            "TargetCommitSha": new_sha
                        }
                    }
                ]
            }),
            &agent,
        )
        .await
        .expect_err("stale ref update should fail before appending pack objects")
        .to_string();

    assert!(err.contains("stale ref"), "unexpected error: {err}");
    assert!(
        store
            .dump_journal("default:Blob:repo-test-cas-update-blob")
            .is_empty(),
        "losing pack object must not persist when the ref update CAS fails"
    );
    let ref_state = state
        .get_tenant_entity_state(&tenant, "Ref", ref_id)
        .await
        .expect("original ref should remain readable");
    assert_eq!(
        ref_state.state.fields.get("TargetCommitSha"),
        Some(&json!(current_sha))
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_sub_write_idempotency_survives_actor_restart() {
    let store = SimEventStore::no_faults(40);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");
    let callback_params = json!({
        "sub_writes": [{
            "entity_type": "Child",
            "entity_id": "child-replay",
            "action": "Create",
            "params": { "Name": "created once" }
        }]
    });

    let applied = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-1",
            "CreateChild",
            &callback_params,
            &agent,
        )
        .await
        .expect("first composite result should apply");
    assert!(applied);

    let child_pid = "default:Child:child-replay";
    let first_journal_len = store.dump_journal(child_pid).len();
    assert!(
        first_journal_len >= 2,
        "child journal should contain bootstrap + Create event"
    );

    let restarted = composite_test_state_with_store(store.clone());
    let replayed = restarted
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-1",
            "CreateChild",
            &callback_params,
            &agent,
        )
        .await
        .expect("duplicate composite result should be idempotent after replay");
    assert!(replayed);

    let child = restarted
        .get_tenant_entity_state(&tenant, "Child", "child-replay")
        .await
        .expect("child should still be readable");
    assert_eq!(child.state.status, "Active");
    assert_eq!(child.state.fields.get("Name"), Some(&json!("created once")));
    assert_eq!(
        store.dump_journal(child_pid).len(),
        first_journal_len,
        "duplicate sub-write should not append a second Create event"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_atomic_batch_allows_existing_sub_write_to_delete_target() {
    let store = SimEventStore::no_faults(40);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");
    let child_id = "child-delete-through-composite";

    let created = state
        .dispatch_tenant_action(
            &tenant,
            "Child",
            child_id,
            "Create",
            json!({ "Name": "temporary child" }),
            &agent,
        )
        .await
        .expect("child create should run");
    assert!(created.success);
    assert!(state.entity_exists(&tenant, "Child", child_id));

    let applied = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-delete-child",
            "DeleteChild",
            &json!({
                "sub_writes": [{
                    "entity_type": "Child",
                    "entity_id": child_id,
                    "action": "Delete",
                    "params": {}
                }]
            }),
            &agent,
        )
        .await
        .expect("composite delete sub-write should commit without reloading a tombstone");
    assert!(applied);

    assert!(
        !state.ensure_entity_loaded(&tenant, "Child", child_id).await,
        "deleted composite sub-write target should not be reloaded as a live entity"
    );
    assert!(!state.entity_exists(&tenant, "Child", child_id));

    let child_journal = store.dump_journal(&format!("default:Child:{child_id}"));
    assert_eq!(
        child_journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Create", "Deleted"]
    );
}
