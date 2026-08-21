//! SQLite-compatible schema for the Turso/libSQL event store.

mod query_plane;
pub(crate) mod schema_deployment;

pub use crate::schema_event_history::{
    ALTER_EVENTS_ADD_SEGMENT_INDEX, CREATE_EVENT_SEGMENTS_OPEN_INDEX, CREATE_EVENT_SEGMENTS_TABLE,
    CREATE_SNAPSHOT_HISTORY_ENTITY_INDEX, CREATE_SNAPSHOT_HISTORY_TABLE,
};
pub use query_plane::{
    CREATE_ENTITY_CATALOG_STATUS_INDEX, CREATE_ENTITY_CATALOG_TABLE,
    CREATE_ENTITY_CATALOG_TYPE_INDEX, CREATE_ENTITY_FIELD_INDEX_LOOKUP,
    CREATE_ENTITY_FIELD_INDEX_STATUS, CREATE_ENTITY_FIELD_INDEX_TABLE,
    CREATE_ENTITY_KEY_INDEX_ENTITY, CREATE_ENTITY_KEY_INDEX_TABLE,
    CREATE_ENTITY_VECTOR_INDEX_ENTITY, CREATE_ENTITY_VECTOR_INDEX_PARTITION,
    CREATE_ENTITY_VECTOR_INDEX_TABLE, CREATE_VECTOR_INDEX_BACKFILL_WATERMARK,
};

pub const CREATE_EVENTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    sequence_nr INTEGER NOT NULL,
    segment_index INTEGER NOT NULL DEFAULT 0,
    event_type TEXT NOT NULL,
    payload TEXT NOT NULL,
    metadata TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(tenant, entity_type, entity_id, sequence_nr)
);";

pub const CREATE_EVENTS_ENTITY_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_events_entity
    ON events(tenant, entity_type, entity_id, sequence_nr);";

pub const CREATE_SNAPSHOTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS snapshots (
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    sequence_nr INTEGER NOT NULL,
    snapshot BLOB NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY(tenant, entity_type, entity_id)
);";

pub const CREATE_SPECS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS specs (
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    ioa_source TEXT NOT NULL,
    csdl_xml TEXT,
    version INTEGER NOT NULL DEFAULT 1,
    verified INTEGER NOT NULL DEFAULT 0,
    verification_status TEXT NOT NULL DEFAULT 'pending',
    levels_passed INTEGER,
    levels_total INTEGER,
    verification_result TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(tenant, entity_type)
);";

pub const CREATE_TRAJECTORIES_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS trajectories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    action TEXT NOT NULL,
    success INTEGER NOT NULL DEFAULT 0,
    from_status TEXT,
    to_status TEXT,
    error TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

pub const CREATE_TRAJECTORIES_SUCCESS_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_trajectories_success
    ON trajectories(success);";

pub const CREATE_TRAJECTORIES_ENTITY_ACTION_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_trajectories_entity_action
    ON trajectories(tenant, entity_type, action);";

