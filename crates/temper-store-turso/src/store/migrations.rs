//! Turso schema migration orchestration.

use super::*;

impl TursoEventStore {
    /// Run schema migrations on connect.
    #[instrument(skip_all, fields(otel.name = "turso.migrate"))]
    pub(super) async fn migrate(&self) -> Result<(), PersistenceError> {
        let conn = self.connection()?;

        // PRAGMAs are SQLite-specific and not supported on remote Turso Cloud.
        // Turso Cloud manages its own journal mode and concurrency.
        if !self.is_remote {
            let _ = conn
                .query("PRAGMA journal_mode=WAL", ())
                .await
                .map_err(storage_error)?;
            let _ = conn
                .query("PRAGMA busy_timeout=30000", ())
                .await
                .map_err(storage_error)?;
        }

        conn.execute(schema::CREATE_EVENTS_TABLE, ())
            .await
            .map_err(storage_error)?;
        let _ = conn
            .execute(schema::ALTER_EVENTS_ADD_SEGMENT_INDEX, ())
            .await;
        conn.execute(schema::CREATE_EVENTS_ENTITY_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_EVENT_SEGMENTS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_EVENT_SEGMENTS_OPEN_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_SNAPSHOTS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_SNAPSHOT_HISTORY_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_SNAPSHOT_HISTORY_ENTITY_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_SPECS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_TRAJECTORIES_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_TRAJECTORIES_SUCCESS_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_TRAJECTORIES_ENTITY_ACTION_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_TENANT_CONSTRAINTS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_WASM_MODULES_TABLE, ())
            .await
            .map_err(storage_error)?;
        // Idempotent ALTER for pre-existing DBs created before the source
        // column existed. Turso has no IF NOT EXISTS for ADD COLUMN, so we
        // ignore "duplicate column" errors.
        match conn
            .execute(schema::ADD_WASM_MODULES_SOURCE_COLUMN, ())
            .await
        {
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string();
                if !msg.contains("duplicate column")
                    && !msg.contains("already exists")
                    && !msg.contains("already has")
                {
                    return Err(storage_error(e));
                }
            }
        }
        conn.execute(schema::CREATE_WASM_INVOCATION_LOGS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_WASM_INVOCATION_LOGS_TENANT_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_WASM_INVOCATION_LOGS_MODULE_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_WASM_INVOCATION_LOGS_CREATED_INDEX, ())
            .await
            .map_err(storage_error)?;

        conn.execute(schema::CREATE_PENDING_DECISIONS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_PENDING_DECISIONS_TENANT_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_PENDING_DECISIONS_STATUS_INDEX, ())
            .await
            .map_err(storage_error)?;

        conn.execute(schema::CREATE_TENANT_POLICIES_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_POLICIES_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_POLICY_DENIAL_PATTERNS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_POLICY_DENIAL_PATTERNS_TENANT_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_PUBLISHED_ARTIFACTS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_PUBLISHED_ARTIFACTS_OWNER_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_PUBLISHED_ARTIFACTS_SOURCE_INDEX, ())
            .await
            .map_err(storage_error)?;
        // Migration: add `enabled` column to existing `policies` tables.
        let _ = conn.execute(schema::ALTER_POLICIES_ADD_ENABLED, ()).await;
        conn.execute(schema::CREATE_TENANT_INSTALLED_APPS_TABLE, ())
            .await
            .map_err(storage_error)?;
        for stmt in [
            schema::ALTER_INSTALLED_APPS_ADD_APP_VERSION,
            schema::ALTER_INSTALLED_APPS_ADD_SOURCE_KIND,
            schema::ALTER_INSTALLED_APPS_ADD_APP_REF,
            schema::ALTER_INSTALLED_APPS_ADD_VERSION_HASH,
            schema::ALTER_INSTALLED_APPS_ADD_PINNED_VERSION_HASH,
            schema::ALTER_INSTALLED_APPS_ADD_CURRENT_VERSION_HASH,
            schema::ALTER_INSTALLED_APPS_ADD_FOLLOW_POLICY,
            schema::ALTER_INSTALLED_APPS_ADD_CLOSURE_ID,
            schema::ALTER_INSTALLED_APPS_ADD_REGISTRY_URL,
            schema::ALTER_INSTALLED_APPS_ADD_REGISTRY_TENANT,
            schema::ALTER_INSTALLED_APPS_ADD_BUNDLE_DIGEST,
            schema::ALTER_INSTALLED_APPS_ADD_SPEC_DIGEST,
            schema::ALTER_INSTALLED_APPS_ADD_POLICY_DIGEST,
            schema::ALTER_INSTALLED_APPS_ADD_WASM_DIGEST,
            schema::ALTER_INSTALLED_APPS_ADD_CONTENT_DIGEST,
            schema::ALTER_INSTALLED_APPS_ADD_SEED_DIGEST,
            schema::ALTER_INSTALLED_APPS_ADD_LAST_RECONCILED_AT,
            schema::ALTER_INSTALLED_APPS_ADD_STATUS,
        ] {
            let _ = conn.execute(stmt, ()).await;
        }

        // Phase 0: New tables for Turso-as-single-source-of-truth.
        conn.execute(schema::CREATE_FEATURE_REQUESTS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_EVOLUTION_RECORDS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_EVOLUTION_RECORDS_TYPE_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_EVOLUTION_RECORDS_STATUS_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_DESIGN_TIME_EVENTS_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_DESIGN_TIME_EVENTS_TENANT_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_TENANT_SECRETS_TABLE, ())
            .await
            .map_err(storage_error)?;

