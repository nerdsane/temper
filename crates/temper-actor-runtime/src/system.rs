//! ActorSystem — the top-level API for the actor runtime.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use deadpool_postgres::Pool;

use crate::actor::{Actor, ActorError, ActorHandle, Message};
use crate::mailbox::{Mailbox, MailboxError};
use crate::pg::{PgActorActivator, PgMailbox, PgMailboxConfig};
use crate::scheduler::{Scheduler, SchedulerConfig};
use crate::schema;

/// The actor system. Entry point for all actor operations.
pub struct ActorSystem {
    pool: Pool,
    mailbox: Arc<dyn Mailbox>,
    activator: PgActorActivator,
    handlers: Arc<RwLock<HashMap<String, Arc<dyn Actor>>>>,
    scheduler_config: SchedulerConfig,
}

impl ActorSystem {
    /// Create a new actor system backed by PG.
    pub fn new(pool: Pool, scheduler_config: SchedulerConfig) -> Self {
        let mailbox = Arc::new(PgMailbox::new(pool.clone(), PgMailboxConfig::default()));
        let activator = PgActorActivator::new(pool.clone(), mailbox.clone());
        Self {
            pool,
            mailbox,
            activator,
            handlers: Arc::new(RwLock::new(HashMap::new())),
            scheduler_config,
        }
    }

    /// Register an actor implementation. Persists the type to PG and
    /// stores the handler in-memory.
    pub async fn register(&self, actor: Arc<dyn Actor>) -> Result<(), ActorError> {
        let actor_type = actor.actor_type().to_string();

        // Persist to PG.
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| ActorError::Internal(format!("pool: {e}")))?;
        client
            .execute(schema::REGISTER_ACTOR_TYPE, &[&actor_type])
            .await
            .map_err(|e| ActorError::Internal(format!("register: {e}")))?;

        // Store handler in-memory.
        let mut handlers = self.handlers.write().unwrap();
        handlers.insert(actor_type, actor);

        Ok(())
    }

    /// Spawn an actor instance for a session.
    pub async fn spawn(
        &self,
        namespace: &str,
        actor_type: &str,
    ) -> Result<ActorHandle, ActorError> {
        let handle = ActorHandle::new(namespace, actor_type);

        let initial_state = {
            let handlers = self.handlers.read().unwrap();
            handlers
                .get(actor_type)
                .map(|h| h.initial_state())
                .unwrap_or_default()
        };

        let client = self
            .pool
            .get()
            .await
            .map_err(|e| ActorError::Internal(format!("pool: {e}")))?;

        client
            .execute(
                schema::CREATE_ACTOR,
                &[&handle.namespace, &handle.actor_type, &initial_state],
            )
            .await
            .map_err(|e| ActorError::Internal(format!("spawn: {e}")))?;

        Ok(handle)
    }

    /// Send a fire-and-forget message from outside the actor system.
    pub async fn tell<M: prost::Message>(
        &self,
        from: Option<&ActorHandle>,
        to: &ActorHandle,
        msg: M,
    ) -> Result<i64, MailboxError> {
        let message_type = {
            let full = std::any::type_name::<M>();
            full.rsplit("::").next().unwrap_or(full)
        };
        self.mailbox
            .tell(from, to, message_type, msg.encode_to_vec())
            .await
    }

    /// Send a message and wait for a response from outside the actor system.
    pub async fn ask<M: prost::Message>(
        &self,
        from: &ActorHandle,
        to: &ActorHandle,
        msg: M,
        timeout: Duration,
    ) -> Result<Message, MailboxError> {
        let message_type = {
            let full = std::any::type_name::<M>();
            full.rsplit("::").next().unwrap_or(full)
        };
        self.mailbox
            .ask(from, to, message_type, msg.encode_to_vec(), timeout)
            .await
    }

    /// Run the scheduler loop. Blocks until cancelled.
    pub async fn run(&self, cancel: tokio::sync::watch::Receiver<bool>) {
        let scheduler = Scheduler::new(
            self.pool.clone(),
            self.activator.clone(),
            self.handlers.clone(),
            self.scheduler_config.clone(),
        );
        scheduler.run(cancel).await;
    }

    /// Run a single poll cycle (for testing).
    pub async fn poll_once(&self) -> Result<usize, anyhow::Error> {
        let scheduler = Scheduler::new(
            self.pool.clone(),
            self.activator.clone(),
            self.handlers.clone(),
            self.scheduler_config.clone(),
        );
        scheduler.poll_once().await
    }

    /// Spawn all registered actor types in a namespace (idempotent — skips existing instances).
    pub async fn spawn_all_registered(&self, namespace: &str) -> Result<(), ActorError> {
        let actor_types: Vec<String> = { self.handlers.read().unwrap().keys().cloned().collect() };
        for actor_type in &actor_types {
            // ON CONFLICT DO NOTHING — idempotent.
            let client = self
                .pool
                .get()
                .await
                .map_err(|e| ActorError::Internal(format!("pool: {e}")))?;
            let initial_state = {
                let handlers = self.handlers.read().unwrap();
                handlers
                    .get(actor_type.as_str())
                    .map(|h| h.initial_state())
                    .unwrap_or_default()
            };
            client
                .execute(
                    crate::schema::CREATE_ACTOR,
                    &[
                        &namespace.to_string(),
                        &actor_type.to_string(),
                        &initial_state,
                    ],
                )
                .await
                .map_err(|e| ActorError::Internal(format!("spawn {actor_type}: {e}")))?;
        }
        Ok(())
    }

    /// Get a reference to the mailbox (for advanced use).
    pub fn mailbox(&self) -> &Arc<dyn Mailbox> {
        &self.mailbox
    }

    /// Load actor state bytes. Returns None if actor instance doesn't exist.
    pub async fn load_state(
        &self,
        namespace: &str,
        actor_type: &str,
    ) -> Result<Option<Vec<u8>>, ActorError> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|e| ActorError::Internal(format!("pool: {e}")))?;

        let rows = client
            .query(crate::schema::LOAD_ACTOR, &[&namespace, &actor_type])
            .await
            .map_err(|e| ActorError::Internal(format!("load state: {e}")))?;

        Ok(rows.first().map(|row| row.get::<_, Vec<u8>>("state")))
    }

    /// Load and deserialize spec actor state. Returns None if not found.
    pub async fn get_spec_actor_state(
        &self,
        handle: &ActorHandle,
    ) -> Option<crate::spec_actor::SpecActorState> {
        let bytes = self
            .load_state(&handle.namespace, &handle.actor_type)
            .await
            .ok()??;
        serde_json::from_slice(&bytes).ok()
    }
}
