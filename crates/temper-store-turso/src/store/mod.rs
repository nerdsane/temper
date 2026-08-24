//! Turso/libSQL-backed implementation of the [`EventStore`] trait.
//!
//! Split into domain-focused sub-modules for cohesion:
//! - [`specs`]: Spec CRUD (upsert, verification, load)
//! - [`trajectory`] / [`trajectory_queries`]: Trajectory writes and reads
//! - [`evolution`]: Feature requests, evolution records, design-time events
//! - [`authz`]: Authorization decisions and Cedar policies
//! - [`wasm`]: WASM module storage and invocation logs
//! - [`constraints`]: Tenant-level cross-entity constraints
//! - [`event_store`]: [`EventStore`] trait implementation

use libsql::{Builder, Database};
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tokio::sync::Semaphore;
use tracing::instrument;

use crate::schema;

mod append_config;
mod authz;
mod blobs;
mod constraints;
mod entity_listing;
mod event_store;
mod evolution;
pub mod field_index;
mod instrumentation;
mod migration_support;
pub mod ots;
mod policy;
mod policy_approval;
mod published_artifacts;
mod query_page;
mod schema_deployment;
mod secrets;
mod specs;
#[cfg(test)]
mod tests;
mod trajectory;
mod trajectory_queries;
mod wasm;
mod write_gate;

pub use field_index::QueryProjectionUpsert;
use instrumentation::InstrumentedConnection;
pub use published_artifacts::{PublishedArtifactRow, PublishedArtifactUpsert};

#[derive(Clone, Debug)]
pub struct TursoEventStore {
    db: Arc<Database>,
    write_gate: Arc<Semaphore>,
    high_priority_write_waiters: Arc<AtomicUsize>,
    /// True when connected to a remote Turso Cloud instance (libsql:// URL).
    /// PRAGMAs are skipped for remote connections.
    is_remote: bool,
}

impl TursoEventStore {
    /// Connect to a Turso database.
    ///
    /// `url`: `"libsql://your-db.turso.io"` or `"file:local.db"` for local SQLite.
    /// `auth_token`: Turso auth token (`None` for local SQLite).
    #[instrument(skip_all, fields(otel.name = "turso.new"))]
    pub async fn new(url: &str, auth_token: Option<&str>) -> Result<Self, PersistenceError> {
        let db = if url.starts_with("libsql://") {
            let token = auth_token.ok_or_else(|| {
                tracing::error!("auth token is required for libsql:// URLs");
                PersistenceError::Storage("auth token is required for libsql:// URLs".to_string())
            })?;
            Builder::new_remote(url.to_string(), token.to_string())
                .build()
                .await
                .map_err(storage_error)?
        } else {
            let local_path = url.strip_prefix("file:").unwrap_or(url);
            Builder::new_local(local_path)
                .build()
                .await
                .map_err(storage_error)?
        };

        let is_remote = url.starts_with("libsql://");
        let store = Self {
            db: Arc::new(db),
            write_gate: Arc::new(Semaphore::new(write_gate::configured_write_concurrency(
                is_remote,
            ))),
            high_priority_write_waiters: Arc::new(AtomicUsize::new(0)),
            is_remote,
        };
        store.migrate().await?;
        Ok(store)
    }

    /// Obtain a connection configured for local-SQLite concurrency.
    ///
    /// WAL mode is set in `migrate()` (persists in the DB file). `busy_timeout`
    /// is a per-connection setting — 30 s gives concurrent verification threads
    /// time to wait for the write lock instead of immediately returning SQLITE_BUSY.
    #[instrument(skip_all, fields(otel.name = "turso.configured_connection"))]
    async fn configured_connection(&self) -> Result<InstrumentedConnection, PersistenceError> {
        let conn = InstrumentedConnection::new(self.db.connect().map_err(storage_error)?);
        if !self.is_remote {
            let _ = conn
                .query("PRAGMA busy_timeout=30000", ())
                .await
                .map_err(storage_error)?;
        }
        Ok(conn)
    }

