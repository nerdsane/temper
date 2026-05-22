CREATE TABLE IF NOT EXISTS events (
    id            BIGSERIAL    NOT NULL,
    tenant        TEXT         NOT NULL DEFAULT 'default',
    entity_type   TEXT         NOT NULL,
    entity_id     TEXT         NOT NULL,
    sequence_nr   BIGINT       NOT NULL,
    event_type    TEXT         NOT NULL,
    payload       JSONB        NOT NULL,
    metadata      JSONB        NOT NULL,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (id),
    UNIQUE (tenant, entity_type, entity_id, sequence_nr)
);

CREATE TABLE IF NOT EXISTS snapshots (
    tenant        TEXT         NOT NULL DEFAULT 'default',
    entity_type   TEXT         NOT NULL,
    entity_id     TEXT         NOT NULL,
    sequence_nr   BIGINT       NOT NULL,
    state         BYTEA        NOT NULL,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, entity_type, entity_id)
);

CREATE TABLE IF NOT EXISTS specs (
    id                  BIGSERIAL    PRIMARY KEY,
    tenant              TEXT         NOT NULL,
    entity_type         TEXT         NOT NULL,
    ioa_source          TEXT         NOT NULL,
    csdl_xml            TEXT,
    version             INT          NOT NULL DEFAULT 1,
    verified            BOOLEAN      NOT NULL DEFAULT false,
    verification_status TEXT         NOT NULL DEFAULT 'pending',
    levels_passed       INT,
    levels_total        INT,
    verification_result JSONB,
    content_hash        TEXT         NOT NULL DEFAULT '',
    committed           BOOLEAN      NOT NULL DEFAULT true,
    created_at          TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ  NOT NULL DEFAULT now(),
    UNIQUE (tenant, entity_type)
);

ALTER TABLE specs ADD COLUMN IF NOT EXISTS content_hash TEXT NOT NULL DEFAULT '';
ALTER TABLE specs ADD COLUMN IF NOT EXISTS committed BOOLEAN NOT NULL DEFAULT true;

