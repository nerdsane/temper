//! SQLite schema for task-scoped bundle deployment and migration.

/// Immutable scoped bundles plus their mutable lifecycle record.
pub const CREATE_SCHEMA_DEPLOYMENTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS schema_deployments (
    tenant TEXT NOT NULL,
    scope_kind TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    bundle_digest TEXT NOT NULL,
    record_json TEXT NOT NULL,
    PRIMARY KEY(tenant, scope_kind, scope_id, bundle_digest)
);";

/// Atomic idempotency mapping for schema deployment operations.
pub const CREATE_SCHEMA_DEPLOYMENT_IDEMPOTENCY_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS schema_deployment_idempotency (
    tenant TEXT NOT NULL,
    operation TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    bundle_digest TEXT NOT NULL,
    scope_kind TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    PRIMARY KEY(tenant, operation, idempotency_key)
);";

/// Immutable schema verification receipts.
pub const CREATE_SCHEMA_VERIFICATION_RECEIPTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS schema_verification_receipts (
    tenant TEXT NOT NULL,
    scope_kind TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    bundle_digest TEXT NOT NULL,
    receipt_id TEXT NOT NULL,
    receipt_json TEXT NOT NULL,
    PRIMARY KEY(tenant, scope_kind, scope_id, bundle_digest, receipt_id)
);";

/// One atomic active pointer per tenant-local schema scope.
pub const CREATE_SCHEMA_ACTIVE_POINTERS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS schema_active_pointers (
    tenant TEXT NOT NULL,
    scope_kind TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    pointer_json TEXT NOT NULL,
    PRIMARY KEY(tenant, scope_kind, scope_id)
);";

/// Durable fenced migration jobs.
pub const CREATE_SCHEMA_MIGRATION_JOBS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS schema_migration_jobs (
    tenant TEXT NOT NULL,
    job_id TEXT NOT NULL,
    scope_kind TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    job_json TEXT NOT NULL,
    PRIMARY KEY(tenant, job_id)
);";

/// Atomic migration-start idempotency records.
pub const CREATE_SCHEMA_MIGRATION_IDEMPOTENCY_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS schema_migration_idempotency (
    tenant TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    job_id TEXT NOT NULL,
    PRIMARY KEY(tenant, idempotency_key)
);";

/// Atomic migration-retry idempotency reservations.
pub const CREATE_SCHEMA_MIGRATION_RETRY_IDEMPOTENCY_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS schema_migration_retry_idempotency (
    tenant TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    job_id TEXT NOT NULL,
    starting_sequence INTEGER NOT NULL CHECK(starting_sequence >= 0),
    request_id TEXT NOT NULL,
    PRIMARY KEY(tenant, idempotency_key)
);";

/// Invisible transformed rows keyed by migration job and entity identity.
pub const CREATE_SCHEMA_MIGRATION_SHADOW_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS schema_migration_shadow (
    tenant TEXT NOT NULL,
    job_id TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    row_json TEXT NOT NULL,
    PRIMARY KEY(tenant, job_id, entity_type, entity_id)
);";

/// Immutable receipts that make shadow batches replay-safe.
pub const CREATE_SCHEMA_MIGRATION_BATCH_RECEIPTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS schema_migration_batch_receipts (
    tenant TEXT NOT NULL,
    job_id TEXT NOT NULL,
    receipt_id TEXT NOT NULL,
    receipt_json TEXT NOT NULL,
    PRIMARY KEY(tenant, job_id, receipt_id)
);";

/// Immutable receipts authorizing migration cutover.
pub const CREATE_SCHEMA_MIGRATION_VALIDATION_RECEIPTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS schema_migration_validation_receipts (
    tenant TEXT NOT NULL,
    job_id TEXT NOT NULL,
    receipt_id TEXT NOT NULL,
    receipt_json TEXT NOT NULL,
    PRIMARY KEY(tenant, job_id, receipt_id)
);";
