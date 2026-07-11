use super::*;

#[tokio::test]
async fn turso_quarantine_resolution_rolls_back_the_entire_batch_on_version_drift() {
    let directory = tempfile::tempdir().expect("temporary atomicity directory");
    let url = format!("file:{}", directory.path().join("atomic.db").display());
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("create atomicity store");
    for entity_type in ["Order", "Task"] {
        store
            .upsert_spec(
                "atomic",
                entity_type,
                ORDER_IOA,
                ORDER_CSDL,
                &format!("{entity_type}-v1"),
            )
            .await
            .expect("seed atomicity source");
    }
    store
        .commit_specs("atomic")
        .await
        .expect("commit atomicity sources");
    store
        .replace_registry_restore_quarantines(
            &turso_snapshot("atomic", &[("Order", 1), ("Task", 1)], None),
            &[
                TursoRegistryQuarantineUpsert {
                    tenant: "atomic",
                    entity_type: "Order",
                    spec_version: 1,
                    constraint_version: None,
                    reason: "invalid_csdl",
                    source_kind: "csdl",
                    source_line: None,
                    source_column: None,
                    detail: "order failed",
                },
                TursoRegistryQuarantineUpsert {
                    tenant: "atomic",
                    entity_type: "Task",
                    spec_version: 1,
                    constraint_version: None,
                    reason: "invalid_csdl",
                    source_kind: "csdl",
                    source_line: None,
                    source_column: None,
                    detail: "task failed",
                },
            ],
        )
        .await
        .expect("seed atomic quarantines");
    store
        .upsert_spec("atomic", "Order", ORDER_IOA, ORDER_CSDL, "Order-v2")
        .await
        .expect("write repaired Order");
    store
        .commit_specs("atomic")
        .await
        .expect("commit repaired Order");
    let repaired_source = turso_snapshot("atomic", &[("Order", 2), ("Task", 1)], None);
    let exact_resolutions = [
        TursoRegistryQuarantineResolution {
            tenant: "atomic",
            entity_type: "Order",
            quarantined_version: 1,
            quarantined_constraint_version: None,
        },
        TursoRegistryQuarantineResolution {
            tenant: "atomic",
            entity_type: "Task",
            quarantined_version: 1,
            quarantined_constraint_version: None,
        },
    ];

    store
        .upsert_spec("atomic", "Invoice", ORDER_IOA, ORDER_CSDL, "Invoice-v1")
        .await
        .expect("insert Turso sibling after validation");
    store
        .commit_specs("atomic")
        .await
        .expect("commit Turso sibling insertion");
    assert!(
        !store
            .resolve_registry_restore_quarantines(&repaired_source, &exact_resolutions)
            .await
            .expect("Turso sibling insertion is a typed conflict")
    );
    store
        .delete_spec("atomic", "Invoice")
        .await
        .expect("remove inserted Turso sibling");

    store
        .delete_spec("atomic", "Task")
        .await
        .expect("remove Turso sibling after validation");
    assert!(
        !store
            .resolve_registry_restore_quarantines(&repaired_source, &exact_resolutions)
            .await
            .expect("Turso sibling removal is a typed conflict")
    );
    store
        .upsert_spec("atomic", "Task", ORDER_IOA, ORDER_CSDL, "Task-v1")
        .await
        .expect("restore Turso Task fixture");
    store
        .commit_specs("atomic")
        .await
        .expect("commit restored Turso Task fixture");

    let resolution = store
        .resolve_registry_restore_quarantines(
            &repaired_source,
            &[
                TursoRegistryQuarantineResolution {
                    tenant: "atomic",
                    entity_type: "Order",
                    quarantined_version: 1,
                    quarantined_constraint_version: None,
                },
                TursoRegistryQuarantineResolution {
                    tenant: "atomic",
                    entity_type: "Task",
                    quarantined_version: 999,
                    quarantined_constraint_version: None,
                },
            ],
        )
        .await
        .expect("stale batch must remain a typed conflict");
    assert!(!resolution, "one stale version must reject the batch");
    assert_eq!(
        store
            .load_registry_restore_quarantines()
            .await
            .expect("load rolled-back quarantines")
            .len(),
        2,
        "no earlier record may resolve when a later CAS fails"
    );

    store
        .upsert_tenant_constraints("atomic", "version = 1")
        .await
        .expect("seed Turso constraint version");
    store
        .upsert_tenant_constraints("atomic", "version = 2")
        .await
        .expect("advance Turso constraint version");
    assert!(
        !store
            .resolve_registry_restore_quarantines(
                &turso_snapshot("atomic", &[("Order", 2), ("Task", 1)], Some(1)),
                &[TursoRegistryQuarantineResolution {
                    tenant: "atomic",
                    entity_type: "Order",
                    quarantined_version: 1,
                    quarantined_constraint_version: None,
                }],
            )
            .await
            .expect("constraint drift must remain a typed conflict"),
        "stale constraints must prevent durable quarantine resolution"
    );
}

