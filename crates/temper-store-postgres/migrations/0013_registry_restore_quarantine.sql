-- ARN-190: durable, tenant-scoped registry restore quarantine.
--
-- A bad committed spec remains the source of truth while activation is withheld.
-- Keep a versioned diagnostic record so degraded boot is inspectable and can be
-- acknowledged without deleting either the source row or its failure history.
CREATE TABLE IF NOT EXISTS registry_restore_quarantines (
    tenant              TEXT        NOT NULL,
    entity_type         TEXT        NOT NULL,
    spec_version        BIGINT      NOT NULL CHECK (spec_version > 0),
    constraint_version  BIGINT      NOT NULL DEFAULT 0 CHECK (constraint_version >= 0),
    reason              TEXT        NOT NULL CHECK (reason IN ('missing_csdl', 'invalid_csdl', 'registration_failed')),
    source_kind         TEXT        NOT NULL CHECK (source_kind IN ('csdl', 'ioa', 'cross_invariants', 'registration')),
    source_line         BIGINT,
    source_column       BIGINT,
    detail              TEXT        NOT NULL CHECK (octet_length(detail) <= 512),
    acknowledged_at     TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_observed_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at         TIMESTAMPTZ,
    PRIMARY KEY (tenant, entity_type, spec_version, constraint_version)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_registry_restore_quarantines_active
    ON registry_restore_quarantines (tenant, entity_type)
    WHERE resolved_at IS NULL;

ALTER TABLE registry_restore_quarantines ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON registry_restore_quarantines;
CREATE POLICY tenant_isolation ON registry_restore_quarantines
    USING (tenant = current_setting('app.current_tenant', true));
