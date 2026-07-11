use temper_server::platform_store::{
    RegistryQuarantineResolution, RegistryQuarantineUpsert, SimPlatformFaultConfig,
};

use super::*;

#[tokio::test]
async fn quarantine_durability_failure_cannot_masquerade_as_safe_degraded_boot() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(91);
    let store = SimPlatformStore::no_faults(91);
    store
        .upsert_spec(
            "corrupt",
            "Order",
            "[automaton]\nname = \"Order\"\n",
            "<a><b",
            "corrupt",
        )
        .await
        .expect("seed corrupt source");
    store
        .commit_specs("corrupt")
        .await
        .expect("commit corrupt source");
    let mut faults = SimPlatformFaultConfig::none();
    faults.quarantine_write_failure_prob = 1.0;
    store.restore_faults(faults);

    let mut registry = SpecRegistry::new();
    let error = restore_registry_from_platform_store(&mut registry, &store)
        .await
        .expect_err("quarantine persistence failure must fail restore");
    assert!(error.contains("persist registry restore quarantine"));
    assert!(
        registry.restore_health().is_healthy(),
        "process health must not claim an unpersisted quarantine"
    );

    store.disable_faults();
    assert!(
        store
            .load_registry_restore_quarantines()
            .await
            .expect("read quarantine state")
            .is_empty()
    );
    assert_eq!(
        store.load_specs().await.expect("retained source").len(),
        1,
        "failed diagnostic persistence must never delete committed source"
    );
}

#[tokio::test]
async fn resolved_sim_quarantine_reopens_same_version_with_history_intact() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(92);
    let store = SimPlatformStore::no_faults(92);
    store
        .upsert_spec("history", "Order", ORDER_IOA, ORDER_CSDL, "history-v1")
        .await
        .expect("seed history source");
    store
        .commit_specs("history")
        .await
        .expect("commit history source");
    let active = [RegistryQuarantineUpsert {
        tenant: "history",
        entity_type: "Order",
        spec_version: 1,
        constraint_version: None,
        reason: "invalid_csdl",
        source_kind: "csdl",
        source_line: Some(2),
        source_column: Some(4),
        detail: "history contract",
    }];
    let source = source_snapshot(&store).await;
    store
        .replace_registry_restore_quarantines(&source, &active)
        .await
        .expect("open history quarantine");
    assert_eq!(
        store
            .acknowledge_registry_restore_quarantine("history", "Order", 1, None)
            .await
            .expect("acknowledge history quarantine"),
        Some((1, None))
    );
    let before = store
        .load_registry_restore_quarantines()
        .await
        .expect("load acknowledged history")
        .pop()
        .expect("active history record");

    assert!(
        store
            .resolve_registry_restore_quarantines(
                &source,
                &[RegistryQuarantineResolution {
                    tenant: "history",
                    entity_type: "Order",
                    quarantined_version: 1,
                    quarantined_constraint_version: None,
                }],
            )
            .await
            .expect("resolve history quarantine")
    );
    assert!(
        store
            .load_registry_restore_quarantines()
            .await
            .expect("load resolved history")
            .is_empty()
    );

    store
        .replace_registry_restore_quarantines(&source, &active)
        .await
        .expect("reopen same history version");
    let reopened = store
        .load_registry_restore_quarantines()
        .await
        .expect("load reopened history")
        .pop()
        .expect("reopened history record");
    assert_eq!(reopened.created_at, before.created_at);
    assert_eq!(reopened.acknowledged_at, before.acknowledged_at);
}

#[tokio::test]
async fn constraint_identity_change_reopens_unacknowledged_and_rejects_stale_ack() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(93);
    let store = SimPlatformStore::no_faults(93);
    store
        .upsert_spec("history", "Order", ORDER_IOA, ORDER_CSDL, "history-v1")
        .await
        .expect("seed constraint history source");
    store
        .commit_specs("history")
        .await
        .expect("commit constraint history source");
    store
        .upsert_tenant_constraints("history", "version = 1")
        .await
        .expect("seed constraint version one");
    let first = [RegistryQuarantineUpsert {
        tenant: "history",
        entity_type: "Order",
        spec_version: 1,
        constraint_version: Some(1),
        reason: "registration_failed",
        source_kind: "cross_invariants",
        source_line: None,
        source_column: None,
        detail: "constraint version one",
    }];
    let first_source = source_snapshot(&store).await;
    store
        .replace_registry_restore_quarantines(&first_source, &first)
        .await
        .expect("open first constraint quarantine");
    assert_eq!(
        store
            .acknowledge_registry_restore_quarantine("history", "Order", 1, Some(1))
            .await
            .expect("acknowledge first constraint identity"),
        Some((1, Some(1)))
    );

    store
        .upsert_tenant_constraints("history", "version = 2")
        .await
        .expect("advance constraint version");
    assert!(
        !store
            .replace_registry_restore_quarantines(&first_source, &first)
            .await
            .expect("stale snapshot remains a typed conflict"),
        "quarantine replacement must compare-and-set the constraint snapshot"
    );
    let retained_first = store
        .load_registry_restore_quarantines()
        .await
        .expect("load retained first identity")
        .pop()
        .expect("first identity remains active");
    assert_eq!(retained_first.constraint_version, Some(1));
    assert!(retained_first.acknowledged_at.is_some());
    let second = [RegistryQuarantineUpsert {
        constraint_version: Some(2),
        detail: "constraint version two",
        ..first[0]
    }];
    let second_source = source_snapshot(&store).await;
    store
        .replace_registry_restore_quarantines(&second_source, &second)
        .await
        .expect("open second constraint quarantine");
    assert_eq!(
        store
            .acknowledge_registry_restore_quarantine("history", "Order", 1, Some(1))
            .await
            .expect("stale acknowledgment remains a typed conflict"),
        Some((1, Some(2)))
    );
    let current = store
        .load_registry_restore_quarantines()
        .await
        .expect("load current constraint quarantine")
        .pop()
        .expect("current constraint quarantine");
    assert_eq!(current.constraint_version, Some(2));
    assert!(
        current.acknowledged_at.is_none(),
        "a new constraint identity must not inherit acknowledgment"
    );
}

