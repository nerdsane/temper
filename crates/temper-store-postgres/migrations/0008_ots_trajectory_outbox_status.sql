ALTER TABLE ots_trajectories
    ADD COLUMN IF NOT EXISTS persistence_status TEXT NOT NULL DEFAULT 'persisted';

ALTER TABLE ots_trajectories
    ADD COLUMN IF NOT EXISTS persist_attempts BIGINT NOT NULL DEFAULT 0;

ALTER TABLE ots_trajectories
    ADD COLUMN IF NOT EXISTS last_error TEXT;

ALTER TABLE ots_trajectories
    ADD COLUMN IF NOT EXISTS updated_at TIMESTAMPTZ NOT NULL DEFAULT now();

CREATE INDEX IF NOT EXISTS idx_ots_trajectories_status
    ON ots_trajectories (persistence_status, updated_at);
