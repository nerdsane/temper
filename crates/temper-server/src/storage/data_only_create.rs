use temper_runtime::persistence::{PersistenceEnvelope, PersistenceError};

/// Inputs for a native brand-new data-only entity create.
///
/// This capability is only valid for entities whose first durable event and
/// first query projection row can be inserted atomically by a storage backend.
pub struct DataOnlyCreateRecord<'a> {
    /// Tenant that owns the entity.
    pub tenant: &'a str,
    /// Entity type being created.
    pub entity_type: &'a str,
    /// Entity id being created.
    pub entity_id: &'a str,
    /// Initial entity status.
    pub status: &'a str,
    /// Projection fields to store in the query catalog and scalar index.
    pub fields: &'a serde_json::Value,
    /// Full response projection to store in the query catalog.
    pub state: &'a serde_json::Value,
    /// First event envelope to append at sequence number 1.
    pub event: &'a PersistenceEnvelope,
    /// Fingerprint of the exact table snapshot that derived the event.
    pub spec_declaration_fingerprint: Option<&'a str>,
}

/// Optional native storage capability for brand-new data-only creates.
#[async_trait::async_trait]
pub trait DataOnlyCreateStore: Send + Sync {
    /// Persist the first event and initial projection atomically.
    ///
    /// Returns the new sequence number on success. Duplicate first events or
    /// duplicate projection rows should return [`PersistenceError::ConcurrencyViolation`]
    /// so the caller can decline the fast path and use the generic path.
    async fn create_data_only_entity(
        &self,
        record: DataOnlyCreateRecord<'_>,
    ) -> Result<u64, PersistenceError>;
}
