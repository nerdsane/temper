//! Production-adapter contract tests for ARN-190 registry restore.

#[path = "registry_restore_adapters/atomicity.rs"]
mod atomicity;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::registry_bootstrap::{
    restore_registry_from_postgres, restore_registry_from_turso, retry_registry_tenant,
};
use temper_store_postgres::{
    PostgresEventStore, PostgresRegistryQuarantineResolution, PostgresRegistryQuarantineUpsert,
    PostgresRegistrySourceSnapshot, migration::run_migrations,
};
use temper_store_turso::{
    TursoEventStore, TursoRegistryQuarantineResolution, TursoRegistryQuarantineUpsert,
    TursoRegistrySourceSnapshot,
};

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");
const ORDER_CSDL: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");

fn turso_snapshot(
    tenant: &str,
    specs: &[(&str, i64)],
    constraint_version: Option<i64>,
) -> TursoRegistrySourceSnapshot {
    TursoRegistrySourceSnapshot {
        spec_versions: specs
            .iter()
            .map(|(entity_type, version)| {
                ((tenant.to_string(), (*entity_type).to_string()), *version)
            })
            .collect(),
        constraint_versions: BTreeMap::from([(tenant.to_string(), constraint_version)]),
    }
}

fn postgres_snapshot(
    tenant: &str,
    specs: &[(&str, i64)],
    constraint_version: Option<i64>,
) -> PostgresRegistrySourceSnapshot {
    PostgresRegistrySourceSnapshot {
        spec_versions: specs
            .iter()
            .map(|(entity_type, version)| {
                ((tenant.to_string(), (*entity_type).to_string()), *version)
            })
            .collect(),
        constraint_versions: BTreeMap::from([(tenant.to_string(), constraint_version)]),
    }
}

#[tokio::test]
async fn turso_adapter_persists_acknowledges_and_repairs_quarantine() {
    let directory = tempfile::tempdir().expect("temporary Turso directory");
    let url = format!("file:{}", directory.path().join("registry.db").display());
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("create real Turso adapter");

    store
        .upsert_spec("healthy", "Order", ORDER_IOA, ORDER_CSDL, "healthy-v1")
        .await
        .expect("seed healthy spec");
    store
        .commit_specs("healthy")
        .await
        .expect("commit healthy spec");
    store
        .upsert_spec("corrupt", "Order", ORDER_IOA, "<a><b", "corrupt-v1")
        .await
        .expect("seed corrupt spec");
    store
        .commit_specs("corrupt")
        .await
        .expect("commit corrupt spec");
    store
        .upsert_spec(
            "interrupted",
            "Order",
            ORDER_IOA,
            "<uncommitted>",
            "partial-v1",
        )
        .await
        .expect("seed uncommitted spec");

    let mut registry = SpecRegistry::new();
    let restored = restore_registry_from_turso(&mut registry, &store)
        .await
        .expect("fault-isolated restore");
    assert_eq!(restored, 1);
    assert!(
        registry
            .get_table(&TenantId::new("healthy"), "Order")
            .is_some()
    );
    assert!(
        registry
            .get_table(&TenantId::new("corrupt"), "Order")
            .is_none()
    );
    assert!(registry.restore_health().is_quarantined("corrupt", "Order"));
    assert!(
        store
            .load_specs()
            .await
            .expect("load committed specs")
            .iter()
            .all(|row| row.tenant != "interrupted"),
        "uncommitted source must never activate and must be garbage-collected"
    );

    let active = store
        .load_registry_restore_quarantines()
        .await
        .expect("load durable quarantine");
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].tenant, "corrupt");
    assert_eq!(active[0].spec_version, 1);
    assert_eq!(active[0].source_kind, "csdl");
    assert!(active[0].acknowledged_at.is_none());

    assert_eq!(
        store
            .acknowledge_registry_restore_quarantine("corrupt", "Order", 1, None)
            .await
            .expect("acknowledge quarantine"),
        Some((1, None))
    );
    let mut second_registry = SpecRegistry::new();
    restore_registry_from_turso(&mut second_registry, &store)
        .await
        .expect("repeat restore");
    assert!(
        store
            .load_registry_restore_quarantines()
            .await
            .expect("load acknowledged quarantine after restart")[0]
            .acknowledged_at
            .is_some(),
        "durable acknowledgment must survive restart"
    );

    store
        .upsert_spec("corrupt", "Order", ORDER_IOA, ORDER_CSDL, "repaired-v2")
        .await
        .expect("repair persisted source");
    store
        .commit_specs("corrupt")
        .await
        .expect("commit repaired source");
    let first_live = Arc::new(RwLock::new(registry));
    let second_live = Arc::new(RwLock::new(second_registry));
    let retry = retry_registry_tenant(&first_live, &store, "corrupt", "Order")
        .await
        .expect("first replica retries repaired tenant");
    assert!(retry.is_healthy());
    assert!(
        first_live
            .read()
            .expect("registry lock")
            .get_table(&TenantId::new("corrupt"), "Order")
            .is_some()
    );
    assert!(
        store
            .load_registry_restore_quarantines()
            .await
            .expect("load resolved quarantine")
            .is_empty(),
        "successful repair must resolve active durable quarantine"
    );
    assert!(
        second_live
            .read()
            .expect("second registry lock")
            .restore_health()
            .is_quarantined("corrupt", "Order"),
        "the second replica starts with process-local degraded state"
    );
    let second_retry = retry_registry_tenant(&second_live, &store, "corrupt", "Order")
        .await
        .expect("stale replica accepts the exact already-resolved durable identity");
    assert!(second_retry.is_healthy());
    assert!(
        second_live
            .read()
            .expect("second registry lock")
            .get_table(&TenantId::new("corrupt"), "Order")
            .is_some(),
        "the stale replica must converge without restart"
    );
}

