//! Actor scheduler — polls mailboxes and activates actors.
//!
//! Activations are fire-and-forget (tokio::spawn). An in-flight set
//! tracks which actors are currently being activated, preventing
//! redundant dispatches. The PG advisory lock is the real safety net
//! (prevents double-execution even if the in-flight set has a race).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use deadpool_postgres::Pool;
use tracing::{debug, error, info, warn};

use crate::actor::{Actor, ActorHandle};
use crate::pg::{ActivationError, PgActorActivator};
use crate::schema;

/// Scheduler configuration.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// How often to poll for pending work (default: 100ms).
    pub poll_interval: Duration,
    /// Max actors to check per poll cycle (default: 50).
    pub batch_size: i64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(100),
            batch_size: 50,
        }
    }
}

/// The actor scheduler.
pub struct Scheduler {
    pool: Pool,
    activator: Arc<PgActorActivator>,
    handlers: Arc<RwLock<HashMap<String, Arc<dyn Actor>>>>,
    in_flight: Arc<tokio::sync::Mutex<HashSet<String>>>,
    config: SchedulerConfig,
}

impl Scheduler {
    pub fn new(
        pool: Pool,
        activator: PgActorActivator,
        handlers: Arc<RwLock<HashMap<String, Arc<dyn Actor>>>>,
        config: SchedulerConfig,
    ) -> Self {
        Self {
            pool,
            activator: Arc::new(activator),
            handlers,
            in_flight: Arc::new(tokio::sync::Mutex::new(HashSet::new())),
            config,
        }
    }

    /// Run the scheduler loop. Blocks until cancelled.
    pub async fn run(&self, cancel: tokio::sync::watch::Receiver<bool>) {
        info!("actor scheduler starting");

        loop {
            if *cancel.borrow() {
                info!("actor scheduler shutting down");
                break;
            }

            match self.poll_once().await {
                Ok(_) => {}
                Err(e) => {
                    error!(error = %e, "scheduler poll error");
                }
            };

            tokio::time::sleep(self.config.poll_interval).await;
        }
    }

    /// Single poll cycle. Dispatches activations for actors with pending
    /// messages, skipping any that are already in-flight.
    /// Returns number of NEW activations dispatched.
    pub async fn poll_once(&self) -> Result<usize, anyhow::Error> {
        let client = self.pool.get().await?;

        let promoted = client
            .execute(schema::PROMOTE_DUE_MESSAGES, &[&self.config.batch_size])
            .await?;
        if promoted > 0 {
            debug!(promoted, "promoted due actor messages");
        }

        let rows = client
            .query(schema::FIND_PENDING_ACTORS, &[&self.config.batch_size])
            .await?;

        let mut dispatched = 0;

        for row in &rows {
            let namespace: String = row.get("namespace");
            let actor_type: String = row.get("actor_type");
            let key = format!("{namespace}:{actor_type}");

            // Skip if already in-flight.
            {
                let in_flight = self.in_flight.lock().await;
                if in_flight.contains(&key) {
                    continue;
                }
            }

            let handler: Arc<dyn Actor> = {
                let handlers = self.handlers.read().unwrap();
                match handlers.get(&actor_type) {
                    Some(h) => h.clone(),
                    None => {
                        warn!(actor_type = %actor_type, "no handler registered, skipping");
                        continue;
                    }
                }
            };

            // Mark in-flight before spawning.
            {
                let mut in_flight = self.in_flight.lock().await;
                in_flight.insert(key.clone());
            }

            let handle = ActorHandle::new(namespace, &actor_type);
            let activator = self.activator.clone();
            let in_flight = self.in_flight.clone();

            tokio::spawn(async move {
                let result = activator.activate(&handle, handler.as_ref()).await;

                match &result {
                    Ok(r) if r.activated => {
                        debug!(actor = %handle, "activated");
                    }
                    Ok(_) => {}
                    Err(ActivationError::ActorError(e)) => {
                        warn!(actor = %handle, error = %e, "actor handler error");
                    }
                    Err(e) => {
                        warn!(actor = %handle, error = %e, "activation error");
                    }
                }

                // Remove from in-flight when done (success or failure).
                in_flight.lock().await.remove(&key);
            });

            dispatched += 1;
        }

        if dispatched > 0 {
            debug!(dispatched = dispatched, "poll cycle dispatched");
        }

        Ok(dispatched)
    }
}
