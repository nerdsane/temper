//! Integration engine: receives events and dispatches integrations.

use std::sync::Arc;

use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::registry::IntegrationRegistry;
use super::types::{IntegrationEvent, IntegrationResult, IntegrationStatus};
use super::webhook::WebhookDispatcher;

/// Event-driven integration engine.
///
/// Receives integration events and dispatches them to registered webhook
/// endpoints. Runs as a background tokio task.
pub struct IntegrationEngine {
    /// Registry of trigger-to-integration-config mappings.
    registry: Arc<IntegrationRegistry>,
    /// Webhook HTTP dispatcher.
    dispatcher: Arc<WebhookDispatcher>,
    /// Channel for receiving integration events.
    rx: tokio::sync::mpsc::Receiver<IntegrationEvent>,
}

impl IntegrationEngine {
    /// Create a new integration engine.
    pub fn new(
        registry: IntegrationRegistry,
        rx: tokio::sync::mpsc::Receiver<IntegrationEvent>,
    ) -> Self {
        Self {
            registry: Arc::new(registry),
            dispatcher: Arc::new(WebhookDispatcher::new()),
            rx,
        }
    }

    /// Start the engine as a background task.
    ///
    /// Returns a join handle. The engine runs until the sender half is dropped.
    pub fn start(mut self) -> JoinHandle<()> {
        tokio::spawn(async move {
            info!("integration engine started");
            while let Some(event) = self.rx.recv().await {
                let configs = self.registry.lookup(&event.event_name);
                if configs.is_empty() {
                    debug!(
                        event = %event.event_name,
                        "no integrations registered for event"
                    );
                    continue;
                }

                for config in configs {
                    let dispatcher = Arc::clone(&self.dispatcher);
                    let config = config.clone();
                    let event = event.clone();

                    // Dispatch each integration concurrently.
                    tokio::spawn(async move {
                        let result = dispatcher.dispatch(&config, &event).await;
                        match &result.status {
                            IntegrationStatus::Success => {
                                info!(
                                    integration = %result.name,
                                    duration_ms = result.duration.as_millis(),
                                    "integration completed"
                                );
                            }
                            IntegrationStatus::Failed(err) => {
                                warn!(
                                    integration = %result.name,
                                    error = %err,
                                    duration_ms = result.duration.as_millis(),
                                    "integration failed permanently"
                                );
                            }
                            IntegrationStatus::Retrying { attempt } => {
                                debug!(
                                    integration = %result.name,
                                    attempt,
                                    "integration retrying"
                                );
                            }
                        }
                    });
                }
            }
            info!("integration engine stopped (sender dropped)");
        })
    }

    /// Process a single event synchronously (for testing).
    pub async fn process_event(
        registry: &IntegrationRegistry,
        dispatcher: &WebhookDispatcher,
        event: &IntegrationEvent,
    ) -> Vec<IntegrationResult> {
        let configs = registry.lookup(&event.event_name);
        let mut results = Vec::new();
        for config in configs {
            let result = dispatcher.dispatch(config, event).await;
            results.push(result);
        }
        results
    }
}