    /// Run schema migrations on connect.
    #[instrument(skip_all, fields(otel.name = "turso.migrate"))]
    async fn migrate(&self) -> Result<(), PersistenceError> {
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
        conn.execute(
            schema::schema_deployment::CREATE_SCHEMA_DEPLOYMENTS_TABLE,
            (),
        )
        .await
        .map_err(storage_error)?;
        conn.execute(
            schema::schema_deployment::CREATE_SCHEMA_DEPLOYMENT_IDEMPOTENCY_TABLE,
            (),
        )
        .await
        .map_err(storage_error)?;
        conn.execute(
            schema::schema_deployment::CREATE_SCHEMA_VERIFICATION_RECEIPTS_TABLE,
            (),
        )
        .await
        .map_err(storage_error)?;
        conn.execute(
            schema::schema_deployment::CREATE_SCHEMA_ACTIVE_POINTERS_TABLE,
            (),
        )
        .await
        .map_err(storage_error)?;
        conn.execute(
            schema::schema_deployment::CREATE_SCHEMA_MIGRATION_JOBS_TABLE,
            (),
        )
        .await
        .map_err(storage_error)?;
        conn.execute(
            schema::schema_deployment::CREATE_SCHEMA_MIGRATION_IDEMPOTENCY_TABLE,
            (),
        )
        .await
        .map_err(storage_error)?;
        conn.execute(
            schema::schema_deployment::CREATE_SCHEMA_MIGRATION_RETRY_IDEMPOTENCY_TABLE,
            (),
        )
        .await
        .map_err(storage_error)?;
        conn.execute(
            schema::schema_deployment::CREATE_SCHEMA_MIGRATION_SHADOW_TABLE,
            (),
        )
        .await
        .map_err(storage_error)?;
        conn.execute(
            schema::schema_deployment::CREATE_SCHEMA_MIGRATION_BATCH_RECEIPTS_TABLE,
            (),
        )
        .await
        .map_err(storage_error)?;
        conn.execute(
            schema::schema_deployment::CREATE_SCHEMA_MIGRATION_VALIDATION_RECEIPTS_TABLE,
            (),
        )
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
            schema::ALTER_INSTALLED_APPS_ADD_DEPENDENCY_LOCK_DIGEST,
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
        migration_support::add_column_if_missing(&conn, schema::ALTER_FEATURE_REQUESTS_ADD_TENANT)
            .await?;
        migration_support::add_column_if_missing(&conn, schema::ALTER_EVOLUTION_RECORDS_ADD_TENANT)
            .await?;
        conn.execute(schema::CREATE_FEATURE_REQUESTS_TENANT_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_EVOLUTION_RECORDS_TENANT_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_EVOLUTION_RECORDS_TENANT_PARENT_INDEX, ())
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
            schema::ALTER_TRAJECTORIES_ADD_CAPTURE_SEQ,
        ] {
            let _ = conn.execute(stmt, ()).await; // ignore "duplicate column" errors
        }
        conn.execute(schema::CREATE_TRAJECTORIES_AGENT_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_TRAJECTORIES_SESSION_INDEX, ())
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
        // Runs after the column migrations, so the rebuild copies a table that
        // already has every column, and before the indexes, which the rebuild
        // drops along with the old table.
        Self::rebuild_ots_trajectories_for_tenant_identity(&conn).await?;
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

    /// Rekey an existing `ots_trajectories` table from `trajectory_id` alone to
    /// `(tenant, trajectory_id)`.
    ///
    /// SQLite has no `ALTER TABLE ... PRIMARY KEY`, so the only way to change a
    /// key is to rebuild the table. The rebuild runs once: it reads the stored
    /// DDL first and returns immediately on a table that is already scoped, so
    /// it costs one `sqlite_master` read per startup after that.
    ///
    /// All four steps run in one transaction. Run as separate statements, a
    /// process that died between the `DROP` and the `RENAME` would leave no
    /// `ots_trajectories` at all: the next boot's `CREATE TABLE IF NOT EXISTS`
    /// would make an empty one, already carrying the tenant-scoped marker, so
    /// this function would skip the rebuild and every stored trajectory would
    /// stay stranded in `ots_trajectories_rebuild`. SQLite's DDL is
    /// transactional, so the whole rekey either lands or does not.
    async fn rebuild_ots_trajectories_for_tenant_identity(
        conn: &InstrumentedConnection,
    ) -> Result<(), PersistenceError> {
        // Scoped so the cursor is closed before the rebuild runs: an open read
        // on `sqlite_master` holds a lock that `DROP TABLE` cannot take.
        let ddl = {
            let mut rows = conn
                .query(schema::SELECT_OTS_TRAJECTORIES_DDL, ())
                .await
                .map_err(storage_error)?;
            match rows.next().await.map_err(storage_error)? {
                Some(row) => row.get::<String>(0).unwrap_or_default(),
                // No table at all: the CREATE above did not run, so there is
                // nothing to rebuild and nothing to check.
                None => return Ok(()),
            }
        };
        if ddl.contains(schema::OTS_TRAJECTORIES_TENANT_IDENTITY_MARKER) {
            return Ok(());
        }

        tracing::info!("rekeying ots_trajectories by (tenant, trajectory_id)");
        conn.execute("BEGIN IMMEDIATE", ())
            .await
            .map_err(storage_error)?;
        for stmt in [
            schema::CREATE_OTS_TRAJECTORIES_REBUILD_TABLE,
            schema::COPY_OTS_TRAJECTORIES_INTO_REBUILD,
            schema::DROP_OTS_TRAJECTORIES_LEGACY_TABLE,
            schema::RENAME_OTS_TRAJECTORIES_REBUILD,
        ] {
            if let Err(error) = conn.execute(stmt, ()).await {
                // Leaving the transaction open would hold a write lock for the
                // life of the connection, so the rollback runs before the
                // error is returned and its own failure is reported alongside.
                if let Err(rollback) = conn.execute("ROLLBACK", ()).await {
                    tracing::error!(
                        error = %rollback,
                        "failed to roll back the ots_trajectories rekey"
                    );
                }
                return Err(storage_error(error));
            }
        }
        conn.execute("COMMIT", ()).await.map_err(storage_error)?;
        Ok(())
    }

