//! Platform state shared across all handlers.
//!
//! [`PlatformState`] extends `ServerState` with the broadcast channel
//! for internal event propagation, evolution record storage, and the
//! Claude API key for agentic evolution.

use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;

use temper_evolution::RecordStore;
use temper_runtime::ActorSystem;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;

use crate::protocol::PlatformEvent;
use crate::spec_store::SpecStore;

/// Shared state for the Temper hosting platform.
///
/// Wraps `ServerState` and adds platform-specific facilities:
/// broadcast channel for internal event propagation, evolution records,
/// and the Claude API key for agentic evolution agents.
#[derive(Clone)]
pub struct PlatformState {
    /// The underlying server state (OData routing, actor dispatch).
    pub server: ServerState,
    /// Multi-tenant specification registry (mutable for live registration).
    pub registry: Arc<RwLock<SpecRegistry>>,
    /// Broadcast sender for platform events (deploy, verify, evolution, etc.).
    pub broadcast_tx: broadcast::Sender<PlatformEvent>,
    /// Evolution record store.
    pub record_store: RecordStore,
    /// Anthropic API key for Claude-powered evolution agents.
    pub api_key: Option<String>,
    /// Bearer token for API authentication (`TEMPER_API_KEY`).
    pub api_token: Option<String>,
    /// In-memory spec storage for pending tenant deployments.
    pub spec_store: Arc<RwLock<SpecStore>>,
}

/// Default broadcast channel capacity.
const BROADCAST_CAPACITY: usize = 256;

impl PlatformState {
    /// Create a new platform state with an empty registry.
    pub fn new(api_key: Option<String>) -> Self {
        let system = ActorSystem::new("temper-platform");
        let registry = Arc::new(RwLock::new(SpecRegistry::new()));
        let server = ServerState::from_registry_shared(system, registry.clone());
        let (broadcast_tx, _) = broadcast::channel(BROADCAST_CAPACITY);

        Self {
            server,
            registry,
            broadcast_tx,
            record_store: RecordStore::new(),
            api_key,
            api_token: None,
            spec_store: Arc::new(RwLock::new(SpecStore::new())),
        }
    }

    /// Create a new platform state with a pre-loaded registry.
    pub fn with_registry(registry: SpecRegistry, api_key: Option<String>) -> Self {
        let system = ActorSystem::new("temper-platform");
        let registry = Arc::new(RwLock::new(registry));
        let server = ServerState::from_registry_shared(system, registry.clone());
        let (broadcast_tx, _) = broadcast::channel(BROADCAST_CAPACITY);

        Self {
            server,
            registry,
            broadcast_tx,
            record_store: RecordStore::new(),
            api_key,
            api_token: None,
            spec_store: Arc::new(RwLock::new(SpecStore::new())),
        }
    }

    /// Subscribe to the broadcast channel for platform events.
    pub fn subscribe(&self) -> broadcast::Receiver<PlatformEvent> {
        self.broadcast_tx.subscribe()
    }

    /// Broadcast a platform event to all subscribers.
    pub fn broadcast(&self, event: PlatformEvent) {
        // Ignore send errors (no active receivers).
        let _ = self.broadcast_tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_state() {
        let state = PlatformState::new(None);
        assert!(state.api_key.is_none());
    }

    #[test]
    fn test_new_with_api_key() {
        let state = PlatformState::new(Some("sk-test-key".into()));
        assert_eq!(state.api_key.as_deref(), Some("sk-test-key"));
    }

    #[test]
    fn test_with_registry() {
        let registry = SpecRegistry::new();
        let state = PlatformState::with_registry(registry, None);
        assert!(state.api_key.is_none());
    }

    #[test]
    fn test_broadcast_and_subscribe() {
        let state = PlatformState::new(None);
        let mut rx = state.subscribe();

        state.broadcast(PlatformEvent::Error {
            message: "test broadcast".into(),
        });

        let received = rx.try_recv().unwrap();
        match received {
            PlatformEvent::Error { message } => assert_eq!(message, "test broadcast"),
            _ => panic!("expected Error message"),
        }
    }

    #[test]
    fn test_broadcast_no_receivers_does_not_panic() {
        let state = PlatformState::new(None);
        // No subscribers — should not panic.
        state.broadcast(PlatformEvent::Error {
            message: "nobody listening".into(),
        });
    }

    #[test]
    fn test_multiple_subscribers() {
        let state = PlatformState::new(None);
        let mut rx1 = state.subscribe();
        let mut rx2 = state.subscribe();

        state.broadcast(PlatformEvent::TenantRegistered {
            tenant: "test".into(),
            entity_count: 1,
        });

        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());
    }
}
