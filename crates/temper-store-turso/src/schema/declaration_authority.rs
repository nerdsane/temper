//! Durable spec-declaration ordering used by vector reconciliation (ADR-0181).

/// Per-type monotonic declaration source/tombstone, independent of vector work.
pub(crate) const CREATE_SPEC_DECLARATION_AUTHORITY_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS spec_declaration_authority (
    tenant                  TEXT NOT NULL,
    entity_type             TEXT NOT NULL,
    revision                INTEGER NOT NULL,
    ioa_source              TEXT NOT NULL DEFAULT '',
    declaration_fingerprint TEXT NOT NULL DEFAULT '',
    present                 INTEGER NOT NULL,
    PRIMARY KEY (tenant, entity_type)
);";

/// Upgrade authority tables created by an earlier ADR-0181 build.
pub(crate) const ALTER_SPEC_DECLARATION_AUTHORITY_ADD_FINGERPRINT: &str = "\
ALTER TABLE spec_declaration_authority
ADD COLUMN declaration_fingerprint TEXT NOT NULL DEFAULT '';";

/// Bootstrap authority for specs that exist before the ADR-0181 triggers.
const SEED_PRESENT_SPEC_DECLARATION_AUTHORITY: &str = "\
INSERT INTO spec_declaration_authority
    (tenant, entity_type, revision, ioa_source, declaration_fingerprint, present)
SELECT tenant, entity_type, MAX(version, 1), ioa_source, COALESCE(content_hash, ''), 1
FROM specs
WHERE true
ON CONFLICT(tenant, entity_type) DO NOTHING;";

/// Bootstrap deletion tombstones for retained legacy vector state.
const SEED_ABSENT_SPEC_DECLARATION_AUTHORITY: &str = "\
INSERT INTO spec_declaration_authority
    (tenant, entity_type, revision, ioa_source, declaration_fingerprint, present)
SELECT known.tenant, known.entity_type, 1, '', 'absent:v1', 0
FROM (
    SELECT tenant, entity_type FROM entity_vector_index
    UNION
    SELECT tenant, entity_type FROM entity_vector_index_version
    UNION
    SELECT tenant, entity_type FROM entity_vector_reconciliation_generation
    UNION
    SELECT tenant, entity_type FROM vector_index_backfill_watermark
) AS known
WHERE NOT EXISTS (
    SELECT 1
    FROM specs
    WHERE specs.tenant = known.tenant
      AND specs.entity_type = known.entity_type
)
ON CONFLICT(tenant, entity_type) DO NOTHING;";

/// Advance declaration authority and fence existing vector work on spec insert.
const DROP_SPEC_DECLARATION_INSERT_TRIGGER: &str =
    "DROP TRIGGER IF EXISTS specs_declaration_authority_insert;";
const CREATE_SPEC_DECLARATION_INSERT_TRIGGER: &str = "\
CREATE TRIGGER specs_declaration_authority_insert
AFTER INSERT ON specs
BEGIN
    INSERT INTO spec_declaration_authority
        (tenant, entity_type, revision, ioa_source, declaration_fingerprint, present)
    VALUES (NEW.tenant, NEW.entity_type, 1, NEW.ioa_source, COALESCE(NEW.content_hash, ''), 1)
    ON CONFLICT(tenant, entity_type) DO UPDATE SET
        revision = spec_declaration_authority.revision + 1,
        ioa_source = excluded.ioa_source,
        declaration_fingerprint = excluded.declaration_fingerprint,
        present = excluded.present;
    UPDATE entity_vector_reconciliation_generation
    SET generation = generation + 1,
        declaration_revision = (
            SELECT revision FROM spec_declaration_authority
            WHERE tenant = NEW.tenant AND entity_type = NEW.entity_type
        ),
        declaration_fingerprint = '',
        vector_set = ''
    WHERE tenant = NEW.tenant AND entity_type = NEW.entity_type;
    DELETE FROM vector_index_backfill_watermark
    WHERE tenant = NEW.tenant AND entity_type = NEW.entity_type;
