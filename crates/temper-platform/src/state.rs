//! Platform state shared across all handlers.
//!
//! [`PlatformState`] extends `ServerState` with the broadcast channel
//! for internal event propagation and the Claude API key.

use std::sync::{Arc, OnceLock, RwLock};

use tokio::sync::broadcast;

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;

use crate::module_review::{ModuleReviewer, ReviewsUnderWay};
use crate::protocol::PlatformEvent;
use crate::spec_store::SpecStore;

/// Shared state for the Temper hosting platform.
///
/// Wraps `ServerState` and adds platform-specific facilities:
/// broadcast channel for internal event propagation and the
/// Claude API key.
#[derive(Clone)]
pub struct PlatformState {
    /// The underlying server state (OData routing, actor dispatch).
    pub server: ServerState,
    /// Multi-tenant specification registry (mutable for live registration).
    pub registry: Arc<RwLock<SpecRegistry>>,
    /// Broadcast sender for platform events (deploy, verify, evolution, etc.).
    pub broadcast_tx: broadcast::Sender<PlatformEvent>,
    /// Anthropic API key for Claude-powered agents.
    pub api_key: Option<String>,
    /// Optional bootstrap token registered as a normal tenant credential.
    /// It has no direct runtime authentication fallback (ADR-0157).
    pub api_token: Option<String>,
    /// In-memory spec storage for pending tenant deployments.
    pub spec_store: Arc<RwLock<SpecStore>>,
    /// Jev's review of an app's modules at install, once the server is given one ([`Self::set_module_reviewer`]).
    /// Shared by every clone of the state, so hooks made with a clone before it was set review too.
    pub module_reviewer: Arc<OnceLock<Arc<ModuleReviewer>>>,
    /// The module reviews running in this process.
    pub module_reviews: Arc<ReviewsUnderWay>,
}

/// Default broadcast channel capacity.
const BROADCAST_CAPACITY: usize = 256;

/// Cedar policy for the temper-system tenant.
///
/// Platform entities (GovernanceDecision, etc.) live in the temper-system
/// tenant but have no os-app to ship policies with. This baseline policy
/// permits only a credential-verified, tenant-scoped operator Agent to manage
/// platform entities. The deployment key has no implicit Admin authority.
///
/// The operator predicate is generated from the shared
/// [`temper_authz::VERIFIED_OPERATOR_WHEN`] const rather than hand-written, so
/// it cannot drift from the verified-operator definition used by the
/// system-platform Cedar policy. Interpolating the const also brings the
/// `principal has …` attribute guards, so a principal missing `agent_type` /
/// `agentTypeVerified` is a clean deny rather than a Cedar evaluation error.
fn system_tenant_policy() -> String {
    format!(
        r#"
// Baseline policy for the temper-system tenant.
// Operators are ordinary tenant credentials. Both the registered AgentType and
// the credential-verification attribute must match.
permit(
  principal is Agent,
  action,
  resource
) when {{ {op} }};
"#,
        op = temper_authz::VERIFIED_OPERATOR_WHEN
    )
}

impl PlatformState {
    /// Create a new platform state with an empty registry.
    pub fn new(api_key: Option<String>) -> Self {
        let system = ActorSystem::new("temper-platform");
        let registry = Arc::new(RwLock::new(SpecRegistry::new()));
        let spec_store = Arc::new(RwLock::new(SpecStore::new()));
        let mut server = ServerState::from_registry_shared(system, registry.clone());
        // Register platform custom effect handler so hooks.rs is called
        // from the dispatch pipeline for system entity effects.
        server.custom_effect_handler = Some(Arc::new(crate::hooks::PlatformEffectHandler {
            spec_store: spec_store.clone(),
        }));
        // Load baseline Cedar policies for the temper-system tenant so that
        // WASM modules can manage GovernanceDecision entities via HTTP.
        if let Err(e) = server
            .authz
            .reload_tenant_policies("temper-system", &system_tenant_policy())
        {
            tracing::warn!(error = %e, "failed to load temper-system Cedar policies");
        }
        let (broadcast_tx, _) = broadcast::channel(BROADCAST_CAPACITY);

        let mut state = Self {
            server,
            registry,
            broadcast_tx,
            api_key,
            api_token: None,
            spec_store,
            module_reviewer: Arc::new(OnceLock::new()),
            module_reviews: Arc::default(),
        };
        state.server.bound_action_hook = Some(Arc::new(
            crate::genesis_install::GenesisInstallHook::new(state.clone()),
        ));
        state
    }

    /// Why `tenant` is unavailable, if startup could not restore its stored
    /// specs. Such a tenant's stored rows are kept as they are for repair, so
    /// nothing installs or writes specs for it until a restart restores it.
    pub fn tenant_unavailable_reason(&self, tenant: &str) -> Option<String> {
        self.server
            .tenant_unavailable_reason(&TenantId::new(tenant))
    }

    /// Create a new platform state with a pre-loaded registry.
    pub fn with_registry(registry: SpecRegistry, api_key: Option<String>) -> Self {
        let system = ActorSystem::new("temper-platform");
        let registry = Arc::new(RwLock::new(registry));
        let spec_store = Arc::new(RwLock::new(SpecStore::new()));
        let mut server = ServerState::from_registry_shared(system, registry.clone());
        // Register platform custom effect handler so hooks.rs is called
        // from the dispatch pipeline for system entity effects.
        server.custom_effect_handler = Some(Arc::new(crate::hooks::PlatformEffectHandler {
            spec_store: spec_store.clone(),
        }));
        // Load baseline Cedar policies for the temper-system tenant so that
        // WASM modules can manage GovernanceDecision entities via HTTP.
        if let Err(e) = server
            .authz
            .reload_tenant_policies("temper-system", &system_tenant_policy())
        {
            tracing::warn!(error = %e, "failed to load temper-system Cedar policies");
        }
        let (broadcast_tx, _) = broadcast::channel(BROADCAST_CAPACITY);

        let mut state = Self {
            server,
            registry,
            broadcast_tx,
            api_key,
            api_token: None,
            spec_store,
            module_reviewer: Arc::new(OnceLock::new()),
            module_reviews: Arc::default(),
        };
        state.server.bound_action_hook = Some(Arc::new(
            crate::genesis_install::GenesisInstallHook::new(state.clone()),
        ));
        state
    }

    /// Review the modules of every app installed from now on with `reviewer`. The first one set stays.
    pub fn set_module_reviewer(&self, reviewer: Arc<ModuleReviewer>) {
        let _ = self.module_reviewer.set(reviewer);
        debug_assert!(self.module_reviewer.get().is_some());
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

    #[test]
    fn system_tenant_policy_operator_clause_matches_shared_predicate() {
        // ARN-255: the operator predicate must be generated from the shared
        // const, never hand-written, so the system-tenant baseline policy and
        // the system-platform Cedar policy cannot drift apart.
        let policy = system_tenant_policy();
        assert!(
            policy.contains(temper_authz::VERIFIED_OPERATOR_WHEN),
            "system-tenant policy must interpolate VERIFIED_OPERATOR_WHEN verbatim;\n\
             policy was:\n{policy}"
        );
        // Interpolating the const also carries the `has` attribute guards, so a
        // principal missing these attributes is a clean deny, not a Cedar
        // evaluation error.
        assert!(
            policy.contains("principal has agent_type")
                && policy.contains("principal has agentTypeVerified"),
            "generated policy must carry the `has` attribute guards"
        );
        // And the generated text must be valid Cedar.
        assert!(
            temper_authz::AuthzEngine::new(&policy).is_ok(),
            "generated system-tenant policy must parse as valid Cedar"
        );
    }
}
