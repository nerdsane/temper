//! Scoped schema-pin journal index coverage.

use super::*;

#[tokio::test]
async fn scoped_entity_bundle_digest_lookup_is_exact_and_bounded() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let suffix = uuid::Uuid::new_v4();
    let tenant = format!("schema-pin-{suffix}");
    let entity_id = format!("order-{suffix}");
    let scope = SchemaScope {
        kind: temper_runtime::persistence::schema_deployment::SchemaScopeKind::Task,
        id: "task-redis".into(),
    };
    let first = format!("sha256:{}", "a".repeat(64));
    let second = format!("sha256:{}", "b".repeat(64));
    for digest in [&first, &second] {
        let persistence_id = format!(
            "{tenant}:Order:{}",
            temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                &entity_id,
                &temper_runtime::persistence::schema_deployment::SchemaExecutionPin {
                    scope: scope.clone(),
                    bundle_digest: digest.clone(),
                },
            )
        );
        store
            .append(
                &persistence_id,
                0,
                &[test_envelope("OrderCreated", serde_json::json!({}))],
            )
            .await
            .expect("append scoped event");
    }
    let collision_base = format!("collision-{suffix}");
    let first_pin = temper_runtime::persistence::schema_deployment::SchemaExecutionPin {
        scope: scope.clone(),
        bundle_digest: first.clone(),
    };
    let collision_entity = temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
        &collision_base,
        &first_pin,
    );
    store
        .append(
            &format!(
                "{tenant}:Order:{}",
                temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                    &collision_entity,
                    &first_pin,
                )
            ),
            0,
            &[test_envelope("OrderCreated", serde_json::json!({}))],
        )
        .await
        .expect("append colon-bearing scoped entity");
    assert!(
        store
            .scoped_entity_bundle_digests(&tenant, "Order", &collision_base, &scope, 2)
            .await
            .expect("reject colliding entity prefix")
            .is_empty()
    );
    assert_eq!(
        store
            .scoped_entity_bundle_digests(&tenant, "Order", &collision_entity, &scope, 2)
            .await
            .expect("load colon-bearing entity pin"),
        vec![first.clone()]
    );
    assert_eq!(
        store
            .scoped_entity_bundle_digests(&tenant, "Order", &entity_id, &scope, 1)
            .await
            .expect("lookup scoped pin"),
        vec![first.clone()]
    );
    assert_eq!(
        store
            .scoped_entity_bundle_digests(&tenant, "Order", &entity_id, &scope, 2)
            .await
            .expect("lookup scoped pins"),
        vec![first, second]
    );
}

#[tokio::test]
async fn scoped_entity_bundle_digest_lookup_fails_closed_at_scan_budget() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let suffix = uuid::Uuid::new_v4();
    let tenant = format!("schema-pin-budget-{suffix}");
    let entity_id = format!("order-{suffix}");
    let scope = SchemaScope {
        kind: temper_runtime::persistence::schema_deployment::SchemaScopeKind::Task,
        id: "task-redis-budget".into(),
    };
    let target_digest = format!("sha256:{}", "f".repeat(64));
    let collision_digest = format!("sha256:{}", "a".repeat(64));
    for index in 0_u16..256 {
        let embedded_digest = format!("sha256:{index:064x}");
        let collision_entity = format!(
            "{}{}",
            temper_runtime::persistence::schema_deployment::scoped_journal_pin_prefix(
                &entity_id, &scope,
            ),
            embedded_digest,
        );
        let collision_pin = temper_runtime::persistence::schema_deployment::SchemaExecutionPin {
            scope: scope.clone(),
            bundle_digest: collision_digest.clone(),
        };
        store
            .append(
                &format!(
                    "{tenant}:Order:{}",
                    temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                        &collision_entity,
                        &collision_pin,
                    )
                ),
                0,
                &[test_envelope("OrderCreated", serde_json::json!({}))],
            )
            .await
            .expect("append scan-budget collision");
    }
    store
        .append(
            &format!(
                "{tenant}:Order:{}",
                temper_runtime::persistence::schema_deployment::scoped_journal_entity_id(
                    &entity_id,
                    &temper_runtime::persistence::schema_deployment::SchemaExecutionPin {
                        scope: scope.clone(),
                        bundle_digest: target_digest,
                    },
                )
            ),
            0,
            &[test_envelope("OrderCreated", serde_json::json!({}))],
        )
        .await
        .expect("append target pin");

    let error = store
        .scoped_entity_bundle_digests(&tenant, "Order", &entity_id, &scope, 1)
        .await
        .expect_err("scan-budget exhaustion must fail closed");
    assert!(error.to_string().contains("scan budget exhausted"));
}
