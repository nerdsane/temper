-- ARN-68: make the key-index backfill watermark key-set aware.
--
-- Before this, the watermark was keyed only by (tenant, entity_type) with no memory of
-- WHICH declared keys it covered. So declaring a NEW key on a type that was already
-- backfilled (e.g. adding Directory `ws_path` after `name_parent`) left the type marked
-- "complete" — the backfill skipped it and the new key was never assigned to existing
-- entities, while the read still claimed authoritative absence for the uncovered key
-- (a silent wrong "not found"). Recording the covered key-set lets `complete` mean
-- "covers exactly the currently-declared keys": a key-set change re-keys on next boot.
--
-- key_set is the sorted, comma-joined declared key NAMES the backfill covered (e.g.
-- "name_parent,ws_path"). Existing rows default to '' so they read as "stale key-set"
-- and are re-keyed once against the current declaration.
ALTER TABLE key_index_backfill_watermark
    ADD COLUMN IF NOT EXISTS key_set TEXT NOT NULL DEFAULT '';