#[tokio::test]
async fn postgres_quarantine_resolution_is_atomic_when_available() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = sqlx::PgPool::connect(&database_url)
        .await
        .expect("connect Postgres atomicity database");
    run_migrations(&pool).await.expect("migrate Postgres");
    let store = PostgresEventStore::new(pool.clone());
    let tenant = format!("arn190-atomic-{}", uuid::Uuid::new_v4().simple());
    for entity_type in ["Order", "Task"] {
        store
            .upsert_spec(
                &tenant,
                entity_type,
                ORDER_IOA,
                ORDER_CSDL,
                &format!("{entity_type}-v1"),
            )
            .await
            .expect("seed Postgres atomic source");
    }
    store
        .commit_specs(&tenant)
        .await
        .expect("commit Postgres atomic sources");
    store
        .replace_registry_restore_quarantines_for_tenant(
            &tenant,
            &postgres_snapshot(&tenant, &[("Order", 1), ("Task", 1)], None),
            &[
                PostgresRegistryQuarantineUpsert {
                    tenant: &tenant,
                    entity_type: "Order",
                    spec_version: 1,
                    constraint_version: None,
                    reason: "invalid_csdl",
                    source_kind: "csdl",
                    source_line: None,
                    source_column: None,
                    detail: "atomic contract",
                },
                PostgresRegistryQuarantineUpsert {
                    tenant: &tenant,
                    entity_type: "Task",
                    spec_version: 1,
                    constraint_version: None,
                    reason: "invalid_csdl",
                    source_kind: "csdl",
                    source_line: None,
                    source_column: None,
                    detail: "atomic contract",
                },
            ],
        )
        .await
        .expect("seed Postgres atomic quarantines");
    store
        .upsert_spec(&tenant, "Order", ORDER_IOA, ORDER_CSDL, "Order-v2")
        .await
        .expect("repair Postgres Order");
    store
        .commit_specs(&tenant)
        .await
        .expect("commit Postgres Order repair");
    let repaired_source = postgres_snapshot(&tenant, &[("Order", 2), ("Task", 1)], None);
    let exact_resolutions = [
        PostgresRegistryQuarantineResolution {
            tenant: &tenant,
            entity_type: "Order",
            quarantined_version: 1,
            quarantined_constraint_version: None,
        },
        PostgresRegistryQuarantineResolution {
            tenant: &tenant,
            entity_type: "Task",
            quarantined_version: 1,
            quarantined_constraint_version: None,
        },
    ];

    store
        .upsert_spec(&tenant, "Invoice", ORDER_IOA, ORDER_CSDL, "Invoice-v1")
        .await
        .expect("insert Postgres sibling after validation");
    store
        .commit_specs(&tenant)
        .await
        .expect("commit Postgres sibling insertion");
    assert!(
        !store
            .resolve_registry_restore_quarantines(&repaired_source, &exact_resolutions)
            .await
            .expect("Postgres sibling insertion is a typed conflict")
    );
    store
        .delete_spec(&tenant, "Invoice")
        .await
        .expect("remove inserted Postgres sibling");

    store
        .delete_spec(&tenant, "Task")
        .await
        .expect("remove Postgres sibling after validation");
    assert!(
        !store
            .resolve_registry_restore_quarantines(&repaired_source, &exact_resolutions)
            .await
            .expect("Postgres sibling removal is a typed conflict")
    );
    store
        .upsert_spec(&tenant, "Task", ORDER_IOA, ORDER_CSDL, "Task-v1")
        .await
        .expect("restore Postgres Task fixture");
    store
        .commit_specs(&tenant)
        .await
        .expect("commit restored Postgres Task fixture");

    assert!(
        !store
            .resolve_registry_restore_quarantines(
                &repaired_source,
                &[
                    PostgresRegistryQuarantineResolution {
                        tenant: &tenant,
                        entity_type: "Order",
                        quarantined_version: 1,
                        quarantined_constraint_version: None,
                    },
                    PostgresRegistryQuarantineResolution {
                        tenant: &tenant,
                        entity_type: "Task",
                        quarantined_version: 999,
                        quarantined_constraint_version: None,
                    },
                ],
            )
            .await
            .expect("stale Postgres batch must remain a typed conflict")
    );
    assert_eq!(
        store
            .load_registry_restore_quarantines()
            .await
            .expect("load Postgres atomic quarantines")
            .into_iter()
            .filter(|row| row.tenant == tenant)
            .count(),
        2,
        "Postgres must roll back earlier resolutions when a later CAS fails"
    );

    store
        .upsert_tenant_constraints(&tenant, "version = 1")
        .await
        .expect("seed Postgres constraint version");
    store
        .upsert_tenant_constraints(&tenant, "version = 2")
        .await
        .expect("advance Postgres constraint version");
    assert!(
        !store
            .resolve_registry_restore_quarantines(
                &postgres_snapshot(&tenant, &[("Order", 2), ("Task", 1)], Some(1)),
                &[PostgresRegistryQuarantineResolution {
                    tenant: &tenant,
                    entity_type: "Order",
                    quarantined_version: 1,
                    quarantined_constraint_version: None,
                }],
            )
            .await
            .expect("Postgres constraint drift must remain a typed conflict"),
        "stale Postgres constraints must prevent durable quarantine resolution"
    );

    sqlx::query("DELETE FROM registry_restore_quarantines WHERE tenant = $1")
        .bind(&tenant)
        .execute(&pool)
        .await
        .expect("clean Postgres atomic quarantines");
    sqlx::query("DELETE FROM specs WHERE tenant = $1")
        .bind(&tenant)
        .execute(&pool)
        .await
        .expect("clean Postgres atomic specs");
    sqlx::query("DELETE FROM tenant_constraints WHERE tenant = $1")
        .bind(&tenant)
        .execute(&pool)
        .await
        .expect("clean Postgres atomic constraints");
}
