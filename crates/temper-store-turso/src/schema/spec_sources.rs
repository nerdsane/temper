//! Durable spec-source generations and last-known-good staging schema.

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
    content_hash TEXT,
    committed INTEGER NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(tenant, entity_type)
);";

/// Candidate sources reserved for the next atomic tenant promotion.
pub const CREATE_SPEC_STAGING_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS spec_staging (
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    ioa_source TEXT NOT NULL,
    csdl_xml TEXT,
    content_hash TEXT NOT NULL DEFAULT '',
    version INTEGER NOT NULL CHECK (version > 0),
    verified INTEGER NOT NULL DEFAULT 0,
    verification_status TEXT NOT NULL DEFAULT 'pending',
    levels_passed INTEGER,
    levels_total INTEGER,
    verification_result TEXT,
    staged_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY(tenant, entity_type)
);";

/// High-water generations survive deletion and abandoned staging.
pub const CREATE_SPEC_SOURCE_GENERATIONS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS spec_source_generations (
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation > 0),
    PRIMARY KEY(tenant, entity_type)
);";

pub const BACKFILL_SPEC_SOURCE_GENERATIONS: &str = "\
INSERT INTO spec_source_generations (tenant, entity_type, generation)
SELECT tenant, entity_type, version FROM specs WHERE 1
ON CONFLICT(tenant, entity_type) DO UPDATE SET
    generation = MAX(spec_source_generations.generation, excluded.generation);";

pub const MIGRATE_LEGACY_UNCOMMITTED_SPECS: &str = "\
INSERT INTO spec_staging (
    tenant, entity_type, ioa_source, csdl_xml, content_hash, version,
    verified, verification_status, levels_passed, levels_total,
    verification_result, staged_at
)
SELECT tenant, entity_type, ioa_source, csdl_xml, COALESCE(content_hash, ''), version,
       verified, verification_status, levels_passed, levels_total,
       verification_result, updated_at
FROM specs WHERE committed = 0
ON CONFLICT(tenant, entity_type) DO UPDATE SET
    ioa_source = excluded.ioa_source,
    csdl_xml = excluded.csdl_xml,
    content_hash = excluded.content_hash,
    version = excluded.version,
    verified = excluded.verified,
    verification_status = excluded.verification_status,
    levels_passed = excluded.levels_passed,
    levels_total = excluded.levels_total,
    verification_result = excluded.verification_result,
    staged_at = excluded.staged_at;";

pub const DELETE_LEGACY_UNCOMMITTED_SPECS: &str = "DELETE FROM specs WHERE committed = 0";

pub const CREATE_TENANT_CONSTRAINTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS tenant_constraints (
    tenant TEXT NOT NULL PRIMARY KEY,
    cross_invariants_toml TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

pub const CREATE_TENANT_CONSTRAINT_GENERATIONS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS tenant_constraint_generations (
    tenant TEXT NOT NULL PRIMARY KEY,
    generation INTEGER NOT NULL CHECK (generation > 0)
);";

pub const BACKFILL_TENANT_CONSTRAINT_GENERATIONS: &str = "\
INSERT INTO tenant_constraint_generations (tenant, generation)
SELECT tenant, version FROM tenant_constraints WHERE 1
ON CONFLICT(tenant) DO UPDATE SET
    generation = MAX(tenant_constraint_generations.generation, excluded.generation);";

/// ALTER TABLE migration: add content_hash to legacy specs tables.
pub const ALTER_SPECS_ADD_CONTENT_HASH: &str = "ALTER TABLE specs ADD COLUMN content_hash TEXT";

/// ALTER TABLE migration: add the legacy committed marker when absent.
pub const ALTER_SPECS_ADD_COMMITTED: &str =
    "ALTER TABLE specs ADD COLUMN committed INTEGER NOT NULL DEFAULT 1";
