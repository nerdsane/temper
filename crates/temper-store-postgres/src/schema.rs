//! SQL schema definitions for the PostgreSQL persistence store.
//!
//! These constants define the DDL statements used to create the `events` and
//! `snapshots` tables that back the event-sourced persistence layer.

pub use crate::schema_event_history::*;

/// CREATE TABLE statement for the events journal.
///
/// Each row stores a single domain event for a particular entity. The
/// `(tenant, entity_type, entity_id, sequence_nr)` UNIQUE constraint enforces
/// optimistic concurrency — two writers attempting to persist the same
/// sequence number will conflict, and only one will succeed.
///
/// The `tenant` column defaults to `'default'` for backward compatibility
/// with single-tenant deployments.
pub const CREATE_EVENTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS events (
    id            BIGSERIAL    NOT NULL,
    tenant        TEXT         NOT NULL DEFAULT 'default',
    entity_type   TEXT         NOT NULL,
    entity_id     TEXT         NOT NULL,
    sequence_nr   BIGINT       NOT NULL,
    segment_index BIGINT       NOT NULL DEFAULT 0,
    event_type    TEXT         NOT NULL,
    payload       JSONB        NOT NULL,
    metadata      JSONB        NOT NULL,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (id),
    UNIQUE (tenant, entity_type, entity_id, sequence_nr)
);";

/// CREATE TABLE statement for the snapshots table.
///
/// Snapshots store the serialised state of an entity at a given sequence
/// number. Only the latest snapshot per entity is kept — the UPSERT in
/// `save_snapshot` replaces older rows via the composite primary key.
pub const CREATE_SNAPSHOTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS snapshots (
    tenant        TEXT         NOT NULL DEFAULT 'default',
    entity_type   TEXT         NOT NULL,
    entity_id     TEXT         NOT NULL,
    sequence_nr   BIGINT       NOT NULL,
    state         BYTEA        NOT NULL,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, entity_type, entity_id)
);";

/// CREATE INDEX statement for efficient `list_entity_ids` scans.
///
/// This covering index supports filtering by `tenant` and returning distinct
/// `(entity_type, entity_id)` pairs from the events journal.
pub const CREATE_ENTITY_LISTING_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_events_tenant_entity
    ON events (tenant, entity_type, entity_id);";

/// CREATE TABLE statement for persisted specs and verification status.
pub const CREATE_SPECS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS specs (
    id                  BIGSERIAL    PRIMARY KEY,
    tenant              TEXT         NOT NULL,
    entity_type         TEXT         NOT NULL,
    ioa_source          TEXT         NOT NULL,
    csdl_xml            TEXT,
    version             INT          NOT NULL DEFAULT 1,
    verified            BOOLEAN      NOT NULL DEFAULT false,
    verification_status TEXT         NOT NULL DEFAULT 'pending',
    levels_passed       INT,
    levels_total        INT,
    verification_result JSONB,
    content_hash        TEXT         NOT NULL DEFAULT '',
    committed           BOOLEAN      NOT NULL DEFAULT true,
    created_at          TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ  NOT NULL DEFAULT now(),
    UNIQUE (tenant, entity_type)
);";

/// Add content hash to existing `specs` tables created before ADR-0065.
pub const ALTER_SPECS_ADD_CONTENT_HASH: &str =
    "ALTER TABLE specs ADD COLUMN IF NOT EXISTS content_hash TEXT NOT NULL DEFAULT '';";

/// Add commit flag to existing `specs` tables created before ADR-0065.
pub const ALTER_SPECS_ADD_COMMITTED: &str =
    "ALTER TABLE specs ADD COLUMN IF NOT EXISTS committed BOOLEAN NOT NULL DEFAULT true;";

/// CREATE TABLE statement for persisted trajectory action outcomes.
pub const CREATE_TRAJECTORIES_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS trajectories (
    id            BIGSERIAL    PRIMARY KEY,
    tenant        TEXT         NOT NULL,
    entity_type   TEXT         NOT NULL,
    entity_id     TEXT         NOT NULL DEFAULT '',
    action        TEXT         NOT NULL,
    success       BOOLEAN      NOT NULL,
    from_status   TEXT,
    to_status     TEXT,
    error         TEXT,
    agent_id      TEXT,
    session_id    TEXT,
    authz_denied  BOOLEAN,
    denied_resource TEXT,
    denied_module TEXT,
    source        TEXT,
    spec_governed BOOLEAN,
    request_body  JSONB,
    intent        TEXT,
    matched_policy_ids JSONB,
    capture_seq   BIGINT,
    agent_type    TEXT,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now()
);";

/// CREATE INDEX statement for trajectory success filtering.
pub const CREATE_TRAJECTORIES_SUCCESS_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_trajectories_success ON trajectories (success, created_at DESC);";