    /// Obtain a connection handle to the underlying database.
    ///
    /// `Database::connect()` returns a lightweight handle, **not** a fresh TCP
    /// connection each time:
    /// - **Local SQLite** (`file:` URLs): a handle to the same underlying
    ///   database file — no network overhead.
    /// - **Remote Turso** (`libsql://` URLs): a handle drawn from an internal
    ///   HTTP/gRPC connection pool managed by the `libsql` crate.
    ///
    /// It is safe (and cheap) to call this at the start of every method.
    pub(crate) fn connection(&self) -> Result<InstrumentedConnection, PersistenceError> {
        Ok(InstrumentedConnection::new(
            self.db.connect().map_err(storage_error)?,
        ))
    }
}

// ---------------------------------------------------------------------------
// Row / result types
// ---------------------------------------------------------------------------

pub use policy::PolicyRow;

/// Durable denial-pattern row used to rebuild policy suggestions.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PolicyDenialPatternRow {
    /// Tenant that owns the denial history.
    pub tenant: String,
    /// Agent type, when known.
    pub agent_type: Option<String>,
    /// Action that was denied.
    pub action: String,
    /// Resource type that was denied.
    pub resource_type: String,
    /// Total denial count for this pattern.
    pub count: i64,
    /// First timestamp seen for the pattern.
    pub first_seen: String,
    /// Most recent timestamp seen for the pattern.
    pub last_seen: String,
    /// JSON array of sampled resource IDs.
    pub distinct_resource_ids_json: String,
}

/// Complete query-plane projection row used by storage migrations.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TursoQueryProjectionRow {
    /// Tenant name.
    pub tenant: String,
    /// Entity type name.
    pub entity_type: String,
    /// Entity identifier.
    pub entity_id: String,
    /// Current entity status.
    pub status: String,
    /// Full projected fields JSON preserved in the durable catalog.
    pub fields: serde_json::Value,
    /// Latest sequence number represented by this projection.
    pub sequence_nr: u64,
}

/// Blob row used by storage migrations.
#[derive(Debug, Clone)]
pub struct TursoBlobRow {
    /// Content-addressed blob key.
    pub blob_key: String,
    /// Raw blob bytes.
    pub data: Vec<u8>,
    /// Stored size in bytes.
    pub size_bytes: i64,
    /// Source creation timestamp.
    pub created_at: String,
    /// Optional source expiration timestamp.
    pub expires_at: Option<String>,
}

/// Row returned by [`TursoEventStore::load_specs()`].
#[derive(Debug, Clone)]
pub struct TursoSpecRow {
    /// Tenant name.
    pub tenant: String,
    /// Entity type name.
    pub entity_type: String,
    /// IOA TOML source.
    pub ioa_source: String,
    /// CSDL XML (may be absent for old rows).
    pub csdl_xml: Option<String>,
    /// Verification status string (pending/running/passed/failed/partial).
    pub verification_status: String,
    /// Whether the spec has been verified.
    pub verified: bool,
    /// Number of verification levels that passed.
    pub levels_passed: Option<i32>,
    /// Total number of verification levels.
    pub levels_total: Option<i32>,
    /// Serialized verification result JSON.
    pub verification_result: Option<String>,
    /// SHA-256 hex digest of the IOA source content.
    pub content_hash: Option<String>,
    /// ISO-8601 updated_at timestamp.
    pub updated_at: String,
    /// Whether this spec has been committed (WAL-style commit flag).
    pub committed: bool,
}

