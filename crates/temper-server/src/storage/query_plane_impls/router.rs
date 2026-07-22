use temper_store_turso::{TenantStoreRouter, TursoEventStore};

use super::*;

#[async_trait::async_trait]
impl QueryPlaneStore for TenantStoreRouter {
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

    async fn upsert_projection_if_source(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
        source: ProjectionSourceFence<'_>,
    ) -> Result<bool, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .upsert_query_projection_with_state_if_source(
                tenant,
                entity_type,
                entity_id,
                status,
                fields,
                state,
                sequence_nr,
                source,
            )
            .await
    }

    async fn upsert_projections(
        &self,
        tenant: &str,
        projections: &[QueryProjectionUpsert],
    ) -> Result<(), PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        let turso_projections = projections
            .iter()
            .map(to_turso_projection)
            .collect::<Vec<_>>();
        store
            .upsert_query_projections(tenant, &turso_projections)
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

    async fn remove_projection_if_source(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        source: ProjectionSourceFence<'_>,
    ) -> Result<bool, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .remove_query_projection_if_source(tenant, entity_type, entity_id, source)
            .await
    }

    async fn clear_projection_dirty_if_source(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        source: ProjectionSourceFence<'_>,
    ) -> Result<bool, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .clear_query_projection_dirty_if_source(tenant, entity_type, entity_id, source)
            .await
    }

    async fn remove_projection_if_exact(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<bool, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .remove_query_projection_if_exact(
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

    async fn dirty_projection_entity_ids(
        &self,
        tenant: &str,
        entity_type: &str,
        limit: usize,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .dirty_query_projection_entity_ids(tenant, entity_type, limit)
            .await
            .map(Some)
    }
}
