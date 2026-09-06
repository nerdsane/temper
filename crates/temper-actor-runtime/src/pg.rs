//! Postgres-backed implementations of Mailbox and ActorActivator.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use deadpool_postgres::Pool;
use uuid::Uuid;

use crate::actor::{Actor, ActorContext, ActorError, ActorHandle, Message};
use crate::mailbox::{Mailbox, MailboxError};
use crate::schema;

fn row_to_message(row: &tokio_postgres::Row) -> Message {
    let from_namespace: Option<String> = row.get("from_namespace");
    let from_actor: Option<String> = row.get("from_actor");
    let namespace: String = row.get("namespace");
    Message {
        id: row.get("id"),
        from: match (from_namespace, from_actor) {
            (Some(ns), Some(at)) => Some(ActorHandle::new(ns, at)),
            (None, Some(at)) => Some(ActorHandle::new(namespace.clone(), at)),
            _ => None,
        },
        to: ActorHandle::new(namespace, row.get::<_, String>("to_actor")),
        message_type: row.get("message_type"),
        payload: row.get("payload"),
        correlation_id: row.get("correlation_id"),
        created_at: row.get::<_, DateTime<Utc>>("created_at"),
    }
}

// ─── PgMailbox ───────────────────────────────────────────────────────────────

/// Configuration for the PG mailbox.
#[derive(Debug, Clone)]
pub struct PgMailboxConfig {
    /// Poll interval for ask() response polling (default: 10ms).
    pub ask_poll_interval: Duration,
}

impl Default for PgMailboxConfig {
    fn default() -> Self {
        Self {
            ask_poll_interval: Duration::from_millis(10),
        }
    }
}

/// Postgres-backed mailbox.
#[derive(Clone)]
pub struct PgMailbox {
    pool: Pool,
    config: PgMailboxConfig,
}

impl PgMailbox {
    pub fn new(pool: Pool, config: PgMailboxConfig) -> Self {
        Self { pool, config }
    }
}

#[async_trait::async_trait]
impl Mailbox for PgMailbox {
    async fn tell(
        &self,
        from: Option<&ActorHandle>,
        to: &ActorHandle,
        message_type: &str,
        payload: Vec<u8>,
    ) -> Result<i64, MailboxError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| MailboxError::Storage(format!("pool: {e}")))?;

        let from_namespace = from.map(|h| h.namespace.as_str());
        let from_actor = from.map(|h| h.actor_type.as_str());
        let row = client
            .query_one(
                schema::INSERT_MESSAGE,
                &[
                    &to.namespace,
                    &to.actor_type,
                    &from_namespace,
                    &from_actor,
                    &message_type,
                    &payload,
                    &None::<Uuid>,
                ],
            )
            .await
            .map_err(|e| MailboxError::Storage(format!("insert: {e}")))?;

        Ok(row.get::<_, i64>(0))
    }

    async fn ask(
        &self,
        from: &ActorHandle,
        to: &ActorHandle,
        message_type: &str,
        payload: Vec<u8>,
        timeout: Duration,
    ) -> Result<Message, MailboxError> {
        let correlation_id = Uuid::new_v4();

        // Send the ask message with correlation_id.
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| MailboxError::Storage(format!("pool: {e}")))?;

        let row = client
            .query_one(
                schema::INSERT_MESSAGE,
                &[
                    &to.namespace,
                    &to.actor_type,
                    &Some(&from.namespace),
                    &Some(&from.actor_type),
                    &message_type,
                    &payload,
                    &Some(correlation_id),
                ],
            )
            .await
            .map_err(|e| MailboxError::Storage(format!("insert ask: {e}")))?;

        // Capture request message ID to exclude it from response polling.
        let request_id: i64 = row.get(0);

        // Poll for response (must have id > request_id to avoid self-match).
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(MailboxError::Timeout(timeout));
            }

            let poll_client = self
                .pool
                .get()
                .await
                .map_err(|e| MailboxError::Storage(format!("pool: {e}")))?;

            let rows = poll_client
                .query(
                    schema::READ_ASK_RESPONSE,
                    &[&correlation_id, &from.actor_type, &request_id],
                )
                .await
                .map_err(|e| MailboxError::Storage(format!("poll ask: {e}")))?;

            if let Some(row) = rows.first() {
                let msg = row_to_message(row);
                // Delete the response so the scheduler doesn't re-deliver it.
                let _ = poll_client
                    .execute(schema::DELETE_MESSAGE, &[&msg.id])
                    .await;
                return Ok(msg);
            }

            tokio::time::sleep(self.config.ask_poll_interval).await;
        }
    }
}

// ─── PgActorActivator ────────────────────────────────────────────────────────

/// Result of an activation attempt.
pub struct ActivationResult {
    /// Whether the actor was actually activated.
    pub activated: bool,
}

/// Errors from activation.
#[derive(Debug, thiserror::Error)]
pub enum ActivationError {
    #[error("storage error: {0}")]
    Storage(String),

    #[error("concurrency violation: expected version {expected}")]
    ConcurrencyViolation { expected: i64 },

    #[error("actor error: {0}")]
    ActorError(#[from] ActorError),
}

/// Postgres-backed actor activator.
///
/// Does the entire activation in a single PG transaction:
/// lock → load state → read next message → call handler →
/// persist state → flush buffered tells → commit.
#[derive(Clone)]
pub struct PgActorActivator {
    pool: Pool,
    mailbox: Arc<dyn Mailbox>,
}

impl PgActorActivator {
    pub fn new(pool: Pool, mailbox: Arc<dyn Mailbox>) -> Self {
        Self { pool, mailbox }
    }