/// CREATE INDEX statement for trajectory action/entity grouping.
pub const CREATE_TRAJECTORIES_ENTITY_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_trajectories_entity ON trajectories (entity_type, action);";

/// CREATE INDEX statement for session-scoped trajectory replay.
///
/// Conformance checking reads one session's rows in write order, so the index
/// covers the ordering columns and the read stays a range scan.
pub const CREATE_TRAJECTORIES_SESSION_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_trajectories_session ON trajectories (session_id, created_at, id);";

/// CREATE TABLE statement for persisted design-time workflow events.
pub const CREATE_DESIGN_TIME_EVENTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS design_time_events (
    id            BIGSERIAL    PRIMARY KEY,
    kind          TEXT         NOT NULL,
    entity_type   TEXT         NOT NULL,
    tenant        TEXT         NOT NULL,
    summary       TEXT         NOT NULL,
    level         TEXT,
    passed        BOOLEAN,
    step_number   SMALLINT,
    total_steps   SMALLINT,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now()
);";

/// CREATE INDEX statement for tenant-scoped design-time history queries.
pub const CREATE_DESIGN_TIME_EVENTS_TENANT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_dt_events_tenant ON design_time_events (tenant, created_at DESC);";

/// CREATE TABLE statement for tenant-level cross-entity constraint definitions.
pub const CREATE_TENANT_CONSTRAINTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS tenant_constraints (
    tenant                TEXT         NOT NULL PRIMARY KEY,
    cross_invariants_toml TEXT         NOT NULL,
    version               INT          NOT NULL DEFAULT 1,
    updated_at            TIMESTAMPTZ  NOT NULL DEFAULT now()
);";

/// CREATE TABLE statement for WASM module storage.
///
/// Stores compiled WASM binaries for agent-generated integration handlers.
/// Keyed by (tenant, module_name) with version tracking and SHA-256 integrity.
pub const CREATE_WASM_MODULES_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS wasm_modules (
    tenant        TEXT         NOT NULL,
    module_name   TEXT         NOT NULL,
    wasm_bytes    BYTEA        NOT NULL,
    sha256_hash   TEXT         NOT NULL,
    version       INT          NOT NULL DEFAULT 1,
    size_bytes    INT          NOT NULL,
    updated_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    source        TEXT         NOT NULL DEFAULT 'bundled',
    UNIQUE (tenant, module_name)
);";

/// Idempotent migration to add the `source` column to existing
/// deployments. `'bundled'` rows came from the os-apps install pipeline
/// (on-disk bytes baked into the deploy image); `'upload'` rows came
/// from a hot upload via `POST /api/wasm/modules/{name}` and must
/// survive subsequent restarts (the boot install pipeline checks this
/// flag before overwriting).
pub const ADD_WASM_MODULES_SOURCE_COLUMN: &str = "\
ALTER TABLE wasm_modules \
    ADD COLUMN IF NOT EXISTS source TEXT NOT NULL DEFAULT 'bundled';";

/// CREATE TABLE statement for WASM invocation logs.
///
/// Records every WASM integration invocation for observability and
/// persistence across server restarts. Keyed by auto-increment ID.
pub const CREATE_WASM_INVOCATION_LOGS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS wasm_invocation_logs (
    id              BIGSERIAL    PRIMARY KEY,
    tenant          TEXT         NOT NULL,
    entity_type     TEXT         NOT NULL,
    entity_id       TEXT         NOT NULL,
    module_name     TEXT         NOT NULL,
    trigger_action  TEXT         NOT NULL,
    callback_action TEXT,
    success         BOOLEAN      NOT NULL,
    error           TEXT,
    duration_ms     BIGINT       NOT NULL,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT now()
);";

/// CREATE INDEX for filtering invocation logs by tenant.
pub const CREATE_WASM_INVOCATION_LOGS_TENANT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_wasm_invocation_logs_tenant
    ON wasm_invocation_logs (tenant);";

/// CREATE INDEX for filtering invocation logs by module name.
pub const CREATE_WASM_INVOCATION_LOGS_MODULE_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_wasm_invocation_logs_module
    ON wasm_invocation_logs (module_name);";

/// CREATE INDEX for ordering invocation logs by creation time (newest first).
pub const CREATE_WASM_INVOCATION_LOGS_CREATED_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_wasm_invocation_logs_created
    ON wasm_invocation_logs (created_at DESC);";

/// CREATE TABLE statement for per-tenant encrypted secrets.
///
/// Stores AES-256-GCM encrypted secret values keyed by (tenant, key_name).
/// The master key is held in memory only; ciphertext + nonce are persisted.
pub const CREATE_TENANT_SECRETS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS tenant_secrets (
    tenant      TEXT         NOT NULL,
    key_name    TEXT         NOT NULL,
    ciphertext  BYTEA        NOT NULL,
    nonce       BYTEA        NOT NULL,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, key_name)
);";

