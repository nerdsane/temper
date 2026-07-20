//! Focused composite-dispatch regression group.

use super::*;

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_preflights_sub_write_auth_before_persisting_any_write() {
    let store = SimEventStore::no_faults(40);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    state
        .authz
        .reload_policies(
            r#"
                permit(
                  principal is Agent,
                  action == Action::"Create",
                  resource is Child
                ) when {
                  principal has action_context &&
                  principal.action_context == "composite:Parent.CreateChild" &&
                  resource.id == "child-preflight-first"
                };
                "#,
        )
        .expect("policy should load");

    let err = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-preflight",
            "CreateChild",
            &json!({
                "sub_writes": [
                    {
                        "entity_type": "Child",
                        "entity_id": "child-preflight-first",
                        "action": "Create",
                        "params": { "Name": "would be allowed" }
                    },
                    {
                        "entity_type": "Child",
                        "entity_id": "child-preflight-denied",
                        "action": "Create",
                        "params": { "Name": "should be denied" }
                    }
                ]
            }),
            &agent,
        )
        .await
        .expect_err("second sub-write should be denied during preflight")
        .to_string();

    assert!(
        err.contains("sub-write 1 denied"),
        "unexpected error: {err}"
    );
    assert!(
        store
            .dump_journal("default:Child:child-preflight-first")
            .is_empty(),
        "authorized earlier sub-write should not be persisted before later preflight denial"
    );
    assert!(
        store
            .dump_journal("default:Child:child-preflight-denied")
            .is_empty(),
        "denied sub-write should not be persisted"
    );
    assert!(!state.entity_exists(&tenant, "Child", "child-preflight-first"));
    assert!(!state.entity_exists(&tenant, "Child", "child-preflight-denied"));
}
#[tokio::test]
async fn composite_preflights_sub_write_transition_before_persisting_any_write() {
    let store = SimEventStore::no_faults(41);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    let existing = state
        .dispatch_tenant_action(
            &tenant,
            "Child",
            "child-transition-existing",
            "Create",
            json!({ "Name": "already active" }),
            &agent,
        )
        .await
        .expect("existing child create should run");
    assert!(existing.success);

    let err = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-transition-preflight",
            "CreateChild",
            &json!({
                "sub_writes": [
                    {
                        "entity_type": "Child",
                        "entity_id": "child-transition-first",
                        "action": "Create",
                        "params": { "Name": "would otherwise persist first" }
                    },
                    {
                        "entity_type": "Child",
                        "entity_id": "child-transition-existing",
                        "action": "Create",
                        "params": { "Name": "invalid from Active" }
                    }
                ]
            }),
            &agent,
        )
        .await
        .expect_err("second sub-write should fail transition preflight")
        .to_string();

    assert!(
        err.contains("sub-write 1 would fail"),
        "unexpected error: {err}"
    );
    assert!(
        store
            .dump_journal("default:Child:child-transition-first")
            .is_empty(),
        "earlier sub-write should not persist before later transition preflight failure"
    );
    assert!(
        !state.entity_exists(&tenant, "Child", "child-transition-first"),
        "earlier sub-write actor should not be spawned"
    );
    assert_eq!(
        store
            .dump_journal("default:Child:child-transition-existing")
            .len(),
        2,
        "existing target should keep only its bootstrap and original Create events"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_atomic_batch_conflict_leaves_all_sub_write_journals_empty() {
    let store = SimEventStore::no_faults(42);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");

    store.inject_concurrency_violations("default:Child:child-atomic-second", 1);

    let err = state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-atomic-batch",
            "CreateChild",
            &json!({
                "sub_writes": [
                    {
                        "entity_type": "Child",
                        "entity_id": "child-atomic-first",
                        "action": "Create",
                        "params": { "Name": "must not persist" }
                    },
                    {
                        "entity_type": "Child",
                        "entity_id": "child-atomic-second",
                        "action": "Create",
                        "params": { "Name": "injected conflict" }
                    }
                ]
            }),
            &agent,
        )
        .await
        .expect_err("atomic batch conflict should reject the whole composite")
        .to_string();

    assert!(
        err.contains("composite batch persistence conflict"),
        "unexpected error: {err}"
    );
    assert!(
        store
            .dump_journal("default:Child:child-atomic-first")
            .is_empty(),
        "first sub-write journal must stay empty when a later stream conflicts"
    );
    assert!(
        store
            .dump_journal("default:Child:child-atomic-second")
            .is_empty(),
        "conflicting sub-write journal must also stay empty"
    );
    assert!(!state.entity_exists(&tenant, "Child", "child-atomic-first"));
    assert!(!state.entity_exists(&tenant, "Child", "child-atomic-second"));
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_atomic_batch_records_parent_composite_event_once() {
    let store = SimEventStore::no_faults(40);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");
    let callback_params = json!({
        "sub_writes": [{
            "entity_type": "Child",
            "entity_id": "child-composite-event",
            "action": "Create",
            "params": { "Name": "recorded through CompositeEvent" }
        }]
    });

    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-composite-event",
            "CreateChild",
            &callback_params,
            &agent,
        )
        .await
        .expect("composite result should apply");

    let parent_pid = "default:Parent:parent-composite-event";
    let parent_journal = store.dump_journal(parent_pid);
    assert_eq!(
        parent_journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", COMPOSITE_EVENT_TYPE]
    );
    let composite_event =
        serde_json::from_value::<CompositeEvent>(parent_journal[1].payload.clone())
            .expect("CompositeEvent payload should decode");
    assert_eq!(composite_event.parent_entity_type, "Parent");
    assert_eq!(composite_event.parent_entity_id, "parent-composite-event");
    assert_eq!(composite_event.parent_action, "CreateChild");
    assert_eq!(composite_event.sub_writes.len(), 1);
    assert_eq!(composite_event.sub_writes[0].entity_type, "Child");
    assert_eq!(
        composite_event.sub_writes[0].entity_id,
        "child-composite-event"
    );
    assert_eq!(composite_event.sub_writes[0].action, "Create");
    assert!(
        composite_event.sub_writes[0]
            .idempotency_key
            .contains("subwrite:0")
    );

    let restarted = composite_test_state_with_store(store.clone());
    let parent = restarted
        .get_tenant_entity_state(&tenant, "Parent", "parent-composite-event")
        .await
        .expect("parent should hydrate from journal");
    assert_eq!(parent.state.status, "Active");
    assert_eq!(parent.state.sequence_nr, 2);
    assert!(parent.state.fields.get("sub_writes").is_none());

    restarted
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-composite-event",
            "CreateChild",
            &callback_params,
            &agent,
        )
        .await
        .expect("duplicate composite result should be idempotent");
    assert_eq!(
        store.dump_journal(parent_pid).len(),
        parent_journal.len(),
        "duplicate composite callback must not append a second CompositeEvent"
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn composite_atomic_batch_can_skip_parent_composite_event_by_spec() {
    let store = SimEventStore::no_faults(40);
    let state = composite_test_state_with_store(store.clone());
    let tenant = TenantId::default();
    let agent = AgentContext::for_service("composite-test");
    let callback_params = json!({
        "sub_writes": [{
            "entity_type": "Child",
            "entity_id": "child-no-parent-event",
            "action": "Create",
            "params": { "Name": "recorded only on child" }
        }]
    });

    state
        .apply_composite_integration_result(
            &tenant,
            "Parent",
            "parent-no-composite-event",
            "CreateChildWithoutParentEvent",
            &callback_params,
            &agent,
        )
        .await
        .expect("composite result should apply without parent event");

    assert!(
        store
            .dump_journal("default:Parent:parent-no-composite-event")
            .is_empty(),
        "record_parent_event=false should leave the parent journal untouched"
    );
    assert_eq!(
        store
            .dump_journal("default:Child:child-no-parent-event")
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "Create"]
    );
}
