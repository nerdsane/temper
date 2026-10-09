//! Shared harness for factory regression tests.
//!
//! Boots the reference ecommerce specs on a real `ServerState` +
//! `build_router` over a deterministic `SimEventStore`. A "restart" builds a
//! fresh `ServerState` (fresh actor system, fresh idempotency cache) over the
//! same store, so the next request rehydrates the actor from the journal.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

pub const CSDL_XML: &str =
    include_str!("../../../../reference-apps/ecommerce/specs/model.csdl.xml");
pub const ORDER_IOA: &str =
    include_str!("../../../../reference-apps/ecommerce/specs/order.ioa.toml");
pub const PAYMENT_IOA: &str =
    include_str!("../../../../reference-apps/ecommerce/specs/payment.ioa.toml");
pub const SHIPMENT_IOA: &str =
    include_str!("../../../../reference-apps/ecommerce/specs/shipment.ioa.toml");

/// Tenants registered by the harness.
pub const TENANT_A: &str = "default";
pub const TENANT_B: &str = "tenant-b";

/// The only principal the harness policy permits.
pub const TESTER: &str = "idem-tester";
/// An authenticated principal the harness policy does not permit.
pub const INTRUDER: &str = "idem-intruder";

/// Cedar policy scoped to the test principal; everyone else is denied.
const POLICY: &str = r#"
permit(
  principal,
  action,
  resource
) when {
  principal.id == "idem-tester"
};
"#;

/// Which idempotency path a retry exercises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Same server process: the HTTP / actor idempotency cache is warm.
    WarmHttpCache,
    /// A fresh server over the same store: caches are cold and the actor
    /// rehydrates from the journal (durable path).
    ColdRestart,
}

/// One HTTP response, decoded.
#[derive(Clone, Debug)]
pub struct Reply {
    pub status: StatusCode,
    pub body: Value,
}

impl Reply {
    /// OData error code, if this is an error body.
    pub fn error_code(&self) -> Option<&str> {
        self.body
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(Value::as_str)
    }
}

/// A booted server over a shared sim store.
pub struct Harness {
    pub store: SimEventStore,
    pub state: ServerState,
}

fn verified() -> VerificationStatus {
    VerificationStatus::Completed(EntityVerificationResult {
        all_passed: true,
        levels: vec![EntityLevelSummary {
            level: "L0 SMT".to_string(),
            passed: true,
            summary: "OK".to_string(),
            details: None,
        }],
        verified_at: "2026-10-06T00:00:00Z".to_string(),
    })
}

/// Build a `ServerState` over `store` with the ecommerce specs in both tenants.
pub fn build_state(store: &SimEventStore, name: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    for tenant in [TENANT_A, TENANT_B] {
        let csdl =
            parse_csdl(CSDL_XML).unwrap_or_else(|e| panic!("harness setup: CSDL parse: {e}"));
        registry.register_tenant(
            tenant,
            csdl,
            CSDL_XML.to_string(),
            &[
                ("Order", ORDER_IOA),
                ("Payment", PAYMENT_IOA),
                ("Shipment", SHIPMENT_IOA),
            ],
        );
        for entity in ["Order", "Payment", "Shipment"] {
            registry.set_verification_status(&TenantId::new(tenant), entity, verified());
        }
    }
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    for tenant in [TENANT_A, TENANT_B] {
        state
            .authz
            .reload_tenant_policies(tenant, POLICY)
            .unwrap_or_else(|e| panic!("harness setup: policy load for {tenant}: {e}"));
    }
    state
}

impl Harness {
    /// Fresh store + server.
    pub fn new(seed: u64) -> Self {
        let store = SimEventStore::no_faults(seed);
        let state = build_state(&store, "factory-regression");
        Self { store, state }
    }

    /// Wrap an existing store (e.g. a legacy-format copy) in a fresh server.
    pub fn over(store: SimEventStore) -> Self {
        let state = build_state(&store, "factory-regression-over");
        Self { store, state }
    }

    /// Drop all in-memory state (actors + idempotency caches) and boot a new
    /// server over the same journal.
    pub fn restart(&mut self) {
        self.state = build_state(&self.store, "factory-regression-restarted");
    }

    /// Restart only in `ColdRestart` mode.
    pub fn maybe_restart(&mut self, mode: Mode) {
        if mode == Mode::ColdRestart {
            self.restart();
        }
    }

