use temper_runtime::persistence::{
    EntityVectorCandidate, EntityVectorRow, PersistenceEnvelope, PersistenceError,
};

use super::BoxedEventStore;

/// Derived index rows and declaration authority co-committed with one append.
pub struct AppendIndexRows<'a> {
    /// Unique-key rows derived from the exact transition-table snapshot.
    pub key_rows: &'a [temper_runtime::persistence::EntityKeyRow],
    /// Vector rows derived from the exact transition-table snapshot.
    pub vector_rows: &'a [temper_runtime::persistence::EntityVectorRow],
    /// Whether vector rows absent from this append must be removed.
    pub reconcile_vectors: bool,
    /// Fingerprint of the exact declaration snapshot used by the writer.
    pub spec_declaration_fingerprint: Option<&'a str>,
}

impl BoxedEventStore {
    /// Append journal events and co-commit their derived key/vector index rows.
    pub async fn append_with_index_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        index_rows: AppendIndexRows<'_>,
    ) -> Result<u64, PersistenceError> {
        self.0
            .append_with_index_rows(persistence_id, expected_sequence, events, index_rows)
            .await
    }

    /// Persist one declaration fingerprint or absence tombstone.
    pub async fn persist_spec_declaration(
        &self,
        tenant: &str,
        entity_type: &str,
        declaration_fingerprint: &str,
    ) -> Result<u64, PersistenceError> {
        self.0
            .persist_spec_declaration(tenant, entity_type, declaration_fingerprint)
            .await
    }

    /// Return currently present durable declaration types for one tenant.
    pub async fn spec_declaration_entity_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.0.spec_declaration_entity_types(tenant).await
    }

    /// Replace one entity's vector rows behind a generation and sequence fence.
    pub async fn backfill_entity_vectors(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        reconciliation_generation: u64,
        observed_sequence: u64,
        vector_rows: &[EntityVectorRow],
    ) -> Result<(), PersistenceError> {
        self.0
            .backfill_entity_vectors(
                tenant,
                entity_type,
                entity_id,
                reconciliation_generation,
                observed_sequence,
                vector_rows,
            )
            .await
    }

    /// Claim the durable generation for one declaration snapshot.
    pub async fn begin_vector_index_reconciliation(
        &self,
        tenant: &str,
        entity_type: &str,
        vector_set: &str,
        declaration_revision: u64,
        declaration_fingerprint: &str,
    ) -> Result<u64, PersistenceError> {
        self.0
            .begin_vector_index_reconciliation(
                tenant,
                entity_type,
                vector_set,
                declaration_revision,
                declaration_fingerprint,
            )
            .await
    }

    /// Read bounded candidates from one declaration/model partition.
    pub async fn vector_candidates(
        &self,
        tenant: &str,
        entity_type: &str,
        decl_name: &str,
        model_tag: &str,
        limit: usize,
    ) -> Result<Vec<EntityVectorCandidate>, PersistenceError> {
        self.0
            .vector_candidates(tenant, entity_type, decl_name, model_tag, limit)
            .await
    }

    /// Publish a generation-checked completion watermark for an entity type.
    pub async fn mark_vector_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        reconciliation_generation: u64,
        vector_set: &str,
    ) -> Result<(), PersistenceError> {
        self.0
            .mark_vector_index_backfilled(
                tenant,
                entity_type,
                reconciliation_generation,
                vector_set,
            )
            .await
    }

    /// List entity types and declaration signatures with completion watermarks.
    pub async fn vector_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        self.0.vector_index_backfilled_types(tenant).await
    }

    /// List entity types with any durable vector-reconciliation state.
    pub async fn vector_reconciliation_entity_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.0.vector_reconciliation_entity_types(tenant).await
    }

    /// List entity IDs that currently retain vector candidates for a type.
    pub async fn vectored_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.0
            .vectored_entity_ids_for_type(tenant, entity_type)
            .await
    }

    /// List all journaled IDs, including deleted streams, for vector repair.
    pub async fn list_vector_repair_entity_ids(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.0
            .list_vector_repair_entity_ids(tenant, entity_type)
            .await
    }
}
