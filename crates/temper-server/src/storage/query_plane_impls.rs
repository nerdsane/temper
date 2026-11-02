use temper_runtime::persistence::PersistenceError;
use temper_store_postgres::PostgresEventStore;
use temper_store_turso::TursoEventStore;

use super::{
    CatalogGroup, CatalogRowFreshness, EntityCatalogRow, QueryFieldIndexOrder,
    QueryFieldIndexOrderDirection, QueryFieldIndexPage, QueryPlaneStore, QueryProjectionFieldsRow,
};

pub(super) fn catalog_freshness(
    rows: Vec<(String, bool, Option<serde_json::Value>)>,
) -> Vec<CatalogRowFreshness> {
    rows.into_iter()
        .map(|(entity_id, current, wrapped_fields)| CatalogRowFreshness {
            entity_id,
            current,
            wrapped_fields,
        })
        .collect()
}

/// Whether `journal` is `store` itself. A separately supplied journal can have
/// changes absent from this store's projection delta, so the delta's absence
/// is evidence only about the store's own journal.
fn journal_is<T>(store: &T, journal: &super::BoxedEventStore) -> bool {
    std::ptr::addr_eq(store as *const T, std::sync::Arc::as_ptr(&journal.store))
}

pub(super) fn storage_order_by(order_by: &[QueryFieldIndexOrder]) -> Vec<(String, bool)> {
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
    async fn reconciled_equality_candidates(
        &self,
        journal: &super::BoxedEventStore,
        tenant: &str,
        entity_type: &str,
        predicates: &[(String, String)],
        probe_budget: Option<usize>,
    ) -> Result<Option<(Vec<String>, Vec<String>)>, PersistenceError> {
        if !journal_is(self, journal) {
            return Ok(None);
        }
        self.reconciled_equality_candidates_pg(tenant, entity_type, predicates, probe_budget)
            .await
            .map(Some)
    }

    async fn lagging_projections(
        &self,
        tenant: &str,
    ) -> Result<Option<Vec<temper_runtime::persistence::ProjectionRepairCandidate>>, PersistenceError>
    {
        self.lagging_query_projections(tenant).await.map(Some)
    }
    async fn projection_checkpoints(
        &self,
        tenant: &str,
    ) -> Result<Vec<temper_runtime::persistence::ProjectionCheckpoint>, PersistenceError> {
        self.query_projection_checkpoints(tenant).await
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
        self.refresh_query_projection_with_state(
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

    async fn projection_catalog_freshness(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Option<Vec<CatalogRowFreshness>>, PersistenceError> {
        self.projection_catalog_freshness_pg(tenant, entity_type)
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
        let rows = self
            .catalog_groups_pg(tenant, entity_type, fields, statuses)
            .await?;
        Ok(Some(CatalogGroup::from_rows(rows, fields)))
    }
}

#[async_trait::async_trait]
impl QueryPlaneStore for TursoEventStore {
    async fn lagging_projections(
        &self,
        tenant: &str,
    ) -> Result<Option<Vec<temper_runtime::persistence::ProjectionRepairCandidate>>, PersistenceError>
    {
        self.lagging_query_projections(tenant).await.map(Some)
    }
    async fn projection_checkpoints(
        &self,
        tenant: &str,
    ) -> Result<Vec<temper_runtime::persistence::ProjectionCheckpoint>, PersistenceError> {
        self.query_projection_checkpoints(tenant).await
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
        self.refresh_query_projection_with_state(
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

    async fn reconciled_equality_candidates(
        &self,
        journal: &super::BoxedEventStore,
        tenant: &str,
        entity_type: &str,
        predicates: &[(String, String)],
        probe_budget: Option<usize>,
    ) -> Result<Option<(Vec<String>, Vec<String>)>, PersistenceError> {
        if !journal_is(self, journal) {
            return Ok(None);
        }
        TursoEventStore::reconciled_equality_candidates(
            self,
            tenant,
            entity_type,
            predicates,
            probe_budget,
        )
        .await
        .map(Some)
    }

    async fn projection_catalog_freshness(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Option<Vec<CatalogRowFreshness>>, PersistenceError> {
        TursoEventStore::projection_catalog_freshness(self, tenant, entity_type)
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
        let rows =
            TursoEventStore::catalog_groups(self, tenant, entity_type, fields, statuses).await?;
        Ok(Some(CatalogGroup::from_rows(rows, fields)))
    }
}