pub const CREATE_TENANT_CONSTRAINTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS tenant_constraints (
    tenant TEXT NOT NULL PRIMARY KEY,
    cross_invariants_toml TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

/// CREATE TABLE statement for WASM module storage.
///
/// Stores compiled WASM binaries for agent-generated integration handlers.
/// Keyed by (tenant, module_name) with version tracking and SHA-256 integrity.
pub const CREATE_WASM_MODULES_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS wasm_modules (
    tenant TEXT NOT NULL,
    module_name TEXT NOT NULL,
    wasm_bytes BLOB NOT NULL,
    sha256_hash TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 1,
    size_bytes INTEGER NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    source TEXT NOT NULL DEFAULT 'bundled',
    UNIQUE(tenant, module_name)
);";

/// ALTER for existing Turso DBs: add the `source` column. Idempotent via the
/// IF NOT EXISTS guard. See `0002_wasm_modules_source.sql` for the Postgres
/// equivalent.
pub const ADD_WASM_MODULES_SOURCE_COLUMN: &str = "\
ALTER TABLE wasm_modules ADD COLUMN source TEXT NOT NULL DEFAULT 'bundled';";

/// CREATE TABLE statement for WASM invocation logs.
///
/// Records every WASM integration invocation for observability and
/// persistence across server restarts.
pub const CREATE_WASM_INVOCATION_LOGS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS wasm_invocation_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    module_name TEXT NOT NULL,
    trigger_action TEXT NOT NULL,
    callback_action TEXT,
    success INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    duration_ms INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

/// CREATE INDEX for filtering invocation logs by tenant.
pub const CREATE_WASM_INVOCATION_LOGS_TENANT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_wasm_invocation_logs_tenant
    ON wasm_invocation_logs(tenant);";

/// CREATE INDEX for filtering invocation logs by module name.
pub const CREATE_WASM_INVOCATION_LOGS_MODULE_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_wasm_invocation_logs_module
    ON wasm_invocation_logs(module_name);";

/// CREATE INDEX for ordering invocation logs by creation time (newest first).
pub const CREATE_WASM_INVOCATION_LOGS_CREATED_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_wasm_invocation_logs_created
    ON wasm_invocation_logs(created_at DESC);";

/// CREATE TABLE statement for pending authorization decisions.
///
/// Stores Cedar authorization denials awaiting human approval.
/// The full PendingDecision is stored as JSON in the `data` column.
pub const CREATE_PENDING_DECISIONS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS pending_decisions (
    id TEXT PRIMARY KEY,
    tenant TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    data TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

pub const CREATE_PENDING_DECISIONS_TENANT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_pending_decisions_tenant
    ON pending_decisions(tenant);";

pub const CREATE_PENDING_DECISIONS_STATUS_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_pending_decisions_status
    ON pending_decisions(status);";

/// Cedar policy storage per tenant.
pub const CREATE_TENANT_POLICIES_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS tenant_policies (
    tenant TEXT PRIMARY KEY,
    policy_text TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

/// Granular Cedar policy storage with per-policy tracking and hash-based change detection.
///
/// Replaces the flat `tenant_policies` table for new write paths.
/// Multiple policy entries per tenant are supported (e.g. one per approved decision
/// or one manually-managed "primary" entry).  At boot, all enabled rows for a tenant
/// are concatenated to reconstruct the effective policy set.
pub const CREATE_POLICIES_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS policies (
    tenant TEXT NOT NULL,
    policy_id TEXT NOT NULL,
    cedar_text TEXT NOT NULL,
    policy_hash TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    created_by TEXT NOT NULL DEFAULT 'system',
    enabled INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY(tenant, policy_id)
);";

/// Migration: add `enabled` column to existing `policies` tables.
/// SQLite returns an error if the column already exists — callers should ignore failures.
pub const ALTER_POLICIES_ADD_ENABLED: &str =
    "ALTER TABLE policies ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1";

/// Migration: add `projection_hash` to the durable query-plane catalog.
pub const ALTER_ENTITY_CATALOG_ADD_PROJECTION_HASH: &str =
    "ALTER TABLE entity_catalog ADD COLUMN projection_hash TEXT NOT NULL DEFAULT ''";

/// Migration: add full projected fields JSON to the durable query-plane catalog.
pub const ALTER_ENTITY_CATALOG_ADD_FIELDS: &str =
    "ALTER TABLE entity_catalog ADD COLUMN fields TEXT NOT NULL DEFAULT '{}'";

/// Migration: add full projected response state JSON to the durable query-plane catalog.
pub const ALTER_ENTITY_CATALOG_ADD_STATE: &str = "ALTER TABLE entity_catalog ADD COLUMN state TEXT";

/// Durable per-tenant authorization denial patterns used to reconstruct
/// policy suggestions across process restarts.
pub const CREATE_POLICY_DENIAL_PATTERNS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS policy_denial_patterns (
    tenant TEXT NOT NULL,
    agent_type TEXT NOT NULL DEFAULT '',
    action TEXT NOT NULL,
    resource_type TEXT NOT NULL,
    count INTEGER NOT NULL DEFAULT 0,
    first_seen TEXT NOT NULL,
    last_seen TEXT NOT NULL,
    distinct_resource_ids_json TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (tenant, agent_type, action, resource_type)
);";

pub const CREATE_POLICY_DENIAL_PATTERNS_TENANT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_policy_denial_patterns_tenant
    ON policy_denial_patterns(tenant, last_seen DESC);";

/// Immutable public artifact records derived from governed TemperFS files.
///
/// The source File/FileVersion and event log remain the authority; this table is
/// a rebuildable read model for public delivery URLs.
pub const CREATE_PUBLISHED_ARTIFACTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS published_artifacts (
    id TEXT PRIMARY KEY,
    tenant TEXT NOT NULL,
    source_file_id TEXT NOT NULL,
    source_file_version_id TEXT NOT NULL DEFAULT '',
    content_hash TEXT NOT NULL,
    label TEXT NOT NULL,
    mime_type TEXT NOT NULL,
    byte_length INTEGER NOT NULL,
    public_storage_key TEXT NOT NULL,
    public_url TEXT NOT NULL,
    owner_ref_type TEXT NOT NULL DEFAULT '',
    owner_ref_id TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'published',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(tenant, label, owner_ref_type, owner_ref_id, content_hash)
);";

pub const CREATE_PUBLISHED_ARTIFACTS_OWNER_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_published_artifacts_owner
    ON published_artifacts(tenant, owner_ref_type, owner_ref_id, label, status);";

pub const CREATE_PUBLISHED_ARTIFACTS_SOURCE_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_published_artifacts_source
    ON published_artifacts(tenant, source_file_id, status);";

mod installed_apps;
pub use installed_apps::*;

// ---------------------------------------------------------------------------
// Phase 0: Turso as single source of truth — new tables + trajectory extensions
// ---------------------------------------------------------------------------

/// ALTER TABLE migration: add content_hash to specs table.
pub const ALTER_SPECS_ADD_CONTENT_HASH: &str = "ALTER TABLE specs ADD COLUMN content_hash TEXT";

/// ALTER TABLE migration: add committed flag to specs table (WAL-style commit pattern).
pub const ALTER_SPECS_ADD_COMMITTED: &str =
    "ALTER TABLE specs ADD COLUMN committed INTEGER NOT NULL DEFAULT 1";

/// ALTER TABLE migrations for the `trajectories` table.
///
/// These add columns that were previously only tracked in-memory
/// (agent_id, session_id, authz_denied, etc.). Each statement uses
/// try-and-ignore semantics in SQLite (duplicate column is a no-op error).
pub const ALTER_TRAJECTORIES_ADD_AGENT_ID: &str =
    "ALTER TABLE trajectories ADD COLUMN agent_id TEXT";
pub const ALTER_TRAJECTORIES_ADD_SESSION_ID: &str =
    "ALTER TABLE trajectories ADD COLUMN session_id TEXT";
pub const ALTER_TRAJECTORIES_ADD_AUTHZ_DENIED: &str =
    "ALTER TABLE trajectories ADD COLUMN authz_denied INTEGER";
pub const ALTER_TRAJECTORIES_ADD_DENIED_RESOURCE: &str =
    "ALTER TABLE trajectories ADD COLUMN denied_resource TEXT";
pub const ALTER_TRAJECTORIES_ADD_DENIED_MODULE: &str =
    "ALTER TABLE trajectories ADD COLUMN denied_module TEXT";
pub const ALTER_TRAJECTORIES_ADD_SOURCE: &str = "ALTER TABLE trajectories ADD COLUMN source TEXT";
pub const ALTER_TRAJECTORIES_ADD_SPEC_GOVERNED: &str =
    "ALTER TABLE trajectories ADD COLUMN spec_governed INTEGER";
pub const ALTER_TRAJECTORIES_ADD_REQUEST_BODY: &str =
    "ALTER TABLE trajectories ADD COLUMN request_body TEXT";
pub const ALTER_TRAJECTORIES_ADD_INTENT: &str = "ALTER TABLE trajectories ADD COLUMN intent TEXT";
pub const ALTER_TRAJECTORIES_ADD_MATCHED_POLICY_IDS: &str =
    "ALTER TABLE trajectories ADD COLUMN matched_policy_ids TEXT";
/// Capture order within the writing process.
///
/// Rows are written by independently spawned persistence tasks, so the
/// autoincrement `id` records the order the writes *landed*, not the order the
/// kernel *captured* them. The conformance checker replays a session as a
/// state-machine walk and needs capture order, so the capturing process stamps
/// a monotonic sequence on the entry before it is queued and the read orders
/// by it. Null on rows written before this column existed.
pub const ALTER_TRAJECTORIES_ADD_CAPTURE_SEQ: &str =
    "ALTER TABLE trajectories ADD COLUMN capture_seq INTEGER";

/// Index on agent_id for agent-scoped trajectory queries.
pub const CREATE_TRAJECTORIES_AGENT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_trajectories_agent
    ON trajectories(agent_id);";

/// Index on session_id for session-scoped trajectory replay.
///
/// Conformance checking reads one session's rows in capture order, so the
/// index covers every ordering column and the read is a range scan rather
/// than a table scan.
pub const CREATE_TRAJECTORIES_SESSION_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_trajectories_session_capture
    ON trajectories(session_id, created_at, capture_seq, id);";

/// Feature request records generated from trajectory analysis.
pub const CREATE_FEATURE_REQUESTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS feature_requests (
    id TEXT PRIMARY KEY,
    tenant TEXT NOT NULL DEFAULT 'default',
    category TEXT NOT NULL,
    description TEXT NOT NULL,
    frequency INTEGER NOT NULL DEFAULT 0,
    trajectory_refs TEXT NOT NULL DEFAULT '[]',
    disposition TEXT NOT NULL DEFAULT 'Open',
    developer_notes TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

/// Evolution record chain (O/P/A/D/I records).
pub const CREATE_EVOLUTION_RECORDS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS evolution_records (
    id TEXT PRIMARY KEY,
    tenant TEXT NOT NULL DEFAULT 'default',
    record_type TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'Open',
    created_by TEXT NOT NULL,
    derived_from TEXT,
    data TEXT NOT NULL,
    timestamp TEXT NOT NULL DEFAULT (datetime('now'))
);";

/// Idempotent-at-runner migration for feature-request tenant ownership.
pub const ALTER_FEATURE_REQUESTS_ADD_TENANT: &str =
    "ALTER TABLE feature_requests ADD COLUMN tenant TEXT NOT NULL DEFAULT 'default'";

/// Idempotent-at-runner migration for evolution-record tenant ownership.
pub const ALTER_EVOLUTION_RECORDS_ADD_TENANT: &str =
    "ALTER TABLE evolution_records ADD COLUMN tenant TEXT NOT NULL DEFAULT 'default'";

pub const CREATE_FEATURE_REQUESTS_TENANT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_feature_requests_tenant_disposition
    ON feature_requests(tenant, disposition, frequency DESC, created_at DESC);";

pub const CREATE_EVOLUTION_RECORDS_TENANT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_evolution_records_tenant_type_status
    ON evolution_records(tenant, record_type, status, timestamp DESC);";

pub const CREATE_EVOLUTION_RECORDS_TENANT_PARENT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_evolution_records_tenant_parent
    ON evolution_records(tenant, derived_from);";

pub const CREATE_EVOLUTION_RECORDS_TYPE_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_evolution_records_type
    ON evolution_records(record_type);";

pub const CREATE_EVOLUTION_RECORDS_STATUS_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_evolution_records_status
    ON evolution_records(status);";

/// Design-time events emitted during spec loading and verification.
pub const CREATE_DESIGN_TIME_EVENTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS design_time_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    tenant TEXT NOT NULL,
    summary TEXT NOT NULL,
    level TEXT,
    passed INTEGER,
    step_number INTEGER,
    total_steps INTEGER,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

pub const CREATE_DESIGN_TIME_EVENTS_TENANT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_design_time_events_tenant
    ON design_time_events(tenant, entity_type);";

// ---------------------------------------------------------------------------
// Platform DB tables (tenant registry + user access)
// ---------------------------------------------------------------------------

/// Registry of provisioned tenant databases.
pub const CREATE_TENANT_REGISTRY_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS tenant_registry (
    tenant_id TEXT PRIMARY KEY,
    turso_db_url TEXT NOT NULL,
    turso_auth_token TEXT,
    status TEXT NOT NULL DEFAULT 'active',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

/// User-to-tenant access mappings.
pub const CREATE_TENANT_USERS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS tenant_users (
    tenant_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    role TEXT NOT NULL DEFAULT 'member',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY(tenant_id, user_id)
);";

/// Index for looking up tenants by user.
pub const CREATE_TENANT_USERS_USER_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_tenant_users_user
    ON tenant_users(user_id);";

/// Per-tenant encrypted secret storage.
///
/// Mirrors the Postgres `tenant_secrets` table. Ciphertext and nonce are stored
/// as BLOBs (AES-256-GCM encrypted by [`SecretsVault`]).  Secrets are always
/// stored in the per-tenant database for proper isolation.
pub const CREATE_TENANT_SECRETS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS tenant_secrets (
    tenant TEXT NOT NULL,
    key_name TEXT NOT NULL,
    ciphertext BLOB NOT NULL,
    nonce BLOB NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY(tenant, key_name)
);";

// ---------------------------------------------------------------------------
// Blob storage (content-addressed binary objects for TemperFS)
// ---------------------------------------------------------------------------

/// Content-addressed blob storage for TemperFS `$value` endpoints and
/// field-overflow blob refs (ADR-0040).
///
/// Blobs are keyed by `{bucket}/{content_hash}` (e.g. `temper-fs/sha256:abc...`).
/// This provides persistent local blob storage so the blob_adapter WASM module
/// can upload/download via HTTP without requiring external S3/R2 in development.
///
/// `expires_at` is `NULL` by default (permanent). Callers opt specific rows
/// into TTL via `put_blob_with_ttl`; `sweep_expired_blobs` deletes expired rows.
/// See ADR-0047.
pub const CREATE_BLOBS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS blobs (
    blob_key TEXT PRIMARY KEY,
    data BLOB NOT NULL,
    size_bytes INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    expires_at TEXT
);";

/// Migration: add `expires_at` column to existing `blobs` tables that pre-date
/// ADR-0047. Idempotent — the caller treats "duplicate column" errors as
/// success so the migration runs safely on every startup.
pub const ALTER_BLOBS_ADD_EXPIRES_AT: &str = "\
ALTER TABLE blobs ADD COLUMN expires_at TEXT;";

/// Partial index on `expires_at` so the sweeper query (`WHERE expires_at < now`)
/// stays cheap without costing storage for the default (permanent) rows. The
/// `WHERE` clause excludes NULL, making the index a small-to-moderate overlay.
pub const CREATE_BLOBS_EXPIRES_AT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_blobs_expires_at ON blobs(expires_at) WHERE expires_at IS NOT NULL;";

mod ots;
pub use ots::*;

#[cfg(test)]
#[path = "schema_test.rs"]
mod schema_test;