END;";

/// Advance declaration authority only when a spec's IOA source changes.
const DROP_SPEC_DECLARATION_UPDATE_TRIGGER: &str =
    "DROP TRIGGER IF EXISTS specs_declaration_authority_update;";
const CREATE_SPEC_DECLARATION_UPDATE_TRIGGER: &str = "\
CREATE TRIGGER specs_declaration_authority_update
AFTER UPDATE OF ioa_source, content_hash ON specs
WHEN OLD.ioa_source IS NOT NEW.ioa_source
  OR OLD.content_hash IS NOT NEW.content_hash
BEGIN
    INSERT INTO spec_declaration_authority
        (tenant, entity_type, revision, ioa_source, declaration_fingerprint, present)
    VALUES (NEW.tenant, NEW.entity_type, 1, NEW.ioa_source, COALESCE(NEW.content_hash, ''), 1)
    ON CONFLICT(tenant, entity_type) DO UPDATE SET
        revision = spec_declaration_authority.revision + 1,
        ioa_source = excluded.ioa_source,
        declaration_fingerprint = excluded.declaration_fingerprint,
        present = excluded.present;
    UPDATE entity_vector_reconciliation_generation
    SET generation = generation + 1,
        declaration_revision = (
            SELECT revision FROM spec_declaration_authority
            WHERE tenant = NEW.tenant AND entity_type = NEW.entity_type
        ),
        declaration_fingerprint = '',
        vector_set = ''
    WHERE tenant = NEW.tenant AND entity_type = NEW.entity_type;
    DELETE FROM vector_index_backfill_watermark
    WHERE tenant = NEW.tenant AND entity_type = NEW.entity_type;
END;";

/// Persist an absence tombstone and fence existing vector work on spec delete.
const DROP_SPEC_DECLARATION_DELETE_TRIGGER: &str =
    "DROP TRIGGER IF EXISTS specs_declaration_authority_delete;";
const CREATE_SPEC_DECLARATION_DELETE_TRIGGER: &str = "\
CREATE TRIGGER specs_declaration_authority_delete
AFTER DELETE ON specs
BEGIN
    INSERT INTO spec_declaration_authority
        (tenant, entity_type, revision, ioa_source, declaration_fingerprint, present)
    VALUES (OLD.tenant, OLD.entity_type, 1, '', 'absent:v1', 0)
    ON CONFLICT(tenant, entity_type) DO UPDATE SET
        revision = spec_declaration_authority.revision + 1,
        ioa_source = excluded.ioa_source,
        declaration_fingerprint = excluded.declaration_fingerprint,
        present = excluded.present;
    UPDATE entity_vector_reconciliation_generation
    SET generation = generation + 1,
        declaration_revision = (
            SELECT revision FROM spec_declaration_authority
            WHERE tenant = OLD.tenant AND entity_type = OLD.entity_type
        ),
        declaration_fingerprint = '',
        vector_set = ''
    WHERE tenant = OLD.tenant AND entity_type = OLD.entity_type;
    DELETE FROM vector_index_backfill_watermark
    WHERE tenant = OLD.tenant AND entity_type = OLD.entity_type;
END;";

/// Ordered schema statements for durable declaration authority.
pub(crate) const DECLARATION_AUTHORITY_STATEMENTS: &[&str] = &[
    SEED_PRESENT_SPEC_DECLARATION_AUTHORITY,
    SEED_ABSENT_SPEC_DECLARATION_AUTHORITY,
    DROP_SPEC_DECLARATION_INSERT_TRIGGER,
    CREATE_SPEC_DECLARATION_INSERT_TRIGGER,
    DROP_SPEC_DECLARATION_UPDATE_TRIGGER,
    CREATE_SPEC_DECLARATION_UPDATE_TRIGGER,
    DROP_SPEC_DECLARATION_DELETE_TRIGGER,
    CREATE_SPEC_DECLARATION_DELETE_TRIGGER,
];
