//! Real PostgreSQL: retention is independent of tenants' global id parity.

use sqlx::postgres::PgPoolOptions;
use temper_runtime::persistence::WASM_INVOCATIONS_KEPT_PER_TENANT;
use temper_store_postgres::{
    PostgresEventStore, PostgresSchema, PostgresWasmInvocationInsert, migration,
};

#[tokio::test]
#[ignore = "requires TEMPER_TEST_POSTGRES_URL pointing to disposable PostgreSQL"]
async fn interleaved_tenants_each_keep_their_newest_invocations() {
    let url = std::env::var("TEMPER_TEST_POSTGRES_URL").expect("disposable PostgreSQL URL");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    let name = format!("wasm_bound_{}", uuid::Uuid::new_v4().simple());
    let schema = PostgresSchema::new(&name).unwrap();
    migration::run_migrations_in_schema(&pool, &schema)
        .await
        .unwrap();
    let store = PostgresEventStore::with_schema(pool.clone(), schema);
    let inserted = WASM_INVOCATIONS_KEPT_PER_TENANT + 250;
    for n in 0..inserted {
        let entity_id = format!("e-{n}");
        for tenant in ["odd", "even"] {
            store
                .persist_wasm_invocation(&PostgresWasmInvocationInsert {
                    tenant,
                    entity_type: "Job",
                    entity_id: &entity_id,
                    module_name: "poll",
                    trigger_action: "Poll",
                    callback_action: None,
                    success: true,
                    error: None,
                    duration_ms: 1,
                    created_at: "2026-11-02T00:00:00Z",
                })
                .await
                .unwrap();
        }
    }
    let rows = store
        .load_recent_wasm_invocations(2 * inserted)
        .await
        .unwrap();
    // Clean up even when the assertions below catch a regression.
    sqlx::query(&format!("DROP SCHEMA {name} CASCADE"))
        .execute(&pool)
        .await
        .unwrap();
    for tenant in ["odd", "even"] {
        let mut ids: Vec<i64> = rows
            .iter()
            .filter(|row| row.tenant == tenant)
            .map(|row| row.entity_id.strip_prefix("e-").unwrap().parse().unwrap())
            .collect();
        assert_eq!(
            ids.len() as i64,
            WASM_INVOCATIONS_KEPT_PER_TENANT,
            "{tenant} retains a bounded log"
        );
        ids.sort();
        assert_eq!(
            ids,
            (inserted - WASM_INVOCATIONS_KEPT_PER_TENANT..inserted).collect::<Vec<_>>(),
            "every tenant retains exactly its newest rows, regardless of global id parity"
        );
    }
}
