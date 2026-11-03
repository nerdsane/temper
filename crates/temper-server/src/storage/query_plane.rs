//! Durable query-plane storage boundary.

use std::collections::BTreeMap;

use temper_runtime::persistence::{CatalogGroupRow, PersistenceError};

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

/// One projection catalog row, and whether its field index entries are the
/// entity's committed fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogRowFreshness {
    pub entity_id: String,
    /// The entity's journal holds no event newer than the one this row was
    /// projected from. When false, the row may hold the fields from before a
    /// committed change whose projection failed or is still queued, or belong
    /// to an entity that has since been deleted.
    pub current: bool,
    /// The row's projected fields when they hold a `fields` object, the shape
    /// a write that wraps its body under `fields` leaves. The field index does
    /// not look inside that object, so a reference held there is read from
    /// these fields. `None` for every other row.
    pub wrapped_fields: Option<serde_json::Value>,
}

/// Catalog rows of one entity type that hold the same status and the same
/// value of each field asked for, from [`QueryPlaneStore::catalog_groups`].
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogGroup {
    pub status: String,
    /// The fields asked for that the rows hold, with their values.
    pub values: BTreeMap<String, serde_json::Value>,
    /// Their entity ids, in order.
    pub ids: Vec<String>,
}

impl CatalogGroup {
    /// A group from a store's row, whose values are those of `fields` in
    /// order.
    pub fn from_row(row: CatalogGroupRow, fields: &[String]) -> Self {
        debug_assert_eq!(fields.len(), row.values.len());
        Self {
            status: row.status,
            values: fields
                .iter()
                .zip(row.values)
                .filter_map(|(field, value)| Some((field.clone(), value?)))
                .collect(),
            ids: row.ids,
        }
    }

    /// Groups from a store's rows.
    pub fn from_rows(rows: Vec<CatalogGroupRow>, fields: &[String]) -> Vec<Self> {
        rows.into_iter()
            .map(|row| Self::from_row(row, fields))
            .collect()
    }
}

/// Durable query-plane capability.
#[async_trait::async_trait]
pub trait QueryPlaneStore: Send + Sync {
    /// Indexed equality candidates and every entity whose projection is not
    /// authoritative, from one storage snapshot. The second vector identifies
    /// candidates requiring an authoritative read. With `Some(budget)`, return
    /// at most `budget + 1` candidates, prioritizing pending ids: an empty second
    /// vector proves the entire gap is empty even when the probe overflows.
    /// With no probe budget, read the complete union without truncation.
    /// A `None` result means the backend cannot prove completeness (including
    /// a separately stored journal).
    async fn reconciled_equality_candidates(
        &self,
        _journal: &super::BoxedEventStore,
        _tenant: &str,
        _entity_type: &str,
        _predicates: &[(String, String)],
        _probe_budget: Option<usize>,
    ) -> Result<Option<(Vec<String>, Vec<String>)>, PersistenceError> {
        Ok(None)
    }

    /// Compare catalog checkpoints with the co-located journal in one query.
    /// Called only when the stack verifies this is the journal itself.
    async fn lagging_projections(
        &self,
        _tenant: &str,
    ) -> Result<Option<Vec<temper_runtime::persistence::ProjectionRepairCandidate>>, PersistenceError>
    {
        Ok(None)
    }

    /// All catalog checkpoints, without entity bodies, for a query plane that
    /// lives outside the journal. Combined with a batched journal-head read.
    async fn projection_checkpoints(
        &self,
        _tenant: &str,
    ) -> Result<Vec<temper_runtime::persistence::ProjectionCheckpoint>, PersistenceError> {
        Err(PersistenceError::Storage(
            "batched projection checkpoints unsupported".into(),
        ))
    }

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

    /// Rewrite an existing catalog row and its field index, as
    /// [`Self::upsert_projection`] does, and leave a record with no catalog row
    /// alone. A background pass while the server serves uses this: only a
    /// record's own commit creates its row then, and boot repair creates the
    /// missing ones before serving.
    #[expect(clippy::too_many_arguments, reason = "projection upsert boundary")]
    async fn refresh_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError>;

    async fn remove_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError>;

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

    /// Every projection catalog row of `entity_type`, compared with its
    /// entity's journal in one read. Returns `None` if the backend cannot
    /// compare them.
    async fn projection_catalog_freshness(
        &self,
        _tenant: &str,
        _entity_type: &str,
    ) -> Result<Option<Vec<CatalogRowFreshness>>, PersistenceError> {
        Ok(None)
    }

    /// The catalog rows of `entity_type` in one of `statuses` that
    /// [`Self::projection_catalog_freshness`] reads as current and unwrapped,
    /// grouped by status and by the value of each of `fields`, with their
    /// ids, in one read. Returns `None` if the backend cannot group them.
    async fn catalog_groups(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _fields: &[String],
        _statuses: &[String],
    ) -> Result<Option<Vec<CatalogGroup>>, PersistenceError> {
        Ok(None)
    }
}
