-- Temper PostgreSQL initialization
-- Runs automatically on first docker compose up

-- Event store: append-only journal of all state transitions
CREATE TABLE IF NOT EXISTS events (
    id            BIGSERIAL    NOT NULL,
    entity_type   TEXT         NOT NULL,
    entity_id     TEXT         NOT NULL,
    sequence_nr   BIGINT       NOT NULL,
    event_type    TEXT         NOT NULL,
    payload       JSONB        NOT NULL,
    metadata      JSONB        NOT NULL,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (id),
    UNIQUE (entity_type, entity_id, sequence_nr)
);

-- Snapshots: periodic state checkpoints for fast actor recovery
CREATE TABLE IF NOT EXISTS snapshots (
    entity_type   TEXT         NOT NULL,
    entity_id     TEXT         NOT NULL,
    sequence_nr   BIGINT       NOT NULL,
    state         BYTEA        NOT NULL,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (entity_type, entity_id)
);

-- Actor registry: tracks which node hosts which actor (for future clustering)
CREATE TABLE IF NOT EXISTS actor_registry (
    actor_id        TEXT PRIMARY KEY,
    actor_type      TEXT         NOT NULL,
    node_id         TEXT         NOT NULL,
    shard_id        INT          NOT NULL,
    status          TEXT         NOT NULL DEFAULT 'active',
    last_heartbeat  TIMESTAMPTZ  NOT NULL DEFAULT now()
);

-- Cedar policy store: versioned ABAC policies
CREATE TABLE IF NOT EXISTS cedar_policies (
    policy_id       TEXT PRIMARY KEY,
    policy_set_id   TEXT         NOT NULL,
    policy_text     TEXT         NOT NULL,
    version         INT          NOT NULL,
    active          BOOLEAN      NOT NULL DEFAULT true,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT now()
);

-- Schema versions: tracks CSDL + TLA+ spec evolution
CREATE TABLE IF NOT EXISTS schema_versions (
    version         INT PRIMARY KEY,
    csdl_xml        TEXT         NOT NULL,
    tla_specs       JSONB        NOT NULL,
    status          TEXT         NOT NULL DEFAULT 'draft',
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT now()
);

-- Indexes for common query patterns
CREATE INDEX IF NOT EXISTS idx_events_entity
    ON events (entity_type, entity_id, sequence_nr);

CREATE INDEX IF NOT EXISTS idx_events_created
    ON events (created_at);

CREATE INDEX IF NOT EXISTS idx_actor_registry_type
    ON actor_registry (actor_type, status);
