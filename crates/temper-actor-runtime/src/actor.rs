//! Core actor types: ActorHandle, Message, Actor trait, ActorContext.

use std::time::Duration;
use tokio::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// An actor's address: `(namespace, actor_type)`.
///
/// Immutable, copyable, serializable. This is how actors refer to each other.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct ActorHandle {
    pub namespace: String,
    pub actor_type: String,
}

impl ActorHandle {
    // Note: ActorHandle::new() does no validation — the actor may not exist.
    // Messages sent to a nonexistent handle sit in actor_messages forever (dead letters).
    // Consider making this pub(crate) and forcing all external handle creation through
    // spawn() or lookup(), which validate against PG.
    pub fn new(namespace: impl Into<String>, actor_type: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            actor_type: actor_type.into(),
        }
    }
}

impl std::fmt::Display for ActorHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.namespace, self.actor_type)
    }
}

/// A message received by an actor (the full PG row).
#[derive(Debug, Clone)]
pub struct Message {
    /// Global monotonic ID.
    pub id: i64,
    /// Who sent this message (None for external/API callers).
    pub from: Option<ActorHandle>,
    /// Who this message is addressed to.
    pub to: ActorHandle,
    /// Message type name (e.g., "ExecuteBatch", "Ping").
    pub message_type: String,
    /// Payload bytes. The runtime doesn't inspect this — actors own serialization.
    pub payload: Vec<u8>,
    /// Correlation ID for ask() request-response pattern.
    pub correlation_id: Option<Uuid>,
    /// When the message was created.
    pub created_at: DateTime<Utc>,
}

impl Message {
    /// Check if this message is of a specific proto type.
    pub fn is<T: prost::Message>(&self) -> bool {
        self.message_type == type_name_of::<T>()
    }

    /// Decode the payload as a prost Message.
    pub fn decode<T: prost::Message + Default>(&self) -> Result<T, prost::DecodeError> {
        T::decode(self.payload.as_slice())
    }
}

/// Derive message_type from the Rust type name.
/// Returns the short type name (e.g., "PingMessage" not the full path).
fn type_name_of<T: ?Sized>() -> String {
    let full = std::any::type_name::<T>();
    // Take the last segment after "::" for readability.
    full.rsplit("::").next().unwrap_or(full).to_string()
}

/// An outbound message buffered by ActorContext.tell().
#[derive(Debug, Clone)]
pub(crate) struct BufferedTell {
    pub to: ActorHandle,
    pub message_type: String,
    pub payload: Vec<u8>,
    pub correlation_id: Option<Uuid>,
}

/// Errors from actor operations.
#[derive(Debug, thiserror::Error)]
pub enum ActorError {
    #[error("handler failed: {0}")]
    HandlerFailed(String),

    #[error("actor not found: {0}")]
    NotFound(String),

    #[error("mailbox error: {0}")]
    MailboxError(String),

    #[error("internal error: {0}")]
    Internal(String),

    /// Cross-namespace persistence denied (ARN-215).
    #[error("namespace capability denied: {0}")]
    NamespaceDenied(String),
}

/// The actor's interface to the system. Passed to `handle()`.
///
/// Provides identity, spawning, lookup, and messaging (tell/ask).
pub struct ActorContext {
    /// This actor's own address.
    self_handle: ActorHandle,
    /// Buffered tell() messages — flushed to PG on successful activation.
    pub(crate) pending_tells: Mutex<Vec<BufferedTell>>,
    /// Mailbox reference for ask() (immediate I/O).
    pub(crate) mailbox: Option<std::sync::Arc<dyn crate::mailbox::Mailbox>>,
    /// Pool for spawn/lookup operations.
    pub(crate) pool: Option<deadpool_postgres::Pool>,
    /// Explicit grants for cross-namespace load/upsert (ADR-0164 / ARN-215).
    /// Own namespace is always allowed without a grant.
    cross_namespace_grants: std::sync::Mutex<std::collections::BTreeSet<String>>,
}

