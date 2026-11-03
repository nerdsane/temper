//! Interleave an off-page write between the index probe and native count.
//! Use the production planner and SQL store, with deterministic scheduling at
//! the storage boundary; no separate model of filtering/counting is involved.
use super::*;
use crate::storage::{
    EntityCatalogRow, QueryFieldIndexOrder, QueryFieldIndexPage, QueryPlaneStore,
    QueryProjectionFieldsRow,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use temper_runtime::persistence::PersistenceError;
use temper_runtime::scheduler::install_deterministic_context;

struct WriteBeforeCount {
    store: Arc<dyn QueryPlaneStore>,
    write_pending: AtomicBool,
    replacement: String,
}

#[async_trait::async_trait]
impl QueryPlaneStore for WriteBeforeCount {
    async fn upsert_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        QueryPlaneStore::upsert_projection(
            self.store.as_ref(),
            tenant,
            entity_type,
            entity_id,
            status,
            fields,
            state,
            sequence_nr,
        )
        .await
    }

    async fn refresh_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        QueryPlaneStore::refresh_projection(
            self.store.as_ref(),
            tenant,
            entity_type,
            entity_id,
            status,
            fields,
            state,
            sequence_nr,
        )
        .await
    }

    async fn remove_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        QueryPlaneStore::remove_projection(self.store.as_ref(), tenant, entity_type, entity_id)
            .await
    }

    async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        QueryPlaneStore::query_field_index(
            self.store.as_ref(),
            tenant,
            entity_type,
            where_clause,
            params,
        )
        .await
    }

    async fn query_field_index_page(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
        order_by: &[QueryFieldIndexOrder],
        skip: usize,
        top: usize,
        include_count: bool,
    ) -> Result<Option<QueryFieldIndexPage>, PersistenceError> {
        if include_count && self.write_pending.swap(false, Ordering::SeqCst) {
            // All earlier completeness probes saw an indexed EUR. The write
            // removes that index entry without making the row match USD.
            write_projection(self.store.as_ref(), tenant, "ord-99", &self.replacement, 2).await;
        }
        QueryPlaneStore::query_field_index_page(
            self.store.as_ref(),
            tenant,
            entity_type,
            where_clause,
            params,
            order_by,
            skip,
            top,
            include_count,
        )
        .await
    }

    async fn load_projection_fields_many(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        QueryPlaneStore::load_projection_fields_many(
            self.store.as_ref(),
            tenant,
            entity_type,
            entity_ids,
            field_names,
        )
        .await
    }

    async fn load_entity_catalog_rows(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
    ) -> Result<Option<Vec<EntityCatalogRow>>, PersistenceError> {
        QueryPlaneStore::load_entity_catalog_rows(
            self.store.as_ref(),
            tenant,
            entity_type,
            entity_ids,
        )
        .await
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        QueryPlaneStore::projected_entity_counts_by_tenant(self.store.as_ref()).await
    }
}

async fn write_projection(
    store: &dyn QueryPlaneStore,
    tenant: &str,
    id: &str,
    currency: &str,
    sequence: u64,
) {
    let fields = serde_json::json!({"Id": id, "Currency": currency});
    let state = serde_json::json!({
        "entity_type": "Order", "entity_id": id, "status": "Created",
        "fields": fields, "sequence_nr": sequence, "events": [],
    });
    store
        .upsert_projection(tenant, "Order", id, "Created", &fields, &state, sequence)
        .await
        .unwrap();
}

#[tokio::test]
async fn dst_off_page_index_gap_cannot_inflate_declared_default_count() {
    for seed in 0..16 {
        let _context = install_deterministic_context(seed);
        let dir = tempfile::tempdir().unwrap();
        let store = TursoEventStore::new(
            &format!("file:{}", dir.path().join("count-race.db").display()),
            None,
        )
        .await
        .unwrap();
        assert_count_during_write(StorageStack::from_turso(store), seed).await;
    }
}

#[tokio::test]
async fn postgres_off_page_index_gap_cannot_inflate_declared_default_count() {
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use temper_store_postgres::{PostgresEventStore, PostgresSchema, migration};

    let Some(url) = temper_test_resource::env_var("DATABASE_URL") else {
        return;
    };
    // No catalog relation is visible through search_path: both the page and
    // its completeness proof must resolve the explicitly selected schema.
    let options: PgConnectOptions = url.parse().unwrap();
    let pool = PgPoolOptions::new()
        .connect_with(options.options([("search_path", "pg_catalog")]))
        .await
        .unwrap();
    let schema_name = format!("count_guard_{}", sim_uuid().simple());
    let schema = PostgresSchema::new(&schema_name).unwrap();
    migration::run_migrations_in_schema(&pool, &schema)
        .await
        .unwrap();
    let store = PostgresEventStore::with_schema(pool.clone(), schema);
    for seed in 0..16 {
        let _context = install_deterministic_context(seed);
        assert_count_during_write(StorageStack::from_postgres(store.clone()), seed).await;
    }
    sqlx::query(&format!("DROP SCHEMA \"{schema_name}\" CASCADE"))
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

async fn assert_count_during_write(stack: StorageStack, seed: u64) {
    let tenant = TenantId::from(format!("count-race-{seed}"));
    let store = stack.query_plane().cloned().unwrap();
    for (id, currency) in [("ord-00", "USD"), ("ord-99", "EUR")] {
        write_projection(store.as_ref(), tenant.as_str(), id, currency, 1).await;
    }
    let qp = Arc::new(WriteBeforeCount {
        store,
        write_pending: AtomicBool::new(true),
        replacement: format!("{seed:02001}"),
    });
    let stack = stack.with_read_probe(qp.clone());
    let mut state = build_order_state("declared-default-count-race");
    state.set_storage_stack(stack);
    let spec = format!(
        "{ORDER_IOA}\n[[state]]\nname = \"Currency\"\ntype = \"string\"\ninitial = \"USD\"\n"
    );
    state.registry.write().unwrap().register_tenant(
        tenant.as_str(),
        parse_csdl(CSDL_XML).unwrap(),
        CSDL_XML.to_string(),
        &[("Order", &spec)],
    );
    state
        .authz
        .reload_tenant_policies(tenant.as_str(), "permit(principal, action, resource);")
        .unwrap();
    let security = SecurityContext::system();
    for skip in [0, 1] {
        let options = QueryOptions {
            filter: Some(temper_odata::query::parse_filter("Currency eq 'USD'").unwrap()),
            top: Some(1),
            skip: Some(skip),
            count: Some(true),
            ..QueryOptions::default()
        };
        let request = QueryPlaneReadRequest {
            state: &state,
            tenant: &tenant,
            security_ctx: &security,
            entity_type: "Order",
            entity_set_name: "Orders",
            query_options: &options,
            budget: QueryPlaneReadBudget {
                default_page_size: 1,
                max_entities: 10,
            },
        };
        let plan = native_candidate_page_plan(&request).unwrap();
        let result = try_native_page_read(&request, plan, QueryPlaneCoverageReport::default())
            .await
            .expect("native candidate read")
            .unwrap_or_else(|_| panic!("seed {seed}: count must fit budget"));
        assert!(!qp.write_pending.load(Ordering::SeqCst));
        assert_eq!(result.count, Some(1), "seed {seed}, skip {skip}");
        assert_eq!(result.entities.len(), 1 - skip, "seed {seed}");
        if skip == 0 {
            assert_eq!(result.entities[0]["entity_id"], "ord-00", "seed {seed}");
        }
    }
}