/// Row returned by installed OS app metadata queries.
#[derive(Debug, Clone)]
pub struct TursoInstalledAppRow {
    pub tenant_id: String,
    pub app_name: String,
    pub source_kind: String,
    pub app_ref: String,
    pub version_hash: String,
    pub pinned_version_hash: String,
    pub current_version_hash: String,
    pub follow_policy: String,
    pub closure_id: String,
    pub registry_url: String,
    pub registry_tenant: String,
    pub dependency_lock_digest: String,
    pub app_version: String,
    pub bundle_digest: String,
    pub spec_digest: String,
    pub policy_digest: String,
    pub wasm_digest: String,
    pub content_digest: String,
    pub seed_digest: String,
    pub installed_at: String,
    pub last_reconciled_at: Option<String>,
    pub status: String,
}

/// Row returned by trajectory queries.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TursoTrajectoryRow {
    /// Tenant name.
    pub tenant: String,
    /// Entity type name.
    pub entity_type: String,
    /// Entity ID.
    pub entity_id: String,
    /// Action name.
    pub action: String,
    /// Whether the action succeeded.
    pub success: bool,
    /// Status before the action.
    pub from_status: Option<String>,
    /// Status after the action.
    pub to_status: Option<String>,
    /// Error description (for failed intents).
    pub error: Option<String>,
    /// Agent identity that performed the action.
    pub agent_id: Option<String>,
    /// Session the action belonged to.
    pub session_id: Option<String>,
    /// Whether this was an authorization denial.
    pub authz_denied: Option<bool>,
    /// Denied resource identifier.
    pub denied_resource: Option<String>,
    /// WASM module involved in the denial.
    pub denied_module: Option<String>,
    /// Source: "Entity", "Platform", "Authz".
    pub source: Option<String>,
    /// Whether the action is governed by a spec.
    pub spec_governed: Option<bool>,
    /// ISO-8601 timestamp.
    pub created_at: String,
    /// JSON-serialized request body (up to 4 KB).
    pub request_body: Option<String>,
    /// Explicit intent from X-Intent header.
    pub intent: Option<String>,
    /// Cedar policy IDs that contributed to the authorization decision (JSON array).
    pub matched_policy_ids: Option<Vec<String>>,
    /// Monotonic capture order stamped by the process that recorded the row.
    ///
    /// Null on rows written before the column existed; see
    /// [`crate::schema::ALTER_TRAJECTORIES_ADD_CAPTURE_SEQ`].
    pub capture_seq: Option<i64>,
}

/// Aggregated trajectory statistics.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TrajectoryStats {
    /// Total trajectory count.
    pub total: u64,
    /// Number of successful actions.
    pub success_count: u64,
    /// Number of failed actions.
    pub error_count: u64,
    /// Success rate (0.0 - 1.0).
    pub success_rate: f64,
    /// Per-action breakdown.
    pub by_action: std::collections::BTreeMap<String, ActionStats>,
    /// Recent failed intents.
    pub failed_intents: Vec<TursoTrajectoryRow>,
}

/// Per-action statistics.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ActionStats {
    /// Total actions.
    pub total: u64,
    /// Successful actions.
    pub success: u64,
    /// Failed actions.
    pub error: u64,
}

/// Agent summary aggregated from trajectories.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentSummary {
    /// Agent identifier.
    pub agent_id: String,
    /// Total actions performed by this agent.
    pub total_actions: u64,
    /// Successful actions.
    pub success_count: u64,
    /// Failed actions.
    pub error_count: u64,
    /// Authorization denials.
    pub denial_count: u64,
    /// Success rate (0.0 - 1.0).
    pub success_rate: f64,
    /// Most recent activity timestamp.
    pub last_active_at: String,
}

/// A pre-aggregated row from the unmet-intents SQL GROUP BY query.
///
/// Used by the `/observe/evolution/unmet-intents` endpoint to avoid loading
/// thousands of raw trajectory rows into memory on every poll.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UnmetIntentAggRow {
    /// Entity type that produced failures.
    pub entity_type: String,
    /// Most-recent action name that failed (representative sample).
    pub action: String,
    /// Raw error string from the trajectory row (may be None).
    pub error: Option<String>,
    /// Number of failures in this group.
    pub count: u64,
    /// ISO-8601 timestamp of the oldest failure in the group.
    pub first_seen: String,
    /// ISO-8601 timestamp of the most-recent failure in the group.
    pub last_seen: String,
}