impl ActorContext {
    /// Create a new context for an activation.
    pub(crate) fn new(
        self_handle: ActorHandle,
        mailbox: Option<std::sync::Arc<dyn crate::mailbox::Mailbox>>,
        pool: Option<deadpool_postgres::Pool>,
    ) -> Self {
        Self {
            self_handle,
            pending_tells: Mutex::new(Vec::new()),
            mailbox,
            pool,
            cross_namespace_grants: std::sync::Mutex::new(std::collections::BTreeSet::new()),
        }
    }

    /// Grant cross-namespace load/upsert for a same-tenant namespace (ARN-215).
    ///
    /// Own namespace is always allowed without a grant. Grants are additive.
    ///
    /// # Security contract
    ///
    /// Grants are **type-enforced to the caller's tenant root** (first path
    /// segment of `self_handle.namespace`). A handler cannot self-grant into
    /// another tenant's namespace tree. Path separators, `..`, NUL, and empty
    /// strings are rejected. Untrusted payload segments (e.g. `process_id`)
    /// must still be validated by the caller before composition — see
    /// `temper-agents` process-id checks.
    ///
    /// Returns [`ActorError::NamespaceDenied`] when the grant would cross
    /// tenant roots or fails structural validation.
    pub fn grant_cross_namespace(&self, namespace: impl Into<String>) -> Result<(), ActorError> {
        let ns = namespace.into();
        self.validate_grant_target(&ns)?;
        // Recover poison so a prior panic does not silently drop later grants.
        self.cross_namespace_grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(ns);
        Ok(())
    }

    /// Reject grants outside the actor's tenant root or with path traversal.
    fn validate_grant_target(&self, namespace: &str) -> Result<(), ActorError> {
        if namespace.is_empty()
            || namespace.contains('\0')
            || namespace.contains('\\')
            || namespace.contains("..")
            || namespace.starts_with('/')
        {
            return Err(ActorError::NamespaceDenied(format!(
                "invalid grant namespace '{namespace}'"
            )));
        }
        let self_root = self.self_handle.namespace.split('/').next().unwrap_or("");
        let grant_root = namespace.split('/').next().unwrap_or("");
        if self_root.is_empty() || grant_root != self_root {
            return Err(ActorError::NamespaceDenied(format!(
                "actor '{}' cannot grant namespace '{}' outside tenant root '{}'",
                self.self_handle.namespace, namespace, self_root
            )));
        }
        Ok(())
    }

    /// This actor's own address.
    pub fn self_handle(&self) -> &ActorHandle {
        &self.self_handle
    }

    /// Fail closed unless `namespace` is self or explicitly granted (ARN-215).
    fn require_namespace_access(&self, namespace: &str) -> Result<(), ActorError> {
        if namespace == self.self_handle.namespace {
            return Ok(());
        }
        let granted = self
            .cross_namespace_grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(namespace);
        if granted {
            return Ok(());
        }
        Err(ActorError::NamespaceDenied(format!(
            "actor '{}' cannot access namespace '{}' without an explicit grant",
            self.self_handle.namespace, namespace
        )))
    }

    /// Load raw state bytes for an actor instance.
    ///
    /// Cross-namespace reads require a prior [`grant_cross_namespace`].
    pub async fn load_actor_state(
        &self,
        namespace: &str,
        actor_type: &str,
    ) -> Result<Option<Vec<u8>>, ActorError> {
        self.require_namespace_access(namespace)?;
        let Some(pool) = &self.pool else {
            return Err(ActorError::Internal("no pool in ActorContext".into()));
        };
        let client = pool
            .get()
            .await
            .map_err(|e| ActorError::Internal(format!("pool: {e}")))?;
        let rows = client
            .query(
                "SELECT state FROM odp_temper.actor_instances WHERE namespace = $1 AND actor_type = $2",
                &[&namespace, &actor_type],
            )
            .await
            .map_err(|e| ActorError::Internal(format!("load actor state: {e}")))?;
        Ok(rows.first().map(|row| row.get::<_, Vec<u8>>("state")))
    }