#[tokio::test]
async fn complete_source_manifest_rejects_sibling_insertion_and_removal() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(95);
    let store = SimPlatformStore::no_faults(95);
    store
        .upsert_spec("manifest", "Order", ORDER_IOA, ORDER_CSDL, "order-v1")
        .await
        .expect("seed manifest Order");
    store
        .commit_specs("manifest")
        .await
        .expect("commit manifest Order");
    let order_only = source_snapshot(&store).await;
    let active = [RegistryQuarantineUpsert {
        tenant: "manifest",
        entity_type: "Order",
        spec_version: 1,
        constraint_version: None,
        reason: "invalid_csdl",
        source_kind: "csdl",
        source_line: None,
        source_column: None,
        detail: "complete manifest contract",
    }];
    assert!(
        store
            .replace_registry_restore_quarantines(&order_only, &active)
            .await
            .expect("open manifest quarantine")
    );

    store
        .upsert_spec("manifest", "Task", ORDER_IOA, ORDER_CSDL, "task-v1")
        .await
        .expect("insert sibling after validation");
    store
        .commit_specs("manifest")
        .await
        .expect("commit inserted sibling");
    assert!(
        !store
            .resolve_registry_restore_quarantines(
                &order_only,
                &[RegistryQuarantineResolution {
                    tenant: "manifest",
                    entity_type: "Order",
                    quarantined_version: 1,
                    quarantined_constraint_version: None,
                }],
            )
            .await
            .expect("sibling insertion is a typed conflict"),
        "a newly committed sibling must invalidate the complete tenant snapshot"
    );
    assert_eq!(
        store
            .load_registry_restore_quarantines()
            .await
            .expect("load quarantine after insertion race")
            .len(),
        1
    );

    let with_sibling = source_snapshot(&store).await;
    store
        .delete_spec("manifest", "Task")
        .await
        .expect("remove sibling after validation");
    assert!(
        !store
            .replace_registry_restore_quarantines(&with_sibling, &active)
            .await
            .expect("sibling removal is a typed conflict"),
        "removing a committed sibling must invalidate the complete tenant snapshot"
    );
    assert_eq!(
        store
            .load_registry_restore_quarantines()
            .await
            .expect("load quarantine after removal race")
            .len(),
        1,
        "failed complete-set CAS must preserve the prior active quarantine"
    );
}

#[tokio::test]
async fn sim_quarantine_payload_validation_matches_durable_adapters() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(94);
    let store = SimPlatformStore::no_faults(94);
    let valid = RegistryQuarantineUpsert {
        tenant: "validation",
        entity_type: "Order",
        spec_version: 1,
        constraint_version: None,
        reason: "invalid_csdl",
        source_kind: "csdl",
        source_line: None,
        source_column: None,
        detail: "bounded",
    };

    for invalid in [
        RegistryQuarantineUpsert {
            reason: "unknown",
            ..valid
        },
        RegistryQuarantineUpsert {
            source_kind: "unknown",
            ..valid
        },
        RegistryQuarantineUpsert {
            spec_version: 0,
            ..valid
        },
        RegistryQuarantineUpsert {
            constraint_version: Some(0),
            ..valid
        },
    ] {
        assert!(
            store
                .replace_registry_restore_quarantines(
                    &RegistrySourceSnapshot::default(),
                    &[invalid],
                )
                .await
                .is_err(),
            "invalid quarantine payload must be rejected"
        );
    }

    let oversized = "x".repeat(513);
    assert!(
        store
            .replace_registry_restore_quarantines(
                &RegistrySourceSnapshot::default(),
                &[RegistryQuarantineUpsert {
                    detail: &oversized,
                    ..valid
                }],
            )
            .await
            .is_err()
    );
    assert!(
        store
            .replace_registry_restore_quarantines(
                &RegistrySourceSnapshot::default(),
                &[
                    valid,
                    RegistryQuarantineUpsert {
                        spec_version: 2,
                        ..valid
                    },
                ],
            )
            .await
            .is_err(),
        "one snapshot cannot carry multiple active identities for an entity"
    );
}
