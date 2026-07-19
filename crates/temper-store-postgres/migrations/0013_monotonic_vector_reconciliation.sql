-- ADR-0181: retain one journal-sequence fence per vector-indexed entity.
--
-- The row survives when the entity's vector set is empty. Backfill transactions
-- compare their observed journal sequence against this fence before replacing any
-- rows, so a rebuild that loaded N cannot overwrite a live append committed at N+1.
CREATE TABLE IF NOT EXISTS entity_vector_index_version (
    tenant                    TEXT   NOT NULL,
    entity_type               TEXT   NOT NULL,
    entity_id                 TEXT   NOT NULL,
    reconciliation_generation BIGINT NOT NULL DEFAULT 0,
    sequence_nr               BIGINT NOT NULL,
    PRIMARY KEY (tenant, entity_type, entity_id)
);

ALTER TABLE entity_vector_index_version
    ADD COLUMN IF NOT EXISTS reconciliation_generation BIGINT NOT NULL DEFAULT 0;

-- Durable ordering for overlapping declaration-set reconciliations. Every entity
-- replacement and final watermark must carry the current generation.
CREATE TABLE IF NOT EXISTS entity_vector_reconciliation_generation (
    tenant                  TEXT   NOT NULL,
    entity_type             TEXT   NOT NULL,
    generation              BIGINT NOT NULL,
    declaration_revision    BIGINT NOT NULL DEFAULT 0,
    declaration_fingerprint TEXT   NOT NULL DEFAULT '',
    vector_set              TEXT   NOT NULL,
    PRIMARY KEY (tenant, entity_type)
);

ALTER TABLE entity_vector_reconciliation_generation
    ADD COLUMN IF NOT EXISTS declaration_revision BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS declaration_fingerprint TEXT NOT NULL DEFAULT '';

-- Durable declaration authority is separate from reconciliation state so ordinary
-- non-vector entity types do not become vector-repair work merely because their
-- spec exists. Tombstones deliberately survive hard deletion and preserve the
-- per-type revision across delete/re-add cycles.
CREATE TABLE IF NOT EXISTS spec_declaration_authority (
    tenant                  TEXT    NOT NULL,
    entity_type             TEXT    NOT NULL,
    revision                BIGINT  NOT NULL,
    ioa_source              TEXT    NOT NULL DEFAULT '',
    declaration_fingerprint TEXT    NOT NULL DEFAULT '',
    present                 BOOLEAN NOT NULL,
    PRIMARY KEY (tenant, entity_type)
);

ALTER TABLE spec_declaration_authority
    ADD COLUMN IF NOT EXISTS declaration_fingerprint TEXT NOT NULL DEFAULT '';

-- All reconciliation metadata is tenant-owned state. Keep these statements
-- idempotent because local databases may have created the tables while this
-- migration was under development.
ALTER TABLE entity_vector_index_version ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON entity_vector_index_version;
CREATE POLICY tenant_isolation ON entity_vector_index_version
    USING (tenant = current_setting('app.current_tenant', true))
    WITH CHECK (tenant = current_setting('app.current_tenant', true));

ALTER TABLE entity_vector_reconciliation_generation ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON entity_vector_reconciliation_generation;
CREATE POLICY tenant_isolation ON entity_vector_reconciliation_generation
    USING (tenant = current_setting('app.current_tenant', true))
    WITH CHECK (tenant = current_setting('app.current_tenant', true));

ALTER TABLE spec_declaration_authority ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON spec_declaration_authority;
CREATE POLICY tenant_isolation ON spec_declaration_authority
    USING (tenant = current_setting('app.current_tenant', true))
    WITH CHECK (tenant = current_setting('app.current_tenant', true));

-- Preserve the strongest sequence already present when upgrading an existing
-- index. Rows written by the legacy backfill carry sequence 0 and are deliberately
-- rebuilt once through the revisioned watermark protocol.
INSERT INTO entity_vector_index_version
    (tenant, entity_type, entity_id, reconciliation_generation, sequence_nr)
