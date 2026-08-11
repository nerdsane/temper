-- Session-scoped trajectory replay (conformance checking).
--
-- Conformance reads one session's rows in the order the kernel wrote them,
-- so the index covers both the filter column and the ordering columns.
CREATE INDEX IF NOT EXISTS idx_trajectories_session
    ON trajectories (session_id, created_at, id);