        conn.execute(schema::CREATE_TENANT_SECRETS_TABLE, ())
            .await
            .map_err(storage_error)?;

        // Specs table extensions — add content_hash column for verification caching.
        let _ = conn.execute(schema::ALTER_SPECS_ADD_CONTENT_HASH, ()).await;
        let _ = conn.execute(schema::ALTER_SPECS_ADD_COMMITTED, ()).await;

        // Trajectory table extensions — ALTER TABLE to add missing columns.
        // SQLite returns an error for duplicate columns, so we ignore failures.
        for stmt in &[
            schema::ALTER_TRAJECTORIES_ADD_AGENT_ID,
            schema::ALTER_TRAJECTORIES_ADD_SESSION_ID,
            schema::ALTER_TRAJECTORIES_ADD_AUTHZ_DENIED,
            schema::ALTER_TRAJECTORIES_ADD_DENIED_RESOURCE,
            schema::ALTER_TRAJECTORIES_ADD_DENIED_MODULE,
            schema::ALTER_TRAJECTORIES_ADD_SOURCE,
            schema::ALTER_TRAJECTORIES_ADD_SPEC_GOVERNED,
            schema::ALTER_TRAJECTORIES_ADD_REQUEST_BODY,
            schema::ALTER_TRAJECTORIES_ADD_INTENT,
            schema::ALTER_TRAJECTORIES_ADD_MATCHED_POLICY_IDS,
        ] {
            let _ = conn.execute(stmt, ()).await; // ignore "duplicate column" errors
        }
        conn.execute(schema::CREATE_TRAJECTORIES_AGENT_INDEX, ())
            .await
            .map_err(storage_error)?;

        // OTS trajectory storage — full agent execution traces for GEPA.
        conn.execute(schema::CREATE_OTS_TRAJECTORIES_TABLE, ())
            .await
            .map_err(storage_error)?;
        for stmt in [
            schema::ALTER_OTS_TRAJECTORIES_ADD_PERSISTENCE_STATUS,
            schema::ALTER_OTS_TRAJECTORIES_ADD_PERSIST_ATTEMPTS,
            schema::ALTER_OTS_TRAJECTORIES_ADD_LAST_ERROR,
            schema::ALTER_OTS_TRAJECTORIES_ADD_UPDATED_AT,
        ] {
            let _ = conn.execute(stmt, ()).await;
        }
        conn.execute(schema::CREATE_OTS_TRAJECTORIES_AGENT_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_OTS_TRAJECTORIES_TENANT_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_OTS_TRAJECTORIES_OUTCOME_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_OTS_TRAJECTORIES_STATUS_INDEX, ())
            .await
            .map_err(storage_error)?;

        // Blob storage — content-addressed binary objects for TemperFS and
        // field-overflow blob refs (ADR-0040).
        conn.execute(schema::CREATE_BLOBS_TABLE, ())
            .await
            .map_err(storage_error)?;
        // ADR-0047: idempotent migration that adds `expires_at` to pre-existing
        // blobs tables. Duplicate-column errors are expected on newer deployments
        // that already have the column from `CREATE_BLOBS_TABLE`; swallow them.
        if let Err(error) = conn.execute(schema::ALTER_BLOBS_ADD_EXPIRES_AT, ()).await {
            let message = error.to_string().to_ascii_lowercase();
            if !message.contains("duplicate column") {
                return Err(storage_error(error));
            }
        }
        conn.execute(schema::CREATE_BLOBS_EXPIRES_AT_INDEX, ())
            .await
            .map_err(storage_error)?;

        // Entity catalog — durable query-plane corpus.
        conn.execute(schema::CREATE_ENTITY_CATALOG_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_ENTITY_CATALOG_TYPE_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_ENTITY_CATALOG_STATUS_INDEX, ())
            .await
            .map_err(storage_error)?;
        let _ = conn
            .execute(schema::ALTER_ENTITY_CATALOG_ADD_PROJECTION_HASH, ())
            .await;
        let _ = conn
            .execute(schema::ALTER_ENTITY_CATALOG_ADD_FIELDS, ())
            .await;
        let _ = conn
            .execute(schema::ALTER_ENTITY_CATALOG_ADD_STATE, ())
            .await;
        conn.execute(schema::CREATE_QUERY_PROJECTION_TOMBSTONES_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_QUERY_PROJECTION_TOMBSTONE_INSERT_TRIGGER, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_QUERY_PROJECTION_TOMBSTONE_UPDATE_TRIGGER, ())
            .await
            .map_err(storage_error)?;

        // Entity field index — EAV table for OData filter push-down.
        conn.execute(schema::CREATE_ENTITY_FIELD_INDEX_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_ENTITY_FIELD_INDEX_LOOKUP, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_ENTITY_FIELD_INDEX_STATUS, ())
            .await
            .map_err(storage_error)?;

        // Entity key index (ADR-0153) — declared composite-key -> entity_id, the
        // negative-existence access path co-committed with the journal append.
        conn.execute(schema::CREATE_ENTITY_KEY_INDEX_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_ENTITY_KEY_INDEX_ENTITY, ())
            .await
            .map_err(storage_error)?;

        // Entity vector index (ADR-0155) — declared vector paths for exact-scan kNN,
        // maintained write-behind (the event append is followed by the index write).
        conn.execute(schema::CREATE_ENTITY_VECTOR_INDEX_TABLE, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_ENTITY_VECTOR_INDEX_PARTITION, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_ENTITY_VECTOR_INDEX_ENTITY, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_VECTOR_INDEX_BACKFILL_WATERMARK, ())
            .await
            .map_err(storage_error)?;

        Ok(())
    }
}