    /// Best-effort spawn/update of an actor instance with explicit state bytes.
    /// Used by integrations to persist auxiliary entities (e.g. Message).
    ///
    /// Cross-namespace writes require a prior [`grant_cross_namespace`].
    pub async fn upsert_actor_state(
        &self,
        namespace: &str,
        actor_type: &str,
        state: Vec<u8>,
    ) -> Result<(), ActorError> {
        self.require_namespace_access(namespace)?;
        let Some(pool) = &self.pool else {
            return Err(ActorError::Internal("no pool in ActorContext".into()));
        };
        let client = pool
            .get()
            .await
            .map_err(|e| ActorError::Internal(format!("pool: {e}")))?;
        client
            .execute(
                "INSERT INTO odp_temper.actor_instances (namespace, actor_type, state) VALUES ($1, $2, $3)
                 ON CONFLICT (namespace, actor_type) DO UPDATE SET state = EXCLUDED.state",
                &[&namespace, &actor_type, &state],
            )
            .await
            .map_err(|e| ActorError::Internal(format!("upsert actor state: {e}")))?;
        Ok(())
    }

    /// Fire-and-forget message. Buffered — only committed to PG when the
    /// activation transaction succeeds. If handle() fails, tells are discarded.
    /// Message type is auto-derived from the proto type name.
    pub async fn tell<M: prost::Message>(&self, to: &ActorHandle, msg: M) {
        self.pending_tells.lock().await.push(BufferedTell {
            to: to.clone(),
            message_type: type_name_of::<M>(),
            payload: msg.encode_to_vec(),
            correlation_id: None,
        });
    }

    /// Reply to an ask() message. Convenience for tell() with correlation_id set.
    pub async fn reply<M: prost::Message>(&self, original: &Message, msg: M) {
        if let Some(cid) = original.correlation_id {
            self.pending_tells.lock().await.push(BufferedTell {
                to: original
                    .from
                    .clone()
                    .unwrap_or_else(|| self.self_handle.clone()),
                message_type: type_name_of::<M>(),
                payload: msg.encode_to_vec(),
                correlation_id: Some(cid),
            });
        }
    }

    /// Request-response. Sends a message immediately and blocks until a reply
    /// arrives (or timeout). This is NOT transactional — if handle() fails
    /// after ask(), the ask side-effects are not rolled back.
    ///
    /// Prefer tell() + callback pattern for transactional safety.
    pub async fn ask<M: prost::Message>(
        &self,
        to: &ActorHandle,
        msg: M,
        timeout: Duration,
    ) -> Result<Message, ActorError> {
        let mailbox = self
            .mailbox
            .as_ref()
            .ok_or_else(|| ActorError::Internal("no mailbox in context".into()))?;
        mailbox
            .ask(
                &self.self_handle,
                to,
                &type_name_of::<M>(),
                msg.encode_to_vec(),
                timeout,
            )
            .await
            .map_err(|e| ActorError::MailboxError(e.to_string()))
    }

    /// Spawn a new actor instance in the same session.
    /// The actor's initial state comes from the handler's initial_state()
    /// method when the activator first processes it — not from the caller.
    pub async fn spawn(&self, actor_type: &str) -> Result<ActorHandle, ActorError> {
        let pool = self
            .pool
            .as_ref()
            .ok_or_else(|| ActorError::Internal("no pool in context".into()))?;
        let client = pool
            .get()
            .await
            .map_err(|e| ActorError::Internal(format!("pool: {e}")))?;

        let handle = ActorHandle::new(self.self_handle.namespace.clone(), actor_type);

        // Insert actor instance with empty state (ignore conflict if already exists).
        client
            .execute(
                "INSERT INTO odp_temper.actor_instances (namespace, actor_type, state) \
                 VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
                &[&handle.namespace, &handle.actor_type, &Vec::<u8>::new()],
            )
            .await
            .map_err(|e| ActorError::Internal(format!("spawn: {e}")))?;

        Ok(handle)
    }

