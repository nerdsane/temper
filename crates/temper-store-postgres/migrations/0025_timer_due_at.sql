-- When each armed state timer falls due, so the timers past due (state timers
-- and scheduled actions) are one range read of an index. NULL on a row an
-- older release wrote, until a boot writes it. Due times are RFC 3339 in UTC,
-- whose byte order is time order, so the indexes compare them as "C".
ALTER TABLE state_timers ADD COLUMN IF NOT EXISTS due_at TEXT;

CREATE INDEX IF NOT EXISTS idx_state_timers_due ON state_timers (tenant, due_at COLLATE "C");
CREATE INDEX IF NOT EXISTS idx_scheduled_actions_due ON scheduled_actions (tenant, due_at COLLATE "C");
