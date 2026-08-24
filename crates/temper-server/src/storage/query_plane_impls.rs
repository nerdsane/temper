use temper_runtime::persistence::{
    PersistenceError, QueryProjectionOrder, QueryProjectionOrderTarget,
};
use temper_store_postgres::PostgresEventStore;
use temper_store_turso::{
    QueryProjectionUpsert as TursoQueryProjectionUpsert, TenantStoreRouter, TursoEventStore,
};

use super::{
    EntityCatalogRow, QueryFieldIndexOrder, QueryFieldIndexOrderDirection,
    QueryFieldIndexOrderTarget, QueryFieldIndexPage, QueryPlaneStore, QueryProjectionFieldsRow,
    QueryProjectionUpsert,
};

fn storage_order_by(order_by: &[QueryFieldIndexOrder]) -> Vec<QueryProjectionOrder> {
    order_by
        .iter()
        .map(|order| QueryProjectionOrder {
            target: match &order.target {
                QueryFieldIndexOrderTarget::Property(field) => {
                    QueryProjectionOrderTarget::Property(field.clone())
                }
                QueryFieldIndexOrderTarget::Status => QueryProjectionOrderTarget::Status,
                QueryFieldIndexOrderTarget::EntityId => QueryProjectionOrderTarget::EntityId,
                QueryFieldIndexOrderTarget::EntityCommitSequence => {
                    QueryProjectionOrderTarget::EntityCommitSequence
                }
            },
            descending: order.direction == QueryFieldIndexOrderDirection::Desc,
        })
        .collect()
}

#[async_trait::async_trait]
impl QueryPlaneStore for PostgresEventStore {
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
        self.upsert_query_projection_with_state(
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
        self.remove_query_projection(tenant, entity_type, entity_id)
            .await
    }

    async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        PostgresEventStore::query_field_index(self, tenant, entity_type, where_clause, params)
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
        let order_by = storage_order_by(order_by);
        let (entity_ids, total_count) = PostgresEventStore::query_field_index_page(
            self,
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
        self.load_query_projection_fields_many(tenant, entity_type, entity_ids, field_names)
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
        self.load_entity_catalog_rows_pg(tenant, entity_type, entity_ids)
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

    async fn load_selected_entity_catalog_rows(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        selected_fields: &[String],
    ) -> Result<Option<Vec<EntityCatalogRow>>, PersistenceError> {
        self.load_selected_entity_catalog_rows_pg(tenant, entity_type, entity_ids, selected_fields)
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
        PostgresEventStore::projected_entity_counts_by_tenant(self)
            .await
            .map(Some)
    }
}

#[async_trait::async_trait]
impl QueryPlaneStore for TursoEventStore {
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
        self.upsert_query_projection_with_state(
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

    async fn upsert_projections(
        &self,
        tenant: &str,
        projections: &[QueryProjectionUpsert],
    ) -> Result<(), PersistenceError> {
        let turso_projections = projections
            .iter()
            .map(to_turso_projection)
            .collect::<Vec<_>>();
        self.upsert_query_projections(tenant, &turso_projections)
            .await
    }

    async fn remove_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        self.remove_query_projection(tenant, entity_type, entity_id)
            .await
    }

    async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        TursoEventStore::query_field_index(self, tenant, entity_type, where_clause, params)
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
        let order_by = storage_order_by(order_by);
        let (entity_ids, total_count) = TursoEventStore::query_field_index_page(
            self,
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
        self.load_query_projection_fields_many(tenant, entity_type, entity_ids, field_names)
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
        TursoEventStore::load_entity_catalog_rows(self, tenant, entity_type, entity_ids)
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
        TursoEventStore::projected_entity_counts_by_tenant(self)
            .await
            .map(Some)
    }
}

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
}

fn to_turso_projection(projection: &QueryProjectionUpsert) -> TursoQueryProjectionUpsert {
    TursoQueryProjectionUpsert {
        entity_type: projection.entity_type.clone(),
        entity_id: projection.entity_id.clone(),
        status: projection.status.clone(),
        fields: projection.fields.clone(),
        state: projection.state.clone(),
        indexed_fields: projection.indexed_fields.clone(),
        sequence_nr: projection.sequence_nr,
        known_new: projection.known_new,
    }
}