/// CREATE TABLE statement for pending authorization decisions.
pub const CREATE_PENDING_DECISIONS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS pending_decisions (
    id         TEXT         PRIMARY KEY,
    tenant     TEXT         NOT NULL,
    status     TEXT         NOT NULL DEFAULT 'pending',
    data       JSONB        NOT NULL,
    created_at TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ  NOT NULL DEFAULT now()
);";

/// CREATE INDEX for tenant-scoped decision reads.
pub const CREATE_PENDING_DECISIONS_TENANT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_pending_decisions_tenant
    ON pending_decisions (tenant);";

/// CREATE INDEX for decision status filtering.
pub const CREATE_PENDING_DECISIONS_STATUS_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_pending_decisions_status
    ON pending_decisions (status);";

/// Legacy per-tenant Cedar policy blob.
pub const CREATE_TENANT_POLICIES_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS tenant_policies (
    tenant      TEXT         PRIMARY KEY,
    policy_text TEXT         NOT NULL,
    updated_at  TIMESTAMPTZ  NOT NULL DEFAULT now()
);";

/// Granular Cedar policy storage with one row per policy entry.
pub const CREATE_POLICIES_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS policies (
    tenant      TEXT         NOT NULL,
    policy_id   TEXT         NOT NULL,
    cedar_text  TEXT         NOT NULL,
    policy_hash TEXT         NOT NULL DEFAULT '',
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT now(),
    created_by  TEXT         NOT NULL DEFAULT 'system',
    enabled     BOOLEAN      NOT NULL DEFAULT true,
    PRIMARY KEY (tenant, policy_id)
);";

/// Durable authorization denial patterns for policy suggestion reconstruction.
pub const CREATE_POLICY_DENIAL_PATTERNS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS policy_denial_patterns (
    tenant                     TEXT         NOT NULL,
    agent_type                 TEXT         NOT NULL DEFAULT '',
    action                     TEXT         NOT NULL,
    resource_type              TEXT         NOT NULL,
    count                      BIGINT       NOT NULL DEFAULT 0,
    first_seen                 TIMESTAMPTZ  NOT NULL,
    last_seen                  TIMESTAMPTZ  NOT NULL,
    distinct_resource_ids_json JSONB        NOT NULL DEFAULT '[]'::jsonb,
    PRIMARY KEY (tenant, agent_type, action, resource_type)
);";

/// CREATE INDEX for newest denial patterns by tenant.
pub const CREATE_POLICY_DENIAL_PATTERNS_TENANT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_policy_denial_patterns_tenant
    ON policy_denial_patterns (tenant, last_seen DESC);";

/// Tracks installed OS apps per tenant.
pub const CREATE_TENANT_INSTALLED_APPS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS tenant_installed_apps (
    tenant              TEXT         NOT NULL,
    app_name            TEXT         NOT NULL,
    source_kind         TEXT         NOT NULL DEFAULT 'local',
    app_ref             TEXT         NOT NULL DEFAULT '',
    version_hash        TEXT         NOT NULL DEFAULT '',
    pinned_version_hash TEXT         NOT NULL DEFAULT '',
    current_version_hash TEXT        NOT NULL DEFAULT '',
    follow_policy       TEXT         NOT NULL DEFAULT 'pinned',
    closure_id          TEXT         NOT NULL DEFAULT '',
    registry_url        TEXT         NOT NULL DEFAULT '',
    registry_tenant     TEXT         NOT NULL DEFAULT '',
    dependency_lock_digest TEXT      NOT NULL DEFAULT '',
    app_version         TEXT         NOT NULL DEFAULT '',
    bundle_digest       TEXT         NOT NULL DEFAULT '',
    spec_digest         TEXT         NOT NULL DEFAULT '',
    policy_digest       TEXT         NOT NULL DEFAULT '',
    wasm_digest         TEXT         NOT NULL DEFAULT '',
    content_digest      TEXT         NOT NULL DEFAULT '',
    seed_digest         TEXT         NOT NULL DEFAULT '',
    installed_at        TIMESTAMPTZ  NOT NULL DEFAULT now(),
    last_reconciled_at  TIMESTAMPTZ,
    status              TEXT         NOT NULL DEFAULT 'installed',
    PRIMARY KEY (tenant, app_name)
);";