SELECT tenant, entity_type, entity_id, 0, MAX(sequence_nr)
FROM entity_vector_index
GROUP BY tenant, entity_type, entity_id
ON CONFLICT (tenant, entity_type, entity_id)
DO UPDATE SET
    reconciliation_generation = GREATEST(
        entity_vector_index_version.reconciliation_generation,
        EXCLUDED.reconciliation_generation
    ),
    sequence_nr = CASE
        WHEN entity_vector_index_version.reconciliation_generation
             = EXCLUDED.reconciliation_generation
        THEN GREATEST(entity_vector_index_version.sequence_nr, EXCLUDED.sequence_nr)
        ELSE entity_vector_index_version.sequence_nr
    END;

-- Seed current specs and tombstones for legacy vector state. A type can have no
-- current spec yet still require one final empty reconciliation to purge retained
-- candidates or an old completion watermark.
INSERT INTO spec_declaration_authority
    (tenant, entity_type, revision, ioa_source, declaration_fingerprint, present)
SELECT tenant, entity_type, GREATEST(version::BIGINT, 1), ioa_source, content_hash, true
FROM specs
ON CONFLICT (tenant, entity_type) DO NOTHING;

INSERT INTO spec_declaration_authority
    (tenant, entity_type, revision, ioa_source, declaration_fingerprint, present)
SELECT known.tenant, known.entity_type, 1, '', 'absent:v1', false
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
ON CONFLICT (tenant, entity_type) DO NOTHING;

-- Upgrade authority rows created by an earlier development version of this
-- migration. New catalog rows carry their exact content hash; a blank value is
-- retained only for legacy catalogs whose hash was never persisted, allowing
-- the runtime to derive SHA-256 from ioa_source without encoding it as source.
UPDATE spec_declaration_authority AS authority
SET declaration_fingerprint = specs.content_hash
FROM specs
WHERE authority.tenant = specs.tenant
  AND authority.entity_type = specs.entity_type
  AND authority.present
  AND authority.declaration_fingerprint = ''
  AND specs.content_hash <> '';

UPDATE spec_declaration_authority
SET declaration_fingerprint = 'absent:v1'
WHERE NOT present
  AND declaration_fingerprint = '';

-- Spec mutation is the declaration-order commit point. It advances the durable
-- authority tombstone/source and immediately fences an existing vector rebuild;
-- the next reconciliation claims that already-advanced generation. This closes
-- the interval between spec persistence and background-backfill startup.
CREATE OR REPLACE FUNCTION advance_spec_declaration_authority()
RETURNS TRIGGER AS $$
DECLARE
    authority_tenant TEXT;
    authority_entity_type TEXT;
    authority_source TEXT;
    authority_fingerprint TEXT;
    authority_present BOOLEAN;
    next_revision BIGINT;
BEGIN
    IF TG_OP = 'DELETE' THEN
        authority_tenant := OLD.tenant;
        authority_entity_type := OLD.entity_type;
        authority_source := '';
        authority_fingerprint := 'absent:v1';
        authority_present := false;
    ELSE
        authority_tenant := NEW.tenant;
        authority_entity_type := NEW.entity_type;
        authority_source := NEW.ioa_source;
        authority_fingerprint := NEW.content_hash;
        authority_present := true;
    END IF;

    -- Serialize catalog mutation, compatibility bootstrap, and tombstoning for
    -- one tenant/type even before an authority row exists to take a row lock.
    PERFORM pg_advisory_xact_lock(
        hashtextextended(authority_tenant || ':' || authority_entity_type, 0)
    );

    INSERT INTO spec_declaration_authority
        (tenant, entity_type, revision, ioa_source, declaration_fingerprint, present)
    VALUES (
        authority_tenant,
        authority_entity_type,
        1,
        authority_source,
        authority_fingerprint,
        authority_present
    )
    ON CONFLICT (tenant, entity_type) DO UPDATE SET
        revision = spec_declaration_authority.revision + 1,
        ioa_source = EXCLUDED.ioa_source,
        declaration_fingerprint = EXCLUDED.declaration_fingerprint,
        present = EXCLUDED.present
    RETURNING revision INTO next_revision;

    UPDATE entity_vector_reconciliation_generation
    SET generation = generation + 1,
        declaration_revision = next_revision,
        declaration_fingerprint = '',
        vector_set = ''
    WHERE tenant = authority_tenant
      AND entity_type = authority_entity_type;

    -- A completion claim is invalid as soon as declaration authority changes,
    -- even when no reconciliation-generation row has been created yet.
    DELETE FROM vector_index_backfill_watermark
    WHERE tenant = authority_tenant
      AND entity_type = authority_entity_type;

    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS specs_declaration_authority_insert ON specs;