/// Real Postgres contract proof. This runs when DATABASE_URL is available in
/// local/CI environments and otherwise returns without inventing a mock backend.
#[tokio::test]
async fn postgres_adapter_fault_isolates_and_persists_quarantine_when_available() {
    let Ok(database_url) = std::env::var("DATABASE_URL") else {
        return;
    };
    let pool = sqlx::PgPool::connect(&database_url)
        .await
        .expect("connect Postgres contract database");
    run_migrations(&pool).await.expect("migrate Postgres");
    let store = PostgresEventStore::new(pool.clone());
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let healthy = format!("arn190-healthy-{suffix}");
    let corrupt = format!("arn190-corrupt-{suffix}");
    let interrupted = format!("arn190-interrupted-{suffix}");

    store
        .upsert_spec(&healthy, "Order", ORDER_IOA, ORDER_CSDL, "healthy")
        .await
        .expect("seed healthy Postgres spec");
    store
        .commit_specs(&healthy)
        .await
        .expect("commit healthy Postgres spec");
    store
        .upsert_spec(&corrupt, "Order", ORDER_IOA, "<a><b", "corrupt")
        .await
        .expect("seed corrupt Postgres spec");
    store
        .commit_specs(&corrupt)
        .await
        .expect("commit corrupt Postgres spec");
    store
        .upsert_spec(&interrupted, "Order", ORDER_IOA, "<uncommitted>", "partial")
        .await
        .expect("seed interrupted Postgres spec");

    let mut registry = SpecRegistry::new();
    restore_registry_from_postgres(&mut registry, &store)
        .await
        .expect("restore through real Postgres adapter");
    assert!(
        registry
            .get_table(&TenantId::new(&healthy), "Order")
            .is_some()
    );
    assert!(
        registry
            .get_table(&TenantId::new(&corrupt), "Order")
            .is_none()
    );
    assert!(registry.restore_health().is_quarantined(&corrupt, "Order"));
    assert!(
        registry
            .get_table(&TenantId::new(&interrupted), "Order")
            .is_none(),
        "uncommitted Postgres source must never activate"
    );
    let interrupted_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM specs WHERE tenant = $1")
        .bind(&interrupted)
        .fetch_one(&pool)
        .await
        .expect("count interrupted Postgres source");
    assert_eq!(
        interrupted_rows, 0,
        "startup recovery must garbage-collect uncommitted Postgres source"
    );
    assert!(
        store
            .load_registry_restore_quarantines()
            .await
            .expect("load Postgres quarantine")
            .iter()
            .any(|row| row.tenant == corrupt && row.entity_type == "Order")
    );

    sqlx::query("DELETE FROM registry_restore_quarantines WHERE tenant = ANY($1)")
        .bind(vec![healthy.clone(), corrupt.clone(), interrupted.clone()])
        .execute(&pool)
        .await
        .expect("clean quarantine fixture");
    sqlx::query("DELETE FROM specs WHERE tenant = ANY($1)")
        .bind(vec![healthy, corrupt, interrupted])
        .execute(&pool)
        .await
        .expect("clean spec fixture");
}
