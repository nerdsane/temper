-- ADR-0171: retain one journal-sequence fence per vector-indexed entity.
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
    tenant       TEXT   NOT NULL,
    entity_type  TEXT   NOT NULL,
    generation   BIGINT NOT NULL,
    vector_set   TEXT   NOT NULL,
    PRIMARY KEY (tenant, entity_type)
);

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
