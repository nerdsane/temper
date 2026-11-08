-- Armed state timers (ADR-0056): one row per entity while its timer waits,
-- read once at boot to arm each timer again with the identity that armed it.
CREATE TABLE IF NOT EXISTS state_timers (
    tenant      TEXT    NOT NULL,
    entity_type TEXT    NOT NULL,
    entity_id   TEXT    NOT NULL,
    state       TEXT    NOT NULL,
    arm_id      TEXT    NOT NULL,
    sequence_nr BIGINT  NOT NULL,
    started_at  TEXT    NOT NULL,
    params      TEXT    NOT NULL,
    armed_by    TEXT    NOT NULL,
    PRIMARY KEY (tenant, entity_type, entity_id)
);

ALTER TABLE state_timers ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON state_timers;
CREATE POLICY tenant_isolation ON state_timers USING (tenant = current_setting('app.current_tenant', true));
