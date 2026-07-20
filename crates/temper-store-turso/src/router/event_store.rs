//! Tenant-routed event-store contract.

use super::*;

/// `EventStore` implementation that routes by tenant extracted from `persistence_id`.
impl EventStore for TenantStoreRouter {
    #[instrument(skip_all, fields(persistence_id, otel.name = "router.append"))]
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let store = self.store_for_tenant(tenant).await?;
        store
            .append(persistence_id, expected_sequence, events)
            .await
    }

    #[instrument(skip_all, fields(otel.name = "router.append_batch"))]
    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        let Some(first) = appends.first() else {
            return Ok(Vec::new());
        };
        let (tenant, _, _) =
            parse_persistence_id_parts(&first.persistence_id).map_err(PersistenceError::Storage)?;
        for append in &appends[1..] {
            let (next_tenant, _, _) = parse_persistence_id_parts(&append.persistence_id)
                .map_err(PersistenceError::Storage)?;
            if next_tenant != tenant {
                return Err(PersistenceError::Storage(format!(
                    "append_batch cannot span routed tenant databases: '{tenant}' and '{next_tenant}'"
                )));
            }
        }
        let store = self.store_for_tenant(tenant).await?;
        store.append_batch(appends).await
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "router.read_events"))]
    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let store = self.store_for_tenant(tenant).await?;
        store.read_events(persistence_id, from_sequence).await
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "router.read_events_with_head"))]
    async fn read_events_with_head(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<JournalRead, PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let store = self.store_for_tenant(tenant).await?;
        store
            .read_events_with_head(persistence_id, from_sequence)
            .await
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "router.save_snapshot"))]
    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let store = self.store_for_tenant(tenant).await?;
        store
            .save_snapshot(persistence_id, sequence_nr, snapshot)
            .await
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "router.replace_snapshot"))]
    async fn replace_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        expected_snapshot: &[u8],
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let store = self.store_for_tenant(tenant).await?;
        store
            .replace_snapshot(persistence_id, sequence_nr, expected_snapshot, snapshot)
            .await
    }

    #[instrument(skip_all, fields(persistence_id, otel.name = "router.load_snapshot"))]
    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let store = self.store_for_tenant(tenant).await?;
        store.load_snapshot(persistence_id).await
    }

    #[instrument(skip_all, fields(tenant, otel.name = "router.list_entity_ids"))]
    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store.list_entity_ids(tenant).await
    }

    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "router.list_entity_ids_by_type"))]
    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store.list_entity_ids_by_type(tenant, entity_type).await
    }

    // ADR-0155: forward the vector-index surface to the per-tenant store so kNN works
    // on the routed Turso deployment. (Keys deliberately fall through to the no-op
    // defaults — Turso does not maintain entity_key_index live; see event_store.rs.)
    #[instrument(skip_all, fields(persistence_id, otel.name = "router.append_with_index_rows"))]
    async fn append_with_index_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
        vector_rows: &[temper_runtime::persistence::EntityVectorRow],
        reconcile_vectors: bool,
    ) -> Result<u64, PersistenceError> {
        let (tenant, _, _) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let store = self.store_for_tenant(tenant).await?;
        store
            .append_with_index_rows(
                persistence_id,
                expected_sequence,
                events,
                key_rows,
                vector_rows,
                reconcile_vectors,
            )
            .await
    }

    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "router.backfill_entity_vectors"))]
    async fn backfill_entity_vectors(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        vector_rows: &[temper_runtime::persistence::EntityVectorRow],
    ) -> Result<(), PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .backfill_entity_vectors(tenant, entity_type, entity_id, vector_rows)
            .await
    }

    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "router.vector_candidates"))]
    async fn vector_candidates(
        &self,
        tenant: &str,
        entity_type: &str,
        decl_name: &str,
        model_tag: &str,
        limit: usize,
    ) -> Result<Vec<temper_runtime::persistence::EntityVectorCandidate>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .vector_candidates(tenant, entity_type, decl_name, model_tag, limit)
            .await
    }

    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "router.mark_vector_index_backfilled"))]
    async fn mark_vector_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        vector_set: &str,
    ) -> Result<(), PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .mark_vector_index_backfilled(tenant, entity_type, vector_set)
            .await
    }

    #[instrument(skip_all, fields(tenant, otel.name = "router.vector_index_backfilled_types"))]
    async fn vector_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store.vector_index_backfilled_types(tenant).await
    }

    #[instrument(skip_all, fields(tenant, entity_type, otel.name = "router.vectored_entity_ids_for_type"))]
    async fn vectored_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let store = self.store_for_tenant(tenant).await?;
        store
            .vectored_entity_ids_for_type(tenant, entity_type)
            .await
    }
}
