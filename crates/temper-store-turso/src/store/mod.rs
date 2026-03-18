//! Turso/libSQL-backed implementation of the [`EventStore`] trait.
//!
//! Split into domain-focused sub-modules for cohesion:
//! - [`specs`]: Spec CRUD (upsert, verification, load)
//! - [`trajectory`]: Trajectory persistence and queries
//! - [`evolution`]: Feature requests, evolution records, design-time events
//! - [`authz`]: Authorization decisions and Cedar policies
//! - [`wasm`]: WASM module storage and invocation logs
//! - [`constraints`]: Tenant-level cross-entity constraints
//! - [`event_store`]: [`EventStore`] trait implementation

use libsql::{Builder, Database};
use std::sync::Arc;
use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::instrument;

use crate::schema;

mod authz;
mod constraints;
mod event_store;
mod evolution;
mod instrumentation;
mod policy;
mod secrets;
mod specs;
#[cfg(test)]
mod tests;
mod trajectory;
mod wasm;

use instrumentation::InstrumentedConnection;

#[derive(Clone, Debug)]
pub struct TursoEventStore {
    db: Arc<Database>,
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
        conn.execute(schema::CREATE_EVENTS_ENTITY_INDEX, ())
            .await
            .map_err(storage_error)?;
        conn.execute(schema::CREATE_SNAPSHOTS_TABLE, ())
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
        // Migration: add `enabled` column to existing `policies` tables.
        let _ = conn.execute(schema::ALTER_POLICIES_ADD_ENABLED, ()).await;
        conn.execute(schema::CREATE_TENANT_INSTALLED_APPS_TABLE, ())
            .await
            .map_err(storage_error)?;

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
        ] {
            let _ = conn.execute(stmt, ()).await; // ignore "duplicate column" errors
        }
        conn.execute(schema::CREATE_TRAJECTORIES_AGENT_INDEX, ())
            .await
            .map_err(storage_error)?;

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
