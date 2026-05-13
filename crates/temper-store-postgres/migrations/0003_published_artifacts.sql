-- 0003_published_artifacts.sql
--
-- Persist ADR-0082 generic PublishedArtifact metadata on the canonical
-- Postgres storage backend. The table is a rebuildable read model for public
-- artifact provenance; application entities remain the publication authority.

CREATE TABLE IF NOT EXISTS published_artifacts (
    id                     TEXT         PRIMARY KEY,
    tenant                 TEXT         NOT NULL,
    source_file_id         TEXT         NOT NULL,
    source_file_version_id TEXT         NOT NULL DEFAULT '',
    content_hash           TEXT         NOT NULL,
    label                  TEXT         NOT NULL,
    mime_type              TEXT         NOT NULL,
    byte_length            BIGINT       NOT NULL,
    public_storage_key     TEXT         NOT NULL,
    public_url             TEXT         NOT NULL,
    owner_ref_type         TEXT         NOT NULL,
    owner_ref_id           TEXT         NOT NULL,
    status                 TEXT         NOT NULL,
    updated_at             TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_published_artifacts_owner
    ON published_artifacts(tenant, owner_ref_type, owner_ref_id, label, status);

CREATE INDEX IF NOT EXISTS idx_published_artifacts_source
    ON published_artifacts(tenant, source_file_id, status);

ALTER TABLE published_artifacts ENABLE ROW LEVEL SECURITY;

DO $$ BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_policies
    WHERE tablename = 'published_artifacts' AND policyname = 'tenant_isolation'
  ) THEN
    EXECUTE 'CREATE POLICY tenant_isolation ON published_artifacts USING (tenant = current_setting(''app.current_tenant'', true))';
  END IF;
END $$;
