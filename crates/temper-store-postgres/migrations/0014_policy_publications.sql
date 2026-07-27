-- ADR-0162: versioned, atomic tenant policy publication.
CREATE TABLE IF NOT EXISTS policy_publications (
    tenant        TEXT         PRIMARY KEY,
    version       BIGINT       NOT NULL DEFAULT 0 CHECK (version >= 0),
    snapshot_hash TEXT         NOT NULL DEFAULT '',
    updated_at    TIMESTAMPTZ  NOT NULL DEFAULT now()
);

ALTER TABLE policy_publications ENABLE ROW LEVEL SECURITY;

DO $$ BEGIN
  IF NOT EXISTS (
    SELECT 1
      FROM pg_policies
     WHERE tablename = 'policy_publications'
       AND policyname = 'tenant_isolation'
  ) THEN
    CREATE POLICY tenant_isolation ON policy_publications
      USING (tenant = current_setting('app.current_tenant', true));
  END IF;
END $$;