/// One row per live entity in the durable query plane.
pub const CREATE_ENTITY_CATALOG_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS entity_catalog (
    tenant             TEXT         NOT NULL,
    entity_type        TEXT         NOT NULL,
    entity_id          TEXT         NOT NULL,
    status             TEXT         NOT NULL,
    fields             JSONB        NOT NULL DEFAULT '{}'::jsonb,
    state              JSONB,
    updated_at         TIMESTAMPTZ  NOT NULL DEFAULT now(),
    sequence_nr        BIGINT       NOT NULL DEFAULT 0,
    projection_version INT          NOT NULL DEFAULT 2,
    projection_hash    TEXT         NOT NULL DEFAULT '',
    PRIMARY KEY (tenant, entity_type, entity_id)
);";

/// Fast path for collection lookups by tenant/entity type.
pub const CREATE_ENTITY_CATALOG_TYPE_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_entity_catalog_type
    ON entity_catalog (tenant, entity_type);";

/// Fast path for status-based collection filtering.
pub const CREATE_ENTITY_CATALOG_STATUS_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_entity_catalog_status
    ON entity_catalog (tenant, entity_type, status);";

/// GIN index for JSONB field containment queries.
pub const CREATE_ENTITY_CATALOG_FIELDS_GIN_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_entity_catalog_fields_gin
    ON entity_catalog USING GIN (fields);";

/// EAV field index for existing OData SQL push-down translator.
pub const CREATE_ENTITY_FIELD_INDEX_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS entity_field_index (
    tenant       TEXT NOT NULL,
    entity_type  TEXT NOT NULL,
    entity_id    TEXT NOT NULL,
    field_name   TEXT NOT NULL,
    field_value  TEXT,
    status       TEXT,
    PRIMARY KEY (tenant, entity_type, entity_id, field_name)
);";

/// Composite index for field-value lookups.
pub const CREATE_ENTITY_FIELD_INDEX_LOOKUP: &str = "\
CREATE INDEX IF NOT EXISTS idx_efi_lookup
    ON entity_field_index (tenant, entity_type, field_name, field_value);";

/// Index for status-based EAV filtering.
pub const CREATE_ENTITY_FIELD_INDEX_STATUS: &str = "\
CREATE INDEX IF NOT EXISTS idx_efi_status
    ON entity_field_index (tenant, entity_type, status);";

/// Feature request records generated from trajectory analysis.
pub const CREATE_FEATURE_REQUESTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS feature_requests (
    id              TEXT         PRIMARY KEY,
    tenant          TEXT         NOT NULL DEFAULT 'default',
    category        TEXT         NOT NULL,
    description     TEXT         NOT NULL,
    frequency       BIGINT       NOT NULL DEFAULT 0,
    trajectory_refs JSONB        NOT NULL DEFAULT '[]'::jsonb,
    disposition     TEXT         NOT NULL DEFAULT 'Open',
    developer_notes TEXT,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ  NOT NULL DEFAULT now()
);";

/// Evolution record chain table. Compatible with `temper-evolution`'s
/// `PostgresRecordStore` (`payload` JSONB column) while adding tenant for RLS.
pub const CREATE_EVOLUTION_RECORDS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS evolution_records (
    id           TEXT         PRIMARY KEY,
    tenant       TEXT         NOT NULL DEFAULT 'default',
    record_type  TEXT         NOT NULL,
    status       TEXT         NOT NULL DEFAULT 'Open',
    created_by   TEXT         NOT NULL,
    derived_from TEXT,
    timestamp    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    payload      JSONB        NOT NULL
);";

/// Add tenant to existing `evolution_records` tables.
pub const ALTER_EVOLUTION_RECORDS_ADD_TENANT: &str = "ALTER TABLE evolution_records ADD COLUMN IF NOT EXISTS tenant TEXT NOT NULL DEFAULT 'default';";

/// CREATE INDEX for evolution records by type/status.
pub const CREATE_EVOLUTION_RECORDS_TYPE_STATUS_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_evolution_records_type_status
    ON evolution_records (record_type, status);";

/// CREATE INDEX for derived evolution records.
pub const CREATE_EVOLUTION_RECORDS_DERIVED_FROM_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_evolution_records_derived_from
    ON evolution_records (derived_from);";

/// Full OTS trajectory storage for agent execution traces.
///
/// Keyed by `(tenant, trajectory_id)`. One database holds every tenant's rows
/// and the id is chosen by the uploading harness, so a global key lets one
/// tenant's upload land on an id another tenant already used — and the upsert
/// would rewrite that row's tenant along with its data. Migration 0015 rekeys
/// databases created before this.
pub const CREATE_OTS_TRAJECTORIES_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS ots_trajectories (
    trajectory_id TEXT         NOT NULL,
    tenant        TEXT         NOT NULL,
    agent_id      TEXT         NOT NULL,
    session_id    TEXT,
    outcome       TEXT         NOT NULL DEFAULT 'unknown',
    entity_type   TEXT,
    turn_count    BIGINT       NOT NULL DEFAULT 0,
    data          JSONB        NOT NULL,
    persistence_status TEXT    NOT NULL DEFAULT 'persisted',
    persist_attempts BIGINT    NOT NULL DEFAULT 0,
    last_error    TEXT,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    CONSTRAINT ots_trajectories_tenant_identity PRIMARY KEY (tenant, trajectory_id)
);";

