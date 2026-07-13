//! SQL constants and DDL for the actor runtime.

// ─── DDL (local dev / tests only — production uses k8s-resources migrations) ─

pub const CREATE_SCHEMA: &str = "CREATE SCHEMA IF NOT EXISTS odp_temper";
const LOCK_SCHEMA_INITIALIZATION: &str =
    "SELECT pg_advisory_xact_lock(hashtext('odp_temper.actor_schema_initialization'))";

pub const CREATE_ACTOR_MESSAGES: &str = "\
    CREATE TABLE IF NOT EXISTS odp_temper.actor_messages (\
        id              BIGSERIAL PRIMARY KEY, \
        namespace      TEXT NOT NULL, \
        to_actor        TEXT NOT NULL, \
        from_namespace  TEXT, \
        from_actor      TEXT, \
        message_type    TEXT NOT NULL, \
        payload         BYTEA NOT NULL, \
        correlation_id  UUID, \
        created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()\
    )";

pub const CREATE_ACTOR_MESSAGES_MAILBOX_INDEX: &str = "\
    CREATE INDEX IF NOT EXISTS idx_actor_messages_mailbox \
    ON odp_temper.actor_messages (namespace, to_actor, id)";

pub const CREATE_ACTOR_MESSAGES_CORRELATION_INDEX: &str = "\
    CREATE INDEX IF NOT EXISTS idx_actor_messages_correlation \
    ON odp_temper.actor_messages (correlation_id) WHERE correlation_id IS NOT NULL";

/// Create durable staging for delayed actor messages.
pub const CREATE_SCHEDULED_MESSAGES: &str = "\
    CREATE TABLE IF NOT EXISTS odp_temper.actor_scheduled_messages (\
        id              BIGSERIAL PRIMARY KEY, \
        namespace       TEXT NOT NULL, \
        to_actor         TEXT NOT NULL, \
        from_namespace   TEXT, \
        from_actor       TEXT, \
        message_type     TEXT NOT NULL, \
        payload          BYTEA NOT NULL, \
        correlation_id   UUID, \
        deliver_at       TIMESTAMPTZ NOT NULL, \
        created_at       TIMESTAMPTZ NOT NULL DEFAULT NOW()\
    )";

/// Index delayed messages by promotion eligibility and stable insertion order.
pub const CREATE_SCHEDULED_MESSAGES_DUE_INDEX: &str = "\
    CREATE INDEX IF NOT EXISTS idx_actor_scheduled_messages_due \
    ON odp_temper.actor_scheduled_messages (deliver_at, id)";

pub const CREATE_ACTOR_INSTANCES: &str = "\
    CREATE TABLE IF NOT EXISTS odp_temper.actor_instances (\
        namespace    TEXT NOT NULL, \
        actor_type    TEXT NOT NULL, \
        state         BYTEA NOT NULL DEFAULT '', \
        last_msg_id   BIGINT NOT NULL DEFAULT 0, \
        version       BIGINT NOT NULL DEFAULT 0, \
        created_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(), \
        updated_at    TIMESTAMPTZ NOT NULL DEFAULT NOW(), \
        PRIMARY KEY (namespace, actor_type)\
    )";

pub const CREATE_ACTOR_TYPES: &str = "\
    CREATE TABLE IF NOT EXISTS odp_temper.actor_types (\
        actor_type    TEXT PRIMARY KEY, \
        registered_at TIMESTAMPTZ NOT NULL DEFAULT NOW()\
    )";

/// Run all DDL for local dev / tests.
pub async fn create_tables(
    client: &mut tokio_postgres::Client,
) -> Result<(), tokio_postgres::Error> {
    let transaction = client.transaction().await?;
    transaction
        .batch_execute(LOCK_SCHEMA_INITIALIZATION)
        .await?;
    transaction.batch_execute(CREATE_SCHEMA).await?;
    transaction.batch_execute(CREATE_ACTOR_MESSAGES).await?;
    transaction
        .batch_execute(CREATE_ACTOR_MESSAGES_MAILBOX_INDEX)
        .await?;
    transaction
        .batch_execute(CREATE_ACTOR_MESSAGES_CORRELATION_INDEX)
        .await?;
    transaction.batch_execute(CREATE_SCHEDULED_MESSAGES).await?;
    transaction
        .batch_execute(CREATE_SCHEDULED_MESSAGES_DUE_INDEX)
        .await?;
    transaction.batch_execute(CREATE_ACTOR_INSTANCES).await?;
    transaction.batch_execute(CREATE_ACTOR_TYPES).await?;
    transaction.commit().await
}

// ─── Query constants ─────────────────────────────────────────────────────────

/// Insert a message into the mailbox. Returns the assigned ID.
pub const INSERT_MESSAGE: &str = "\
    INSERT INTO odp_temper.actor_messages \
        (namespace, to_actor, from_namespace, from_actor, message_type, payload, correlation_id) \
    VALUES ($1, $2, $3, $4, $5, $6, $7) \
    RETURNING id";

