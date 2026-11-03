-- What the newest review of each app's modules said, one row per app of a
-- tenant: `result` is JSON, `version` names what was reviewed.
CREATE TABLE IF NOT EXISTS module_reviews (
    tenant      TEXT         NOT NULL,
    app_name    TEXT         NOT NULL,
    version     TEXT         NOT NULL,
    result      TEXT         NOT NULL,
    updated_at  TIMESTAMPTZ  NOT NULL,
    PRIMARY KEY (tenant, app_name)
);

ALTER TABLE module_reviews ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON module_reviews;
CREATE POLICY tenant_isolation ON module_reviews USING (tenant = current_setting('app.current_tenant', true));