/// Add durable outbox status to existing OTS trajectory tables.
pub const ALTER_OTS_TRAJECTORIES_ADD_PERSISTENCE_STATUS: &str = "\
ALTER TABLE ots_trajectories ADD COLUMN IF NOT EXISTS persistence_status TEXT NOT NULL DEFAULT 'persisted';";

/// Add durable outbox attempt count to existing OTS trajectory tables.
pub const ALTER_OTS_TRAJECTORIES_ADD_PERSIST_ATTEMPTS: &str = "\
ALTER TABLE ots_trajectories ADD COLUMN IF NOT EXISTS persist_attempts BIGINT NOT NULL DEFAULT 0;";

/// Add durable outbox error details to existing OTS trajectory tables.
pub const ALTER_OTS_TRAJECTORIES_ADD_LAST_ERROR: &str = "\
ALTER TABLE ots_trajectories ADD COLUMN IF NOT EXISTS last_error TEXT;";

/// Add durable outbox update timestamp to existing OTS trajectory tables.
pub const ALTER_OTS_TRAJECTORIES_ADD_UPDATED_AT: &str = "\
ALTER TABLE ots_trajectories ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ NOT NULL DEFAULT now();";

/// CREATE INDEX for OTS lookup by agent.
pub const CREATE_OTS_TRAJECTORIES_AGENT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_ots_trajectories_agent
    ON ots_trajectories (agent_id);";

/// CREATE INDEX for OTS lookup by tenant.
pub const CREATE_OTS_TRAJECTORIES_TENANT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_ots_trajectories_tenant
    ON ots_trajectories (tenant);";

/// CREATE INDEX for OTS lookup by outcome.
pub const CREATE_OTS_TRAJECTORIES_OUTCOME_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_ots_trajectories_outcome
    ON ots_trajectories (outcome);";

/// CREATE INDEX for OTS durable outbox recovery.
pub const CREATE_OTS_TRAJECTORIES_STATUS_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_ots_trajectories_status
    ON ots_trajectories (persistence_status, updated_at);";

/// Content-addressed blob storage for development/local deployments.
pub const CREATE_BLOBS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS blobs (
    blob_key   TEXT         PRIMARY KEY,
    data       BYTEA        NOT NULL,
    size_bytes BIGINT       NOT NULL,
    created_at TIMESTAMPTZ  NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ
);";

/// Partial index for blob TTL sweeps.
pub const CREATE_BLOBS_EXPIRES_AT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_blobs_expires_at
    ON blobs (expires_at) WHERE expires_at IS NOT NULL;";

/// Rebuildable public artifact metadata produced by `publish-artifact`.
pub const CREATE_PUBLISHED_ARTIFACTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS published_artifacts (
    id                     TEXT         PRIMARY KEY,
    tenant                 TEXT         NOT NULL,
    source_file_id         TEXT         NOT NULL,
    source_file_version_id TEXT         NOT NULL DEFAULT '',
    content_hash           TEXT         NOT NULL,
    label                  TEXT         NOT NULL,
    mime_type              TEXT         NOT NULL,
    byte_length            BIGINT       NOT NULL,
    public_storage_key     TEXT         NOT NULL,
    public_url             TEXT         NOT NULL,
    owner_ref_type         TEXT         NOT NULL,
    owner_ref_id           TEXT         NOT NULL,
    status                 TEXT         NOT NULL,
    updated_at             TIMESTAMPTZ  NOT NULL DEFAULT now()
);";

/// Fast lookup for app-owned published artifacts.
pub const CREATE_PUBLISHED_ARTIFACTS_OWNER_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_published_artifacts_owner
    ON published_artifacts(tenant, owner_ref_type, owner_ref_id, label, status);";

/// Fast lookup for artifacts derived from a source File.
pub const CREATE_PUBLISHED_ARTIFACTS_SOURCE_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_published_artifacts_source
    ON published_artifacts(tenant, source_file_id, status);";

