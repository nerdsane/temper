//! Query plane storage routed to each tenant's own store.

use temper_runtime::persistence::PersistenceError;
use temper_store_turso::{TenantStoreRouter, TursoEventStore};

use super::query_plane_impls::{catalog_freshness, storage_order_by};
use super::{
    CatalogGroup, CatalogRowFreshness, EntityCatalogRow, QueryFieldIndexOrder, QueryFieldIndexPage,
    QueryPlaneStore, QueryProjectionFieldsRow,
};

#[async_trait::async_trait]
impl QueryPlaneStore for TenantStoreRouter {
    async fn lagging_projections(
        &self,
        tenant: &str,
    ) -> Result<Option<Vec<temper_runtime::persistence::ProjectionRepairCandidate>>, PersistenceError>
    {
        self.store_for_tenant(tenant)
            .await?
            .lagging_query_projections(tenant)
            .await
            .map(Some)
    }
    async fn projection_checkpoints(
        &self,
        tenant: &str,
    ) -> Result<Vec<temper_runtime::persistence::ProjectionCheckpoint>, PersistenceError> {
        self.store_for_tenant(tenant)
            .await?
            .query_projection_checkpoints(tenant)
            .await
    }
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
        let store = self.store_for_tenant(tenant).await?;
        store
            .upsert_query_projection_with_state(
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
        let store = self.store_for_tenant(tenant).await?;
        store
            .refresh_query_projection_with_state(
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
        let store = self.store_for_tenant(tenant).await?;
        store
            .remove_query_projection(tenant, entity_type, entity_id)
            .await
    }

    async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        TursoEventStore::query_field_index(&store, tenant, entity_type, where_clause, params)
            .await
            .map(Some)
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
        let store = self.store_for_tenant(tenant).await?;
        let order_by = storage_order_by(order_by);
        let (entity_ids, total_count) = store
            .query_field_index_page(
                tenant,
                entity_type,
                where_clause,
                params,
                &order_by,
                skip,
                top,
                include_count,
            )
            .await?;
        Ok(Some(QueryFieldIndexPage {
            entity_ids,
            total_count,
        }))
    }

    async fn load_projection_fields_many(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .load_query_projection_fields_many(tenant, entity_type, entity_ids, field_names)
            .await
            .map(|rows| {
                Some(
                    rows.into_iter()
                        .map(|row| QueryProjectionFieldsRow {
                            entity_id: row.entity_id,
                            status: row.status,
                            fields: row.fields,
                        })
                        .collect(),
                )
            })
    }

    async fn load_entity_catalog_rows(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
    ) -> Result<Option<Vec<EntityCatalogRow>>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        TursoEventStore::load_entity_catalog_rows(&store, tenant, entity_type, entity_ids)
            .await
            .map(|rows| {
                Some(
                    rows.into_iter()
                        .map(|row| EntityCatalogRow {
                            entity_id: row.entity_id,
                            status: row.status,
                            fields: row.fields,
                            state: row.state,
                            sequence_nr: row.sequence_nr,
                        })
                        .collect(),
                )
            })
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        let mut counts = Vec::new();
        for tenant_id in self.connected_tenants().await {
            let store = self.store_for_tenant(&tenant_id).await?;
            if let Some((_, count)) = TursoEventStore::projected_entity_counts_by_tenant(&store)
                .await?
                .into_iter()
                .find(|(tenant, _)| tenant == &tenant_id)
            {
                counts.push((tenant_id, count));
            }
        }
        Ok(Some(counts))
    }

    async fn reconciled_equality_candidates(
        &self,
        journal: &super::BoxedEventStore,
        tenant: &str,
        entity_type: &str,
        predicates: &[(String, String)],
        probe_budget: Option<usize>,
    ) -> Result<Option<(Vec<String>, Vec<String>)>, PersistenceError> {
        // A separately supplied journal can have changes absent from this
        // store's delta. Its absence is never evidence about that journal.
        if !std::ptr::addr_eq(self as *const Self, std::sync::Arc::as_ptr(&journal.store)) {
            return Ok(None);
        }
        self.store_for_tenant(tenant)
            .await?
            .reconciled_equality_candidates(tenant, entity_type, predicates, probe_budget)
            .await
            .map(Some)
    }

    async fn projection_catalog_freshness(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Option<Vec<CatalogRowFreshness>>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .projection_catalog_freshness(tenant, entity_type)
            .await
            .map(|rows| Some(catalog_freshness(rows)))
    }

    async fn catalog_groups(
        &self,
        tenant: &str,
        entity_type: &str,
        fields: &[String],
        statuses: &[String],
    ) -> Result<Option<Vec<CatalogGroup>>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        let rows = store
            .catalog_groups(tenant, entity_type, fields, statuses)
            .await?;
        Ok(Some(CatalogGroup::from_rows(rows, fields)))
    }
}
