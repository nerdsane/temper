//! Platform state shared across all handlers.
//!
//! [`PlatformState`] extends `ServerState` with the broadcast channel
//! for real-time WebSocket updates, evolution record storage, and
//! platform mode (developer vs production).

use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;

use temper_evolution::RecordStore;
use temper_runtime::ActorSystem;
use temper_server::registry::SpecRegistry;
use temper_server::ServerState;

use crate::protocol::WsMessage;

/// Platform operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformMode {
    /// Developer mode: interview agent, spec generation, verify-and-deploy.
    Dev,
    /// Production mode: operate within deployed specs, capture trajectories.
    Production,
}

/// Shared state for the conversational development platform.
///
/// Wraps `ServerState` and adds platform-specific facilities:
/// broadcast channel for real-time UI updates, evolution records,
/// and the Claude API key for LLM-powered agents.
#[derive(Clone)]
pub struct PlatformState {
    /// The underlying server state (OData routing, actor dispatch).
    pub server: ServerState,
    /// Multi-tenant specification registry (mutable for live registration).
    pub registry: Arc<RwLock<SpecRegistry>>,
    /// Broadcast sender for WebSocket updates (spec changes, verify status, etc.).
    pub broadcast_tx: broadcast::Sender<WsMessage>,
    /// Evolution record store.
    pub record_store: RecordStore,
    /// Anthropic API key for Claude-powered agents.
    pub api_key: Option<String>,
    /// Platform operating mode.
    pub mode: PlatformMode,
}

/// Default broadcast channel capacity.
const BROADCAST_CAPACITY: usize = 256;

impl PlatformState {
    /// Create a new platform state in developer mode with an empty registry.
    pub fn new_dev(api_key: Option<String>) -> Self {
        let system = ActorSystem::new("temper-platform-dev");
        let registry = SpecRegistry::new();
        let server = ServerState::from_registry(system, registry.clone());
        let (broadcast_tx, _) = broadcast::channel(BROADCAST_CAPACITY);

        Self {
            server,
            registry: Arc::new(RwLock::new(registry)),
            broadcast_tx,
            record_store: RecordStore::new(),
            api_key,
            mode: PlatformMode::Dev,
        }
    }

    /// Create a new platform state in production mode with a pre-loaded registry.
    pub fn new_production(registry: SpecRegistry, api_key: Option<String>) -> Self {
        let system = ActorSystem::new("temper-platform-prod");
        let server = ServerState::from_registry(system, registry.clone());
        let (broadcast_tx, _) = broadcast::channel(BROADCAST_CAPACITY);

        Self {
            server,
            registry: Arc::new(RwLock::new(registry)),
            broadcast_tx,
            record_store: RecordStore::new(),
            api_key,
            mode: PlatformMode::Production,
        }
    }

    /// Subscribe to the broadcast channel for real-time updates.
    pub fn subscribe(&self) -> broadcast::Receiver<WsMessage> {
        self.broadcast_tx.subscribe()
    }

    /// Broadcast a message to all connected WebSocket clients.
    pub fn broadcast(&self, msg: WsMessage) {
        // Ignore send errors (no active receivers).
        let _ = self.broadcast_tx.send(msg);
    }

    /// Whether the platform is in developer mode.
    pub fn is_dev(&self) -> bool {
        self.mode == PlatformMode::Dev
    }

    /// Whether the platform is in production mode.
    pub fn is_production(&self) -> bool {
        self.mode == PlatformMode::Production
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_dev_state() {
        let state = PlatformState::new_dev(None);
        assert!(state.is_dev());
        assert!(!state.is_production());
        assert!(state.api_key.is_none());
    }

    #[test]
    fn test_new_dev_with_api_key() {
        let state = PlatformState::new_dev(Some("sk-test-key".into()));
        assert!(state.is_dev());
        assert_eq!(state.api_key.as_deref(), Some("sk-test-key"));
    }

    #[test]
    fn test_new_production_state() {
        let registry = SpecRegistry::new();
        let state = PlatformState::new_production(registry, None);
        assert!(state.is_production());
        assert!(!state.is_dev());
    }

    #[test]
    fn test_broadcast_and_subscribe() {
        let state = PlatformState::new_dev(None);
        let mut rx = state.subscribe();

        state.broadcast(WsMessage::Error {
            message: "test broadcast".into(),
        });

        let received = rx.try_recv().unwrap();
        match received {
            WsMessage::Error { message } => assert_eq!(message, "test broadcast"),
            _ => panic!("expected Error message"),
        }
    }

    #[test]
    fn test_broadcast_no_receivers_does_not_panic() {
        let state = PlatformState::new_dev(None);
        // No subscribers — should not panic.
        state.broadcast(WsMessage::Error {
            message: "nobody listening".into(),
        });
    }

    #[test]
    fn test_multiple_subscribers() {
        let state = PlatformState::new_dev(None);
        let mut rx1 = state.subscribe();
        let mut rx2 = state.subscribe();

        state.broadcast(WsMessage::PhaseUpdate {
            phase: "Welcome".into(),
            progress: 0,
        });

        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());
    }
}