    /// Look up a sibling actor in the same session.
    pub async fn lookup(&self, actor_type: &str) -> Result<Option<ActorHandle>, ActorError> {
        let pool = self
            .pool
            .as_ref()
            .ok_or_else(|| ActorError::Internal("no pool in context".into()))?;
        let client = pool
            .get()
            .await
            .map_err(|e| ActorError::Internal(format!("pool: {e}")))?;

        let rows = client
            .query(
                "SELECT 1 FROM odp_temper.actor_instances WHERE namespace = $1 AND actor_type = $2",
                &[&self.self_handle.namespace.clone(), &actor_type],
            )
            .await
            .map_err(|e| ActorError::Internal(format!("lookup: {e}")))?;

        if rows.is_empty() {
            Ok(None)
        } else {
            Ok(Some(ActorHandle::new(
                self.self_handle.namespace.clone(),
                actor_type,
            )))
        }
    }

    /// Take the buffered tell messages (consumed by the activator on commit).
    pub(crate) async fn take_pending_tells(&self) -> Vec<BufferedTell> {
        self.pending_tells.lock().await.drain(..).collect()
    }
}

/// The core actor trait.
///
/// Actors receive messages one at a time, update their state, and send
/// messages to other actors via the context.
#[async_trait::async_trait]
pub trait Actor: Send + Sync + 'static {
    /// Unique type name for this actor (e.g., "Session", "ToolRunner").
    fn actor_type(&self) -> &str;

    /// Initial state for a new actor instance (serialized bytes).
    fn initial_state(&self) -> Vec<u8> {
        vec![]
    }

    /// Handle a single message.
    ///
    /// Use `ctx.tell()` for fire-and-forget (buffered, transactional).
    /// Use `ctx.ask()` for request-response (immediate, NOT transactional).
    /// Use `ctx.reply()` to respond to an ask() caller.
    ///
    /// State is `&mut Vec<u8>` — the actor owns serialization/deserialization.
    /// The runtime persists the state bytes after handle returns Ok(()).
    ///
    /// One message at a time (classic actor model). FIFO order.
    ///
    /// **Important**: handle() must be idempotent. On transient failures,
    /// the runtime may retry the entire activation.
    async fn handle(
        &self,
        ctx: &ActorContext,
        state: &mut Vec<u8>,
        message: &Message,
    ) -> Result<(), ActorError>;
}

#[cfg(test)]
mod namespace_cap_tests {
    use super::*;

    #[test]
    fn same_namespace_allowed_without_grant() {
        let ctx = ActorContext::new(ActorHandle::new("ns-a", "Process"), None, None);
        assert!(ctx.require_namespace_access("ns-a").is_ok());
    }

    #[test]
    fn cross_namespace_denied_without_grant() {
        let ctx = ActorContext::new(ActorHandle::new("ns-a", "Process"), None, None);
        let err = ctx.require_namespace_access("ns-b").unwrap_err();
        assert!(matches!(err, ActorError::NamespaceDenied(_)));
    }

    #[test]
    fn same_tenant_child_allowed_after_grant() {
        let ctx = ActorContext::new(ActorHandle::new("ns-a", "Process"), None, None);
        ctx.grant_cross_namespace("ns-a/child")
            .expect("same-tenant child grant");
        assert!(ctx.require_namespace_access("ns-a/child").is_ok());
    }

    #[test]
    fn grant_rejects_other_tenant_root() {
        let ctx = ActorContext::new(ActorHandle::new("ns-a", "Process"), None, None);
        let err = ctx.grant_cross_namespace("ns-b").unwrap_err();
        assert!(matches!(err, ActorError::NamespaceDenied(_)));
        assert!(ctx.require_namespace_access("ns-b").is_err());
    }

    #[test]
    fn grant_rejects_path_traversal() {
        let ctx = ActorContext::new(ActorHandle::new("tenant/proc", "Process"), None, None);
        assert!(ctx.grant_cross_namespace("tenant/../other").is_err());
        assert!(ctx.grant_cross_namespace("").is_err());
        assert!(ctx.grant_cross_namespace("/tenant/x").is_err());
    }
}
