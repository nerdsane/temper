-- Capture order within the writing process.
--
-- Rows are written by independently spawned persistence tasks, so the
-- BIGSERIAL id records the order the writes landed, not the order the kernel
-- captured them. Conformance checking replays a session as a state-machine
-- walk and needs capture order, so the capturing process stamps a monotonic
-- sequence on the entry before it is queued and the session read orders by it.
-- Null on rows written before this column existed.
ALTER TABLE trajectories
    ADD COLUMN IF NOT EXISTS capture_seq BIGINT;

-- Session-scoped trajectory replay, covering every ordering column so the
-- read is an index scan. Replaces idx_trajectories_session from 0013, which
-- ordered by id.
CREATE INDEX IF NOT EXISTS idx_trajectories_session_capture
    ON trajectories (session_id, created_at, capture_seq, id);
DROP INDEX IF EXISTS idx_trajectories_session;