    /// Create a Draft order through the dispatch layer.
    pub async fn create_order(&self, tenant: &str, id: &str) {
        self.state
            .get_or_create_tenant_entity(&TenantId::new(tenant), "Order", id, json!({"Id": id}))
            .await
            .unwrap_or_else(|e| panic!("harness setup: create order {tenant}/{id}: {e}"));
    }

    /// Send a request through the real router.
    pub async fn send(&self, req: Request<Body>) -> Reply {
        let resp = build_router(self.state.clone())
            .oneshot(req)
            .await
            .unwrap_or_else(|e| panic!("harness setup: router call failed: {e}"));
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 4_000_000)
            .await
            .unwrap_or_else(|e| panic!("harness setup: body read failed: {e}"));
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Reply { status, body }
    }

    /// POST a bound action as the test principal.
    pub async fn action(
        &self,
        tenant: &str,
        id: &str,
        qualified_action: &str,
        key: Option<&str>,
        body: &str,
    ) -> Reply {
        self.send(action_request(
            tenant,
            id,
            qualified_action,
            key,
            body,
            &[],
            Some(TESTER),
        ))
        .await
    }

    /// GET a single order as the test principal.
    pub async fn get_order(&self, tenant: &str, id: &str) -> Reply {
        self.send(authenticated(
            Request::get(format!("/tdata/Orders('{id}')"))
                .body(Body::empty())
                .unwrap_or_else(|e| panic!("harness setup: request build: {e}")),
            tenant,
            TESTER,
        ))
        .await
    }

    /// GET the Orders catalog as the test principal.
    pub async fn list_orders(&self, tenant: &str) -> Reply {
        self.send(authenticated(
            Request::get("/tdata/Orders")
                .body(Body::empty())
                .unwrap_or_else(|e| panic!("harness setup: request build: {e}")),
            tenant,
            TESTER,
        ))
        .await
    }

    /// Journal for one order.
    pub fn journal(
        &self,
        tenant: &str,
        id: &str,
    ) -> Vec<temper_runtime::persistence::PersistenceEnvelope> {
        self.store.dump_journal(&persistence_id(tenant, id))
    }

    /// Journal length for one order.
    pub fn journal_len(&self, tenant: &str, id: &str) -> usize {
        self.journal(tenant, id).len()
    }
}

/// `{tenant}:Order:{id}`.
pub fn persistence_id(tenant: &str, id: &str) -> String {
    format!("{tenant}:Order:{id}")
}

/// Attach the typed authenticated context the platform middleware would set.
pub fn authenticated(mut req: Request<Body>, tenant: &str, principal: &str) -> Request<Body> {
    req.extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            SecurityContext::from_resolved_identity(principal, "test-agent", None),
        ));
    req
}

/// Build a bound-action request. `principal: None` sends it unauthenticated.
pub fn action_request(
    tenant: &str,
    id: &str,
    qualified_action: &str,
    key: Option<&str>,
    body: &str,
    extra_headers: &[(&str, &str)],
    principal: Option<&str>,
) -> Request<Body> {
    let mut builder = Request::post(format!("/tdata/Orders('{id}')/{qualified_action}"))
        .header("Content-Type", "application/json");
    if let Some(key) = key {
        builder = builder.header("Idempotency-Key", key);
    }
    for (name, value) in extra_headers {
        builder = builder.header(*name, *value);
    }
    let req = builder
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|e| panic!("harness setup: request build: {e}"));
    match principal {
        Some(p) => authenticated(req, tenant, p),
        None => req,
    }
}

/// The "logical response" of an entity body (ADR-0182 planned contract):
/// the entity JSON minus top-level `@odata.*` annotations, minus snapshot
/// bookkeeping (`events_since_snapshot`, `last_snapshot_sequence_nr`) and
/// minus the server-internal per-event `idempotency_binding` hash.
pub fn logical(body: &Value) -> Value {
    let mut body = body.clone();
    if let Some(obj) = body.as_object_mut() {
        obj.retain(|k, _| !k.starts_with("@odata."));
        obj.remove("events_since_snapshot");
        obj.remove("last_snapshot_sequence_nr");
        if let Some(Value::Array(events)) = obj.get_mut("events") {
            for event in events {
                if let Some(event) = event.as_object_mut() {
                    event.remove("idempotency_binding");
                }
            }
        }
    }
    body
}

/// `counters.items` from an entity body.
pub fn items(body: &Value) -> Option<u64> {
    body.get("counters")
        .and_then(|c| c.get("items"))
        .and_then(Value::as_u64)
}

/// `status` from an entity body.
pub fn status(body: &Value) -> Option<&str> {
    body.get("status").and_then(Value::as_str)
}