CREATE TABLE IF NOT EXISTS trajectories (
    id                 BIGSERIAL    PRIMARY KEY,
    tenant             TEXT         NOT NULL,
    entity_type        TEXT         NOT NULL,
    entity_id          TEXT         NOT NULL DEFAULT '',
    action             TEXT         NOT NULL,
    success            BOOLEAN      NOT NULL,
    from_status        TEXT,
    to_status          TEXT,
    error              TEXT,
    agent_id           TEXT,
    session_id         TEXT,
    authz_denied       BOOLEAN,
    denied_resource    TEXT,
    denied_module      TEXT,
    source             TEXT,
    spec_governed      BOOLEAN,
    request_body       JSONB,
    intent             TEXT,
    matched_policy_ids JSONB,
    agent_type         TEXT,
    created_at         TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS design_time_events (
    id            BIGSERIAL    PRIMARY KEY,
    kind          TEXT         NOT NULL,
    entity_type   TEXT         NOT NULL,
    tenant        TEXT         NOT NULL,
    summary       TEXT         NOT NULL,
    level         TEXT,
    passed        BOOLEAN,
    step_number   SMALLINT,
    total_steps   SMALLINT,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS tenant_constraints (
    tenant                TEXT         NOT NULL PRIMARY KEY,
    cross_invariants_toml TEXT         NOT NULL,
    version               INT          NOT NULL DEFAULT 1,
    updated_at            TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS wasm_modules (
    tenant        TEXT         NOT NULL,
    module_name   TEXT         NOT NULL,
    wasm_bytes    BYTEA        NOT NULL,
    sha256_hash   TEXT         NOT NULL,
    version       INT          NOT NULL DEFAULT 1,
    size_bytes    INT          NOT NULL,
    updated_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    UNIQUE (tenant, module_name)
);

CREATE TABLE IF NOT EXISTS wasm_invocation_logs (
    id              BIGSERIAL    PRIMARY KEY,
    tenant          TEXT         NOT NULL,
    entity_type     TEXT         NOT NULL,
    entity_id       TEXT         NOT NULL,
    module_name     TEXT         NOT NULL,
    trigger_action  TEXT         NOT NULL,
    callback_action TEXT,
    success         BOOLEAN      NOT NULL,
    error           TEXT,
    duration_ms     BIGINT       NOT NULL,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS tenant_secrets (
    tenant      TEXT         NOT NULL,
    key_name    TEXT         NOT NULL,
    ciphertext  BYTEA        NOT NULL,
    nonce       BYTEA        NOT NULL,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, key_name)
);

CREATE TABLE IF NOT EXISTS pending_decisions (
    id         TEXT         PRIMARY KEY,
    tenant     TEXT         NOT NULL,
    status     TEXT         NOT NULL DEFAULT 'pending',
    data       JSONB        NOT NULL,
    created_at TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS tenant_policies (
    tenant      TEXT         PRIMARY KEY,
    policy_text TEXT         NOT NULL,
    updated_at  TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS policies (
    tenant      TEXT         NOT NULL,
    policy_id   TEXT         NOT NULL,
    cedar_text  TEXT         NOT NULL,
    policy_hash TEXT         NOT NULL DEFAULT '',
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT now(),
    created_by  TEXT         NOT NULL DEFAULT 'system',
    enabled     BOOLEAN      NOT NULL DEFAULT true,
    PRIMARY KEY (tenant, policy_id)
);

CREATE TABLE IF NOT EXISTS policy_denial_patterns (
    tenant                     TEXT         NOT NULL,
    agent_type                 TEXT         NOT NULL DEFAULT '',
    action                     TEXT         NOT NULL,
    resource_type              TEXT         NOT NULL,
    count                      BIGINT       NOT NULL DEFAULT 0,
    first_seen                 TIMESTAMPTZ  NOT NULL,
    last_seen                  TIMESTAMPTZ  NOT NULL,
    distinct_resource_ids_json JSONB        NOT NULL DEFAULT '[]'::jsonb,
    PRIMARY KEY (tenant, agent_type, action, resource_type)
);

CREATE TABLE IF NOT EXISTS tenant_installed_apps (
    tenant              TEXT         NOT NULL,
    app_name            TEXT         NOT NULL,
    app_version         TEXT         NOT NULL DEFAULT '',
    bundle_digest       TEXT         NOT NULL DEFAULT '',
    spec_digest         TEXT         NOT NULL DEFAULT '',
    policy_digest       TEXT         NOT NULL DEFAULT '',
    wasm_digest         TEXT         NOT NULL DEFAULT '',
    content_digest      TEXT         NOT NULL DEFAULT '',
    seed_digest         TEXT         NOT NULL DEFAULT '',
    installed_at        TIMESTAMPTZ  NOT NULL DEFAULT now(),
    last_reconciled_at  TIMESTAMPTZ,
    status              TEXT         NOT NULL DEFAULT 'installed',
    PRIMARY KEY (tenant, app_name)
);

CREATE TABLE IF NOT EXISTS entity_catalog (
    tenant             TEXT         NOT NULL,
    entity_type        TEXT         NOT NULL,
    entity_id          TEXT         NOT NULL,
    status             TEXT         NOT NULL,
    fields             JSONB        NOT NULL DEFAULT '{}'::jsonb,
    updated_at         TIMESTAMPTZ  NOT NULL DEFAULT now(),
    sequence_nr        BIGINT       NOT NULL DEFAULT 0,
    projection_version INT          NOT NULL DEFAULT 2,
    projection_hash    TEXT         NOT NULL DEFAULT '',
    PRIMARY KEY (tenant, entity_type, entity_id)
);

CREATE TABLE IF NOT EXISTS entity_field_index (
    tenant       TEXT NOT NULL,
    entity_type  TEXT NOT NULL,
    entity_id    TEXT NOT NULL,
    field_name   TEXT NOT NULL,
    field_value  TEXT,
    status       TEXT,
    PRIMARY KEY (tenant, entity_type, entity_id, field_name)
);

CREATE TABLE IF NOT EXISTS feature_requests (
    id              TEXT         PRIMARY KEY,
    tenant          TEXT         NOT NULL DEFAULT 'default',
    category        TEXT         NOT NULL,
    description     TEXT         NOT NULL,
    frequency       BIGINT       NOT NULL DEFAULT 0,
    trajectory_refs JSONB        NOT NULL DEFAULT '[]'::jsonb,
    disposition     TEXT         NOT NULL DEFAULT 'Open',
    developer_notes TEXT,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS evolution_records (
    id           TEXT         PRIMARY KEY,
    tenant       TEXT         NOT NULL DEFAULT 'default',
    record_type  TEXT         NOT NULL,
    status       TEXT         NOT NULL DEFAULT 'Open',
    created_by   TEXT         NOT NULL,
    derived_from TEXT,
    timestamp    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    payload      JSONB        NOT NULL
);

ALTER TABLE evolution_records ADD COLUMN IF NOT EXISTS tenant TEXT NOT NULL DEFAULT 'default';

CREATE TABLE IF NOT EXISTS ots_trajectories (
    trajectory_id TEXT         PRIMARY KEY,
    tenant        TEXT         NOT NULL,
    agent_id      TEXT         NOT NULL,
    session_id    TEXT,
    outcome       TEXT         NOT NULL DEFAULT 'unknown',
    entity_type   TEXT,
    turn_count    BIGINT       NOT NULL DEFAULT 0,
    data          JSONB        NOT NULL,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS blobs (
    blob_key   TEXT         PRIMARY KEY,
    data       BYTEA        NOT NULL,
    size_bytes BIGINT       NOT NULL,
    created_at TIMESTAMPTZ  NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_events_tenant_entity ON events (tenant, entity_type, entity_id);
CREATE INDEX IF NOT EXISTS idx_trajectories_success ON trajectories (success, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_trajectories_entity ON trajectories (entity_type, action);
CREATE INDEX IF NOT EXISTS idx_dt_events_tenant ON design_time_events (tenant, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_wasm_invocation_logs_tenant ON wasm_invocation_logs (tenant);
CREATE INDEX IF NOT EXISTS idx_wasm_invocation_logs_module ON wasm_invocation_logs (module_name);
CREATE INDEX IF NOT EXISTS idx_wasm_invocation_logs_created ON wasm_invocation_logs (created_at DESC);
CREATE INDEX IF NOT EXISTS idx_pending_decisions_tenant ON pending_decisions (tenant);
CREATE INDEX IF NOT EXISTS idx_pending_decisions_status ON pending_decisions (status);
CREATE INDEX IF NOT EXISTS idx_policy_denial_patterns_tenant ON policy_denial_patterns (tenant, last_seen DESC);
CREATE INDEX IF NOT EXISTS idx_entity_catalog_type ON entity_catalog (tenant, entity_type);
CREATE INDEX IF NOT EXISTS idx_entity_catalog_status ON entity_catalog (tenant, entity_type, status);
CREATE INDEX IF NOT EXISTS idx_entity_catalog_fields_gin ON entity_catalog USING GIN (fields);
CREATE INDEX IF NOT EXISTS idx_efi_lookup ON entity_field_index (tenant, entity_type, field_name, field_value);
CREATE INDEX IF NOT EXISTS idx_efi_status ON entity_field_index (tenant, entity_type, status);
CREATE INDEX IF NOT EXISTS idx_evolution_records_type_status ON evolution_records (record_type, status);
CREATE INDEX IF NOT EXISTS idx_evolution_records_derived_from ON evolution_records (derived_from);
CREATE INDEX IF NOT EXISTS idx_ots_trajectories_agent ON ots_trajectories (agent_id);
CREATE INDEX IF NOT EXISTS idx_ots_trajectories_tenant ON ots_trajectories (tenant);
CREATE INDEX IF NOT EXISTS idx_ots_trajectories_outcome ON ots_trajectories (outcome);
CREATE INDEX IF NOT EXISTS idx_blobs_expires_at ON blobs (expires_at) WHERE expires_at IS NOT NULL;

ALTER TABLE events ENABLE ROW LEVEL SECURITY;
ALTER TABLE snapshots ENABLE ROW LEVEL SECURITY;
ALTER TABLE specs ENABLE ROW LEVEL SECURITY;
ALTER TABLE trajectories ENABLE ROW LEVEL SECURITY;
ALTER TABLE design_time_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_constraints ENABLE ROW LEVEL SECURITY;
ALTER TABLE wasm_modules ENABLE ROW LEVEL SECURITY;
ALTER TABLE wasm_invocation_logs ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_secrets ENABLE ROW LEVEL SECURITY;
ALTER TABLE pending_decisions ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_policies ENABLE ROW LEVEL SECURITY;
ALTER TABLE policies ENABLE ROW LEVEL SECURITY;
ALTER TABLE policy_denial_patterns ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenant_installed_apps ENABLE ROW LEVEL SECURITY;
ALTER TABLE entity_catalog ENABLE ROW LEVEL SECURITY;
ALTER TABLE entity_field_index ENABLE ROW LEVEL SECURITY;
ALTER TABLE feature_requests ENABLE ROW LEVEL SECURITY;
ALTER TABLE evolution_records ENABLE ROW LEVEL SECURITY;
ALTER TABLE ots_trajectories ENABLE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS tenant_isolation ON events;
CREATE POLICY tenant_isolation ON events USING (tenant = current_setting('app.current_tenant', true));
DROP POLICY IF EXISTS tenant_isolation ON snapshots;
CREATE POLICY tenant_isolation ON snapshots USING (tenant = current_setting('app.current_tenant', true));
DROP POLICY IF EXISTS tenant_isolation ON specs;
CREATE POLICY tenant_isolation ON specs USING (tenant = current_setting('app.current_tenant', true));
DROP POLICY IF EXISTS tenant_isolation ON trajectories;
CREATE POLICY tenant_isolation ON trajectories USING (tenant = current_setting('app.current_tenant', true));
DROP POLICY IF EXISTS tenant_isolation ON design_time_events;
CREATE POLICY tenant_isolation ON design_time_events USING (tenant = current_setting('app.current_tenant', true));
DROP POLICY IF EXISTS tenant_isolation ON tenant_constraints;
CREATE POLICY tenant_isolation ON tenant_constraints USING (tenant = current_setting('app.current_tenant', true));
DROP POLICY IF EXISTS tenant_isolation ON wasm_modules;
CREATE POLICY tenant_isolation ON wasm_modules USING (tenant = current_setting('app.current_tenant', true));
DROP POLICY IF EXISTS tenant_isolation ON wasm_invocation_logs;
CREATE POLICY tenant_isolation ON wasm_invocation_logs USING (tenant = current_setting('app.current_tenant', true));
DROP POLICY IF EXISTS tenant_isolation ON tenant_secrets;
CREATE POLICY tenant_isolation ON tenant_secrets USING (tenant = current_setting('app.current_tenant', true));
DROP POLICY IF EXISTS tenant_isolation ON pending_decisions;
CREATE POLICY tenant_isolation ON pending_decisions USING (tenant = current_setting('app.current_tenant', true));
DROP POLICY IF EXISTS tenant_isolation ON tenant_policies;
CREATE POLICY tenant_isolation ON tenant_policies USING (tenant = current_setting('app.current_tenant', true));
DROP POLICY IF EXISTS tenant_isolation ON policies;
CREATE POLICY tenant_isolation ON policies USING (tenant = current_setting('app.current_tenant', true));
DROP POLICY IF EXISTS tenant_isolation ON policy_denial_patterns;
CREATE POLICY tenant_isolation ON policy_denial_patterns USING (tenant = current_setting('app.current_tenant', true));
DROP POLICY IF EXISTS tenant_isolation ON tenant_installed_apps;
CREATE POLICY tenant_isolation ON tenant_installed_apps USING (tenant = current_setting('app.current_tenant', true));
DROP POLICY IF EXISTS tenant_isolation ON entity_catalog;
CREATE POLICY tenant_isolation ON entity_catalog USING (tenant = current_setting('app.current_tenant', true));
DROP POLICY IF EXISTS tenant_isolation ON entity_field_index;
CREATE POLICY tenant_isolation ON entity_field_index USING (tenant = current_setting('app.current_tenant', true));
DROP POLICY IF EXISTS tenant_isolation ON feature_requests;
CREATE POLICY tenant_isolation ON feature_requests USING (tenant = current_setting('app.current_tenant', true));
DROP POLICY IF EXISTS tenant_isolation ON evolution_records;
CREATE POLICY tenant_isolation ON evolution_records USING (tenant = current_setting('app.current_tenant', true));
DROP POLICY IF EXISTS tenant_isolation ON ots_trajectories;
CREATE POLICY tenant_isolation ON ots_trajectories USING (tenant = current_setting('app.current_tenant', true));