/// Insert a delayed message into durable staging.
pub const INSERT_SCHEDULED_MESSAGE: &str = "\
    INSERT INTO odp_temper.actor_scheduled_messages \
        (namespace, to_actor, from_namespace, from_actor, message_type, payload, correlation_id, deliver_at) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
    RETURNING id";

/// Atomically promote the oldest due delayed messages into the FIFO mailbox.
///
/// The transaction-scoped advisory lock protects the complete promotion only
/// because lock, selection, deletion, and insertion remain one SQL statement.
/// Callers may execute this statement in autocommit mode or inside an explicit
/// transaction, but must not split its CTE stages into separate statements.
pub const PROMOTE_DUE_MESSAGES: &str = "\
    WITH promotion_lock AS ( \
        SELECT pg_advisory_xact_lock(hashtext('odp_temper.actor_scheduled_messages')) \
    ), due AS ( \
        SELECT scheduled.id FROM odp_temper.actor_scheduled_messages scheduled \
        CROSS JOIN promotion_lock \
        WHERE deliver_at <= NOW() \
        ORDER BY deliver_at, id \
        LIMIT $1 \
        FOR UPDATE OF scheduled \
    ), promoted AS ( \
        DELETE FROM odp_temper.actor_scheduled_messages scheduled \
        USING due \
        WHERE scheduled.id = due.id \
        RETURNING scheduled.id, scheduled.namespace, scheduled.to_actor, \
                  scheduled.from_namespace, scheduled.from_actor, \
                  scheduled.message_type, scheduled.payload, \
                  scheduled.correlation_id, scheduled.deliver_at \
    ) \
    INSERT INTO odp_temper.actor_messages \
        (namespace, to_actor, from_namespace, from_actor, message_type, payload, correlation_id) \
    SELECT namespace, to_actor, from_namespace, from_actor, message_type, payload, correlation_id \
    FROM promoted \
    ORDER BY deliver_at, id";

/// Read the next unprocessed message for an actor (FIFO, one at a time).
pub const READ_NEXT_MESSAGE: &str = "\
    SELECT id, namespace, to_actor, from_namespace, from_actor, message_type, payload, correlation_id, created_at \
    FROM odp_temper.actor_messages \
    WHERE namespace = $1 AND to_actor = $2 AND id > $3 \
    ORDER BY id ASC \
    LIMIT 1";

/// Poll for an ask() response by correlation_id.
pub const READ_ASK_RESPONSE: &str = "\
    SELECT id, namespace, to_actor, from_namespace, from_actor, message_type, payload, correlation_id, created_at \
    FROM odp_temper.actor_messages \
    WHERE correlation_id = $1 AND to_actor = $2 AND id > $3 \
    ORDER BY id ASC \
    LIMIT 1";

/// Delete an ask() response after reading it (prevents re-delivery by scheduler).
pub const DELETE_MESSAGE: &str = "\
    DELETE FROM odp_temper.actor_messages WHERE id = $1";

/// Find actors with pending messages (candidates for activation).
pub const FIND_PENDING_ACTORS: &str = "\
    SELECT ai.namespace, ai.actor_type \
    FROM odp_temper.actor_instances ai \
    WHERE EXISTS ( \
        SELECT 1 FROM odp_temper.actor_messages am \
        WHERE am.namespace = ai.namespace \
          AND am.to_actor = ai.actor_type \
          AND am.id > ai.last_msg_id \
    ) \
    ORDER BY random() \
    LIMIT $1";

/// Load actor state + cursor.
pub const LOAD_ACTOR: &str = "\
    SELECT namespace, actor_type, state, last_msg_id, version \
    FROM odp_temper.actor_instances \
    WHERE namespace = $1 AND actor_type = $2";

/// Create a new actor instance.
pub const CREATE_ACTOR: &str = "\
    INSERT INTO odp_temper.actor_instances (namespace, actor_type, state) \
    VALUES ($1, $2, $3) \
    ON CONFLICT DO NOTHING";

/// Update actor state + advance cursor (optimistic concurrency).
pub const UPDATE_ACTOR: &str = "\
    UPDATE odp_temper.actor_instances \
    SET state = $1, last_msg_id = $2, version = version + 1, updated_at = NOW() \
    WHERE namespace = $3 AND actor_type = $4 AND version = $5";

/// Two-key advisory lock (64-bit collision resistance, transaction-scoped).
pub const TRY_ADVISORY_XACT_LOCK: &str =
    "SELECT pg_try_advisory_xact_lock(hashtext($1::text), hashtext($2::text))";

/// Register an actor type.
pub const REGISTER_ACTOR_TYPE: &str = "\
    INSERT INTO odp_temper.actor_types (actor_type) \
    VALUES ($1) ON CONFLICT DO NOTHING";
