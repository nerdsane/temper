//! Durable query-plane storage boundary.

use std::collections::BTreeMap;

use temper_runtime::persistence::{PersistenceError, ProjectionSourceFence};

/// Backend-neutral projection row returned by [`QueryPlaneStore`].
#[derive(Clone, Debug, PartialEq)]
pub struct QueryProjectionFieldsRow {
    pub entity_id: String,
    pub status: String,
    pub fields: BTreeMap<String, Option<String>>,
}

/// Sort direction for a storage-pushed query-field page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueryFieldIndexOrderDirection {
    /// Ascending order.
    Asc,
    /// Descending order.
    Desc,
}

/// One storage-pushed query-field page order clause.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryFieldIndexOrder {
    /// Projection field, `status`, or `entity_id` to order by.
    pub field_name: String,
    /// Sort direction.
    pub direction: QueryFieldIndexOrderDirection,
}

/// Bounded page of entity IDs returned by the query projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryFieldIndexPage {
    /// Final entity IDs to hydrate for the current page.
    pub entity_ids: Vec<String>,
    /// Count of matching entities before pagination when requested.
    pub total_count: Option<usize>,
}

/// Full entity row materialized from the durable query-plane catalog.
///
/// Returned by [`QueryPlaneStore::load_entity_catalog_rows`] and used by the
/// OData read path to skip actor hydration on collection materialization.
#[derive(Clone, Debug, PartialEq)]
pub struct EntityCatalogRow {
    pub entity_id: String,
    pub status: String,
    /// Raw JSONB fields object as written by the projection upsert.
    pub fields: serde_json::Value,
    /// Full projected entity response state, with unbounded event history removed.
    pub state: Option<serde_json::Value>,
    pub sequence_nr: u64,
}

/// Backend-neutral durable projection upsert.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryProjectionUpsert {
    pub entity_type: String,
    pub entity_id: String,
    pub status: String,
    pub fields: serde_json::Value,
    pub state: serde_json::Value,
    pub indexed_fields: serde_json::Value,
    pub sequence_nr: u64,
    pub known_new: bool,
}

/// Durable query-plane capability.
#[async_trait::async_trait]
pub trait QueryPlaneStore: Send + Sync {
    #[expect(clippy::too_many_arguments, reason = "projection upsert boundary")]
    async fn upsert_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError>;

    /// Upsert a projection only while the exact journal and snapshot generation
    /// used to derive it is still current. The source comparison and mutation
    /// must share one backend transaction. `false` means no mutation occurred.
    #[expect(
        clippy::too_many_arguments,
        reason = "source-fenced projection boundary"
    )]
    async fn upsert_projection_if_source(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_id: &str,
        _status: &str,
        _fields: &serde_json::Value,
        _state: &serde_json::Value,
        _sequence_nr: u64,
        _source: ProjectionSourceFence<'_>,
    ) -> Result<bool, PersistenceError> {
        Err(PersistenceError::Storage(
            "query plane does not support source-fenced projection repair".to_string(),
        ))
    }

    async fn upsert_projections(
        &self,
        tenant: &str,
        projections: &[QueryProjectionUpsert],
    ) -> Result<(), PersistenceError> {
        for projection in projections {
            self.upsert_projection(
                tenant,
                &projection.entity_type,
                &projection.entity_id,
                &projection.status,
                &projection.fields,
                &projection.state,
                projection.sequence_nr,
            )
            .await?;
        }
        Ok(())
    }

    async fn remove_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError>;

    /// Remove a projection only while the exact journal and snapshot generation
    /// used to classify the entity as deleted is still current.
    async fn remove_projection_if_source(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_id: &str,
        _source: ProjectionSourceFence<'_>,
    ) -> Result<bool, PersistenceError> {
        Err(PersistenceError::Storage(
            "query plane does not support source-fenced projection repair".to_string(),
        ))
    }

    /// Clear a durable dirty marker only while the exact journal and snapshot
    /// source is current, without changing a compatibility catalog row.
    async fn clear_projection_dirty_if_source(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_id: &str,
        _source: ProjectionSourceFence<'_>,
    ) -> Result<bool, PersistenceError> {
        Err(PersistenceError::Storage(
            "query plane does not support source-fenced dirty acknowledgement".to_string(),
        ))
    }

    /// Remove exactly the attempted projection row after its durable source was
    /// observed to change. A different or newer catalog row is never removed.
    #[expect(
        clippy::too_many_arguments,
        reason = "exact projection cleanup boundary"
    )]
    async fn remove_projection_if_exact(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_id: &str,
        _status: &str,
        _fields: &serde_json::Value,
        _state: &serde_json::Value,
        _sequence_nr: u64,
    ) -> Result<bool, PersistenceError> {
        Err(PersistenceError::Storage(
            "query plane does not support exact projection cleanup".to_string(),
        ))
    }

    async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError>;

    #[expect(
        clippy::too_many_arguments,
        reason = "bounded projection page query boundary"
    )]
    async fn query_field_index_page(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _where_clause: &str,
        _params: Vec<String>,
        _order_by: &[QueryFieldIndexOrder],
        _skip: usize,
        _top: usize,
        _include_count: bool,
    ) -> Result<Option<QueryFieldIndexPage>, PersistenceError> {
        Ok(None)
    }

    async fn load_projection_fields_many(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError>;

    /// Batch-fetch full entity rows from the projection catalog.
    ///
    /// Returns a partial result - only rows present in the catalog. IDs not in
    /// the catalog (yet to be projected, or never written) are simply absent
    /// from the response. Returns `None` if the backend cannot service catalog
    /// reads at all.
    async fn load_entity_catalog_rows(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_ids: &[String],
    ) -> Result<Option<Vec<EntityCatalogRow>>, PersistenceError> {
        Ok(None)
    }

    /// Batch-fetch catalog rows with only the JSON properties required by a
    /// safe `$select` collection read.
    ///
    /// The returned rows preserve `entity_id`, `status`, and `sequence_nr`,
    /// but may set `state` to `None` and store the selected properties in
    /// `fields`. Backends that cannot perform a sparse catalog load return
    /// `None`, allowing callers to use [`QueryPlaneStore::load_entity_catalog_rows`].
    async fn load_selected_entity_catalog_rows(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_ids: &[String],
        _selected_fields: &[String],
    ) -> Result<Option<Vec<EntityCatalogRow>>, PersistenceError> {
        Ok(None)
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError>;

    /// Return at most `limit` entities whose durable source changed after their
    /// last source-fenced projection reconciliation. `None` means the backend
    /// does not expose durable dirty-projection tracking.
    async fn dirty_projection_entity_ids(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _limit: usize,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        Ok(None)
    }
}