    /// Activate an actor instance.
    pub async fn activate(
        &self,
        actor_handle: &ActorHandle,
        handler: &dyn Actor,
    ) -> Result<ActivationResult, ActivationError> {
        let mut client = self
            .pool
            .get()
            .await
            .map_err(|e| ActivationError::Storage(format!("pool: {e}")))?;

        let tx = client
            .transaction()
            .await
            .map_err(|e| ActivationError::Storage(format!("begin tx: {e}")))?;

        // 1. Advisory lock (two-key, transaction-scoped).
        let sid = actor_handle.namespace.to_string();
        let lock_row = tx
            .query_one(
                schema::TRY_ADVISORY_XACT_LOCK,
                &[&sid, &actor_handle.actor_type],
            )
            .await
            .map_err(|e| ActivationError::Storage(format!("lock: {e}")))?;

        if !lock_row.get::<_, bool>(0) {
            return Ok(ActivationResult { activated: false });
        }

        // 2. Load actor state (or create with initial state).
        let rows = tx
            .query(
                schema::LOAD_ACTOR,
                &[&actor_handle.namespace, &actor_handle.actor_type],
            )
            .await
            .map_err(|e| ActivationError::Storage(format!("load: {e}")))?;

        let (mut state, last_msg_id, version) = match rows.first() {
            Some(row) => {
                // Existing bytes are recovery data, including an empty value.
                // The actor owns validation; only an absent row is initialization.
                (
                    row.get::<_, Vec<u8>>("state"),
                    row.get::<_, i64>("last_msg_id"),
                    row.get::<_, i64>("version"),
                )
            }
            None => {
                let initial_state = handler.initial_state_for(actor_handle);
                tx.execute(
                    schema::CREATE_ACTOR,
                    &[
                        &actor_handle.namespace,
                        &actor_handle.actor_type,
                        &initial_state,
                    ],
                )
                .await
                .map_err(|e| ActivationError::Storage(format!("create: {e}")))?;

                (initial_state, 0i64, 0i64)
            }
        };

        // 3. Read next message (ONE message, FIFO).
        let msg_rows = tx
            .query(
                schema::READ_NEXT_MESSAGE,
                &[
                    &actor_handle.namespace,
                    &actor_handle.actor_type,
                    &last_msg_id,
                ],
            )
            .await
            .map_err(|e| ActivationError::Storage(format!("read msg: {e}")))?;

        let msg_row = match msg_rows.first() {
            Some(r) => r,
            None => {
                // No pending messages.
                tx.commit()
                    .await
                    .map_err(|e| ActivationError::Storage(format!("commit: {e}")))?;
                return Ok(ActivationResult { activated: false });
            }
        };

        let message = row_to_message(msg_row);
        let new_last_msg_id = message.id;

        tracing::debug!(
            actor = %actor_handle,
            message_id = message.id,
            message_type = %message.message_type,
            "activating actor"
        );

        // 4. Build context and call handler.
        let ctx = ActorContext::new(
            actor_handle.clone(),
            Some(self.mailbox.clone()),
            Some(self.pool.clone()),
        );

        match handler.handle(&ctx, &mut state, &message).await {
            Ok(()) => {}
            Err(error @ ActorError::Rejected(_)) => {
                // Deterministic refusals must not poison FIFO. Advance only the
                // cursor; never persist modified handler bytes or buffered tells.
                let changed = tx
                    .execute(
                        "UPDATE odp_temper.actor_instances SET last_msg_id = $1, \
                     version = version + 1, updated_at = NOW() \
                     WHERE namespace = $2 AND actor_type = $3 AND version = $4",
                        &[
                            &new_last_msg_id,
                            &actor_handle.namespace,
                            &actor_handle.actor_type,
                            &version,
                        ],
                    )
                    .await
                    .map_err(|error| {
                        ActivationError::Storage(format!("reject message: {error}"))
                    })?;
                if changed == 0 {
                    return Err(ActivationError::ConcurrencyViolation { expected: version });
                }
                tx.commit().await.map_err(|error| {
                    ActivationError::Storage(format!("commit rejection: {error}"))
                })?;
                return Err(error.into());
            }
            Err(error) => return Err(error.into()),
        }

        // 5. Persist state + advance cursor.
        let rows_affected = tx
            .execute(
                schema::UPDATE_ACTOR,
                &[
                    &state,
                    &new_last_msg_id,
                    &actor_handle.namespace,
                    &actor_handle.actor_type,
                    &version,
                ],
            )
            .await
            .map_err(|e| ActivationError::Storage(format!("update: {e}")))?;

        if rows_affected == 0 {
            return Err(ActivationError::ConcurrencyViolation { expected: version });
        }

        // 6. Flush buffered tell() messages.
        let pending = ctx.take_pending_tells().await;
        for tell in &pending {
            let from_namespace = Some(actor_handle.namespace.as_str());
            let from_actor = Some(actor_handle.actor_type.as_str());
            tx.execute(
                schema::INSERT_MESSAGE,
                &[
                    &tell.to.namespace,
                    &tell.to.actor_type,
                    &from_namespace,
                    &from_actor,
                    &tell.message_type,
                    &tell.payload,
                    &tell.correlation_id,
                ],
            )
            .await
            .map_err(|e| ActivationError::Storage(format!("flush tell: {e}")))?;
        }

        // 7. Commit — lock auto-releases.
        tx.commit()
            .await
            .map_err(|e| ActivationError::Storage(format!("commit: {e}")))?;

        tracing::debug!(
            actor = %actor_handle,
            message_type = %message.message_type,
            tells_flushed = pending.len(),
            "activation complete"
        );

        Ok(ActivationResult { activated: true })
    }
}

#[cfg(test)]
#[path = "pg_strict_tests.rs"]
mod strict_tests;