/// Row returned by feature request queries.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FeatureRequestRow {
    /// Feature request ID.
    pub id: String,
    /// Tenant that owns the feature request.
    pub tenant: String,
    /// Category label.
    pub category: String,
    /// Description of the feature request.
    pub description: String,
    /// Number of trajectory references.
    pub frequency: i64,
    /// JSON array of trajectory reference IDs.
    pub trajectory_refs: String,
    /// Disposition: Open, Acknowledged, Planned, WontFix, Resolved.
    pub disposition: String,
    /// Developer notes.
    pub developer_notes: Option<String>,
    /// ISO-8601 created timestamp.
    pub created_at: String,
    /// ISO-8601 updated timestamp.
    pub updated_at: String,
}

/// Row returned by evolution record queries.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EvolutionRecordRow {
    /// Record ID.
    pub id: String,
    /// Tenant that owns the record chain.
    pub tenant: String,
    /// Record type: Observation, Problem, Analysis, Decision, Insight.
    pub record_type: String,
    /// Status: Open, Resolved, Superseded, Rejected.
    pub status: String,
    /// Creator identity.
    pub created_by: String,
    /// ID of the parent record this was derived from.
    pub derived_from: Option<String>,
    /// Full record data as JSON.
    pub data: String,
    /// ISO-8601 timestamp.
    pub timestamp: String,
}

/// Row returned by design-time event queries.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DesignTimeEventRow {
    /// Auto-increment ID.
    pub id: i64,
    /// Event kind.
    pub kind: String,
    /// Entity type.
    pub entity_type: String,
    /// Tenant.
    pub tenant: String,
    /// Human-readable summary.
    pub summary: String,
    /// Verification level name.
    pub level: Option<String>,
    /// Whether this level passed.
    pub passed: Option<bool>,
    /// Step number in the workflow.
    pub step_number: Option<i64>,
    /// Total steps in the workflow.
    pub total_steps: Option<i64>,
    /// ISO-8601 timestamp.
    pub created_at: String,
}

/// Row returned by WASM module queries.
#[derive(Debug, Clone)]
pub struct TursoWasmModuleRow {
    /// Tenant name.
    pub tenant: String,
    /// Module name.
    pub module_name: String,
    /// Raw WASM binary.
    pub wasm_bytes: Vec<u8>,
    /// SHA-256 hash of the WASM binary.
    pub sha256_hash: String,
    /// Monotonic version counter.
    pub version: i32,
    /// Module size in bytes.
    pub size_bytes: i32,
    /// ISO-8601 updated_at timestamp.
    pub updated_at: String,
    /// Provenance: `"bundled"` (install pipeline) or `"upload"` (hot upload).
    pub source: String,
}

/// Metadata row returned by startup WASM registry restore queries.
#[derive(Debug, Clone)]
pub struct TursoWasmModuleMetadataRow {
    /// Tenant name.
    pub tenant: String,
    /// Module name.
    pub module_name: String,
    /// SHA-256 hash of the WASM binary.
    pub sha256_hash: String,
    /// Module size in bytes.
    pub size_bytes: i32,
    /// ISO-8601 updated_at timestamp.
    pub updated_at: String,
}

/// Row returned by WASM invocation log queries.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TursoWasmInvocationRow {
    /// Tenant name.
    pub tenant: String,
    /// Entity type that triggered the invocation.
    pub entity_type: String,
    /// Entity ID that triggered the invocation.
    pub entity_id: String,
    /// WASM module name invoked.
    pub module_name: String,
    /// Action that triggered the integration.
    pub trigger_action: String,
    /// Callback action dispatched (if any).
    pub callback_action: Option<String>,
    /// Whether the invocation succeeded.
    pub success: bool,
    /// Error description (for failures).
    pub error: Option<String>,
    /// Invocation duration in milliseconds.
    pub duration_ms: u64,
    /// ISO-8601 timestamp.
    pub created_at: String,
}

/// Row returned by [`TursoEventStore::load_tenant_constraints()`].
#[derive(Debug, Clone)]
pub struct TursoTenantConstraintRow {
    /// Tenant name.
    pub tenant: String,
    /// Raw `cross-invariants.toml` source.
    pub cross_invariants_toml: String,
    /// Monotonic version counter.
    pub version: i32,
    /// ISO-8601 updated_at timestamp.
    pub updated_at: String,
}