// ---------------------------------------------------------------------------
// Row-Level Security (RLS) — tenant isolation
// ---------------------------------------------------------------------------
// Defence-in-depth: even if application code has a bug that omits a tenant
// WHERE clause, RLS prevents cross-tenant data access at the database level.
//
// Policies match `current_setting('app.current_tenant', true)` against each
// row's `tenant` column. The second argument (`true`) makes the function
// return an empty string instead of raising an error when the setting has not
// been set on the current session/transaction, so existing non-RLS-aware code
// paths do not break (queries run by the table owner bypass RLS by default).
//
// Deploy note: for RLS to be enforced at runtime, the application connection
// must use a non-superuser role and execute
// `SET LOCAL "app.current_tenant" = '<tenant>';` inside each transaction.
// ---------------------------------------------------------------------------

/// Idempotent DDL to enable RLS and create tenant isolation policies on all
/// tenant-scoped tables.
///
/// Each statement is safe to re-execute; `IF NOT EXISTS` guards the policy
/// creation, and `ALTER TABLE … ENABLE ROW LEVEL SECURITY` is a no-op when
/// RLS is already enabled.
pub const ENABLE_TENANT_RLS: &[&str] = &[
    // -- events --
    "ALTER TABLE events ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'events' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON events USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- event_segments --
    "ALTER TABLE event_segments ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'event_segments' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON event_segments USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- snapshots --
    "ALTER TABLE snapshots ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'snapshots' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON snapshots USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- snapshot_history --
    "ALTER TABLE snapshot_history ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'snapshot_history' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON snapshot_history USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- specs --
    "ALTER TABLE specs ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'specs' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON specs USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- trajectories --
    "ALTER TABLE trajectories ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'trajectories' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON trajectories USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- design_time_events --
    "ALTER TABLE design_time_events ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'design_time_events' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON design_time_events USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- tenant_constraints --
    "ALTER TABLE tenant_constraints ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'tenant_constraints' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON tenant_constraints USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- wasm_modules --
    "ALTER TABLE wasm_modules ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'wasm_modules' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON wasm_modules USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- wasm_invocation_logs --
    "ALTER TABLE wasm_invocation_logs ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'wasm_invocation_logs' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON wasm_invocation_logs USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- tenant_secrets --
    "ALTER TABLE tenant_secrets ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'tenant_secrets' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON tenant_secrets USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- pending_decisions --
    "ALTER TABLE pending_decisions ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'pending_decisions' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON pending_decisions USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- tenant_policies --
    "ALTER TABLE tenant_policies ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'tenant_policies' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON tenant_policies USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- policies --
    "ALTER TABLE policies ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'policies' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON policies USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- policy_denial_patterns --
    "ALTER TABLE policy_denial_patterns ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'policy_denial_patterns' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON policy_denial_patterns USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- tenant_installed_apps --
    "ALTER TABLE tenant_installed_apps ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'tenant_installed_apps' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON tenant_installed_apps USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- entity_catalog --
    "ALTER TABLE entity_catalog ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'entity_catalog' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON entity_catalog USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- entity_field_index --
    "ALTER TABLE entity_field_index ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'entity_field_index' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON entity_field_index USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- feature_requests --
    "ALTER TABLE feature_requests ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'feature_requests' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON feature_requests USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- evolution_records --
    "ALTER TABLE evolution_records ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'evolution_records' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON evolution_records USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- ots_trajectories --
    "ALTER TABLE ots_trajectories ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'ots_trajectories' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON ots_trajectories USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
    // -- published_artifacts --
    "ALTER TABLE published_artifacts ENABLE ROW LEVEL SECURITY",
    "DO $$ BEGIN \
       IF NOT EXISTS (SELECT 1 FROM pg_policies WHERE tablename = 'published_artifacts' AND policyname = 'tenant_isolation') THEN \
         EXECUTE 'CREATE POLICY tenant_isolation ON published_artifacts USING (tenant = current_setting(''app.current_tenant'', true))'; \
       END IF; \
     END $$",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_table_contains_required_columns() {
        let sql = CREATE_EVENTS_TABLE.to_uppercase();
        for col in &[
            "TENANT",
            "ENTITY_TYPE",
            "ENTITY_ID",
            "SEQUENCE_NR",
            "EVENT_TYPE",
            "PAYLOAD",
            "METADATA",
            "CREATED_AT",
        ] {
            assert!(sql.contains(col), "events schema missing column: {col}");
        }
    }

    #[test]
    fn events_table_has_unique_constraint() {
        let sql = CREATE_EVENTS_TABLE.to_uppercase();
        assert!(
            sql.contains("UNIQUE"),
            "events schema should enforce a UNIQUE constraint on (tenant, entity_type, entity_id, sequence_nr)"
        );
        let unique_pos = sql.find("UNIQUE").unwrap();
        let after_unique = &sql[unique_pos..];
        assert!(
            after_unique.contains("TENANT"),
            "UNIQUE constraint missing tenant"
        );
        assert!(
            after_unique.contains("ENTITY_TYPE"),
            "UNIQUE constraint missing entity_type"
        );
        assert!(
            after_unique.contains("ENTITY_ID"),
            "UNIQUE constraint missing entity_id"
        );
        assert!(
            after_unique.contains("SEQUENCE_NR"),
            "UNIQUE constraint missing sequence_nr"
        );
    }

    #[test]
    fn snapshots_table_contains_required_columns() {
        let sql = CREATE_SNAPSHOTS_TABLE.to_uppercase();
        for col in &[
            "TENANT",
            "ENTITY_TYPE",
            "ENTITY_ID",
            "SEQUENCE_NR",
            "STATE",
            "CREATED_AT",
        ] {
            assert!(sql.contains(col), "snapshots schema missing column: {col}");
        }
    }

    #[test]
    fn snapshots_table_has_composite_primary_key() {
        let sql = CREATE_SNAPSHOTS_TABLE.to_uppercase();
        let pk_pos = sql
            .find("PRIMARY KEY")
            .expect("snapshots schema missing PRIMARY KEY");
        let after_pk = &sql[pk_pos..];
        assert!(after_pk.contains("TENANT"), "PRIMARY KEY missing tenant");
        assert!(
            after_pk.contains("ENTITY_TYPE"),
            "PRIMARY KEY missing entity_type"
        );
        assert!(
            after_pk.contains("ENTITY_ID"),
            "PRIMARY KEY missing entity_id"
        );
    }

    #[test]
    fn events_table_uses_jsonb_for_payload_and_metadata() {
        let sql = CREATE_EVENTS_TABLE.to_uppercase();
        // Find PAYLOAD line and verify it uses JSONB
        assert!(
            sql.contains("PAYLOAD") && sql.contains("JSONB"),
            "payload should be JSONB"
        );
    }

    #[test]
    fn snapshots_table_uses_bytea_for_state() {
        let sql = CREATE_SNAPSHOTS_TABLE.to_uppercase();
        assert!(
            sql.contains("STATE") && sql.contains("BYTEA"),
            "state should be BYTEA"
        );
    }

    #[test]
    fn schemas_use_if_not_exists() {
        assert!(
            CREATE_EVENTS_TABLE.contains("IF NOT EXISTS"),
            "events schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_SNAPSHOTS_TABLE.contains("IF NOT EXISTS"),
            "snapshots schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_SPECS_TABLE.contains("IF NOT EXISTS"),
            "specs schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_TRAJECTORIES_TABLE.contains("IF NOT EXISTS"),
            "trajectories schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_DESIGN_TIME_EVENTS_TABLE.contains("IF NOT EXISTS"),
            "design_time_events schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_TENANT_CONSTRAINTS_TABLE.contains("IF NOT EXISTS"),
            "tenant_constraints schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_WASM_MODULES_TABLE.contains("IF NOT EXISTS"),
            "wasm_modules schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_WASM_INVOCATION_LOGS_TABLE.contains("IF NOT EXISTS"),
            "wasm_invocation_logs schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_TENANT_SECRETS_TABLE.contains("IF NOT EXISTS"),
            "tenant_secrets schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_PENDING_DECISIONS_TABLE.contains("IF NOT EXISTS"),
            "pending_decisions schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_TENANT_POLICIES_TABLE.contains("IF NOT EXISTS"),
            "tenant_policies schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_POLICIES_TABLE.contains("IF NOT EXISTS"),
            "policies schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_POLICY_DENIAL_PATTERNS_TABLE.contains("IF NOT EXISTS"),
            "policy_denial_patterns schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_TENANT_INSTALLED_APPS_TABLE.contains("IF NOT EXISTS"),
            "tenant_installed_apps schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_ENTITY_CATALOG_TABLE.contains("IF NOT EXISTS"),
            "entity_catalog schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_ENTITY_FIELD_INDEX_TABLE.contains("IF NOT EXISTS"),
            "entity_field_index schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_FEATURE_REQUESTS_TABLE.contains("IF NOT EXISTS"),
            "feature_requests schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_EVOLUTION_RECORDS_TABLE.contains("IF NOT EXISTS"),
            "evolution_records schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_OTS_TRAJECTORIES_TABLE.contains("IF NOT EXISTS"),
            "ots_trajectories schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_BLOBS_TABLE.contains("IF NOT EXISTS"),
            "blobs schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_PUBLISHED_ARTIFACTS_TABLE.contains("IF NOT EXISTS"),
            "published_artifacts schema should use IF NOT EXISTS"
        );
    }

    #[test]
    fn published_artifacts_table_has_required_columns() {
        let sql = CREATE_PUBLISHED_ARTIFACTS_TABLE.to_uppercase();
        for col in &[
            "ID",
            "TENANT",
            "SOURCE_FILE_ID",
            "SOURCE_FILE_VERSION_ID",
            "CONTENT_HASH",
            "LABEL",
            "MIME_TYPE",
            "BYTE_LENGTH",
            "PUBLIC_STORAGE_KEY",
            "PUBLIC_URL",
            "OWNER_REF_TYPE",
            "OWNER_REF_ID",
            "STATUS",
            "UPDATED_AT",
        ] {
            assert!(
                sql.contains(col),
                "published_artifacts schema missing column: {col}"
            );
        }
    }

    #[test]
    fn wasm_invocation_logs_table_has_required_columns() {
        let sql = CREATE_WASM_INVOCATION_LOGS_TABLE.to_uppercase();
        for col in &[
            "TENANT",
            "ENTITY_TYPE",
            "ENTITY_ID",
            "MODULE_NAME",
            "TRIGGER_ACTION",
            "CALLBACK_ACTION",
            "SUCCESS",
            "ERROR",
            "DURATION_MS",
            "CREATED_AT",
        ] {
            assert!(
                sql.contains(col),
                "wasm_invocation_logs schema missing column: {col}"
            );
        }
    }

    #[test]
    fn wasm_modules_table_has_required_columns() {
        let sql = CREATE_WASM_MODULES_TABLE.to_uppercase();
        for col in &[
            "TENANT",
            "MODULE_NAME",
            "WASM_BYTES",
            "SHA256_HASH",
            "VERSION",
            "SIZE_BYTES",
        ] {
            assert!(
                sql.contains(col),
                "wasm_modules schema missing column: {col}"
            );
        }
    }

    #[test]
    fn wasm_modules_table_has_unique_constraint() {
        let sql = CREATE_WASM_MODULES_TABLE.to_uppercase();
        assert!(
            sql.contains("UNIQUE"),
            "wasm_modules should have UNIQUE constraint"
        );
    }

    #[test]
    fn rls_policies_cover_all_tenant_scoped_tables() {
        let expected_tables = [
            "events",
            "snapshots",
            "specs",
            "trajectories",
            "design_time_events",
            "tenant_constraints",
            "wasm_modules",
            "wasm_invocation_logs",
            "tenant_secrets",
            "pending_decisions",
            "tenant_policies",
            "policies",
            "policy_denial_patterns",
            "tenant_installed_apps",
            "entity_catalog",
            "entity_field_index",
            "feature_requests",
            "evolution_records",
            "ots_trajectories",
            "published_artifacts",
        ];
        let all_sql: String = ENABLE_TENANT_RLS.iter().map(|s| s.to_lowercase()).collect();
        for table in &expected_tables {
            assert!(
                all_sql.contains(&format!("alter table {table} enable row level security")),
                "RLS migration missing ENABLE for table: {table}"
            );
            assert!(
                all_sql.contains(&format!("tablename = '{table}'")),
                "RLS migration missing policy for table: {table}"
            );
        }
    }

    #[test]
    fn rls_policies_are_idempotent() {
        for stmt in ENABLE_TENANT_RLS {
            let lower = stmt.to_lowercase();
            // ALTER TABLE ... ENABLE RLS is idempotent by design.
            // Policy creation is guarded by IF NOT EXISTS check.
            if lower.contains("create policy") {
                assert!(
                    lower.contains("if not exists"),
                    "RLS policy DDL must be idempotent: {stmt}"
                );
            }
        }
    }

    #[test]
    fn entity_listing_index_targets_tenant_type_and_id() {
        let sql = CREATE_ENTITY_LISTING_INDEX.to_uppercase();
        assert!(
            sql.contains("CREATE INDEX IF NOT EXISTS"),
            "index DDL should be idempotent"
        );
        assert!(
            sql.contains("IDX_EVENTS_TENANT_ENTITY"),
            "index name should be stable"
        );
        assert!(
            sql.contains("ON EVENTS"),
            "index should target events table"
        );
        assert!(
            sql.contains("(TENANT, ENTITY_TYPE, ENTITY_ID)"),
            "index should cover tenant/entity_type/entity_id"
        );
    }

    #[test]
    fn query_projection_schema_uses_jsonb_and_gin() {
        let catalog_sql = CREATE_ENTITY_CATALOG_TABLE.to_uppercase();
        assert!(
            catalog_sql.contains("FIELDS") && catalog_sql.contains("JSONB"),
            "entity_catalog should store projected fields as JSONB"
        );
        let gin_sql = CREATE_ENTITY_CATALOG_FIELDS_GIN_INDEX.to_uppercase();
        assert!(
            gin_sql.contains("USING GIN"),
            "entity_catalog fields index should use GIN for JSONB containment"
        );
    }
}
