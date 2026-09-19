-- ADR-0153: per-(tenant, entity_type) backfill watermark — the gate that lets a
-- keyed read MISS mean "authoritatively absent" instead of falling back to a
-- full-type scan (#324, the cause of the 413 in ARN-68).
--
-- A row here asserts: every entity of (tenant, entity_type) that existed when the
-- backfill ran has been keyed in entity_key_index, and the co-committed write path
-- has kept it complete since. So a keyed miss is a genuine absence, not a
-- pre-backfill gap. Without a row, misses stay scan-safe (the conservative
-- default), so the watermark only ever turns a 413-scan into an O(log n) answer —
-- it never makes a present entity look absent.
CREATE TABLE IF NOT EXISTS key_index_backfill_watermark (
    tenant       TEXT        NOT NULL,
    entity_type  TEXT        NOT NULL,
    completed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, entity_type)
);