CREATE TRIGGER specs_declaration_authority_insert
AFTER INSERT ON specs
FOR EACH ROW
EXECUTE FUNCTION advance_spec_declaration_authority();

DROP TRIGGER IF EXISTS specs_declaration_authority_update ON specs;
CREATE TRIGGER specs_declaration_authority_update
AFTER UPDATE OF ioa_source, content_hash ON specs
FOR EACH ROW
WHEN (
    OLD.ioa_source IS DISTINCT FROM NEW.ioa_source
    OR OLD.content_hash IS DISTINCT FROM NEW.content_hash
)
EXECUTE FUNCTION advance_spec_declaration_authority();

DROP TRIGGER IF EXISTS specs_declaration_authority_delete ON specs;
CREATE TRIGGER specs_declaration_authority_delete
AFTER DELETE ON specs
FOR EACH ROW
EXECUTE FUNCTION advance_spec_declaration_authority();

-- Full replacement must retain declaration absence even when compatibility
-- constructors bootstrapped authority without ever creating a specs row.
-- Callers with only a PgPool invoke:
--   SELECT tombstone_spec_declaration_authority($1, $2)
CREATE OR REPLACE FUNCTION tombstone_spec_declaration_authority(
    target_tenant TEXT,
    target_entity_type TEXT
)
RETURNS VOID AS $$
DECLARE
    deleted_catalog_rows BIGINT;
    next_revision BIGINT;
BEGIN
    PERFORM pg_advisory_xact_lock(
        hashtextextended(target_tenant || ':' || target_entity_type, 0)
    );

    DELETE FROM specs
    WHERE tenant = target_tenant
      AND entity_type = target_entity_type;
    GET DIAGNOSTICS deleted_catalog_rows = ROW_COUNT;

    -- The DELETE trigger already advanced authority and fenced reconciliation.
    IF deleted_catalog_rows > 0 THEN
        RETURN;
    END IF;

    INSERT INTO spec_declaration_authority
        (tenant, entity_type, revision, ioa_source, declaration_fingerprint, present)
    VALUES (
        target_tenant,
        target_entity_type,
        1,
        '',
        'absent:v1',
        false
    )
    ON CONFLICT (tenant, entity_type) DO UPDATE SET
        revision = spec_declaration_authority.revision + 1,
        ioa_source = '',
        declaration_fingerprint = 'absent:v1',
        present = false
    WHERE spec_declaration_authority.present
    RETURNING revision INTO next_revision;

    -- Repeating an already-persisted tombstone is an idempotent no-op.
    IF next_revision IS NULL THEN
        RETURN;
    END IF;

    UPDATE entity_vector_reconciliation_generation
    SET generation = generation + 1,
        declaration_revision = next_revision,
        declaration_fingerprint = '',
        vector_set = ''
    WHERE tenant = target_tenant
      AND entity_type = target_entity_type;

    DELETE FROM vector_index_backfill_watermark
    WHERE tenant = target_tenant
      AND entity_type = target_entity_type;
END;
$$ LANGUAGE plpgsql;
