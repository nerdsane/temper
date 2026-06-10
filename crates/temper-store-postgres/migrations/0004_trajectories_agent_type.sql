-- 0004_trajectories_agent_type.sql
--
-- Add an `agent_type` column to trajectories so the agent type classification
-- captured by the server (TrajectoryEntry.agent_type, e.g. "claude-code")
-- is persisted alongside agent_id. The column already exists in databases
-- created from the current 0001_initial.sql; this ALTER covers databases
-- created by the pre-ADR-0065 bootstrap runner, where 0001's
-- CREATE TABLE IF NOT EXISTS is a no-op and never adds the column.

ALTER TABLE trajectories
    ADD COLUMN IF NOT EXISTS agent_type TEXT;
