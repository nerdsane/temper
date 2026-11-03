-- Pending schedule(...) and schedule_at(...) actions: one row per scheduled
-- action until it fires, read once at boot to arm each again with the
-- identity that armed it.
CREATE TABLE IF NOT EXISTS scheduled_actions (
    tenant      TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id   TEXT NOT NULL,
    arm_id      TEXT NOT NULL,
    action      TEXT NOT NULL,
    due_at      TEXT NOT NULL,
    armed_by    TEXT NOT NULL,
    PRIMARY KEY (tenant, entity_type, entity_id, arm_id)
);

ALTER TABLE scheduled_actions ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON scheduled_actions;
CREATE POLICY tenant_isolation ON scheduled_actions USING (tenant = current_setting('app.current_tenant', true));
