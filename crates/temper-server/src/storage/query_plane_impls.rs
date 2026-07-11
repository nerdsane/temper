use temper_runtime::persistence::PersistenceError;
use temper_store_postgres::PostgresEventStore;

use super::{
    EntityCatalogRow, QueryFieldIndexOrder, QueryFieldIndexOrderDirection, QueryFieldIndexPage,
    QueryPlaneStore, QueryProjectionFieldsRow,
};

fn storage_order_by(order_by: &[QueryFieldIndexOrder]) -> Vec<(String, bool)> {
    order_by
        .iter()
        .map(|order| {
            (
                order.field_name.clone(),
                order.direction == QueryFieldIndexOrderDirection::Desc,
            )
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

    async fn remove_projection_through_sequence(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        self.remove_query_projection_through_sequence(tenant, entity_type, entity_id, sequence_nr)
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
