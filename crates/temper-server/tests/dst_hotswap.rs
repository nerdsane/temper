//! DST hot-swap safety tests.
//!
//! Verifies that hot-swapping transition tables via `SwapController` is
//! safe while entities are live. The real `ServerState` dispatches through
//! the real `Arc<RwLock<TransitionTable>>` — no simulation abstractions.

use std::sync::Arc;

use temper_runtime::ActorSystem;
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_server::dispatch::AgentContext;
use temper_server::registry::SpecRegistry;
use temper_server::{ServerEventStore, ServerState};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

/// An extended Order spec with an additional "Archived" state and "ArchiveOrder" action.
const ORDER_V2_IOA: &str = r#"
[automaton]
name = "Order"
initial = "Draft"
states = ["Draft", "Submitted", "Confirmed", "Processing", "Shipped", "Delivered", "Cancelled", "Archived"]

[[action]]
name = "AddItem"
from = ["Draft"]
to = "Draft"
kind = "input"

[[action.effect]]
type = "IncrementCounter"
var = "item_count"

[[action]]
name = "SubmitOrder"
from = ["Draft"]
to = "Submitted"
kind = "input"

[[action.guard]]
type = "CounterMin"
var = "item_count"
min = 1

[[action]]
name = "ConfirmOrder"
from = ["Submitted"]
to = "Confirmed"
kind = "internal"

[[action]]
name = "ProcessOrder"
from = ["Confirmed"]
to = "Processing"
kind = "internal"

[[action]]
name = "ShipOrder"
from = ["Processing"]
to = "Shipped"
kind = "internal"

[[action]]
name = "DeliverOrder"
from = ["Shipped"]
to = "Delivered"
kind = "internal"

[[action]]
name = "CancelOrder"
from = ["Draft", "Submitted", "Confirmed", "Processing"]
to = "Cancelled"
kind = "input"

[[action]]
name = "ArchiveOrder"
from = ["Delivered", "Cancelled"]
to = "Archived"
kind = "input"
"#;

fn build_state_with_sim(seed: u64) -> ServerState {
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let store = ServerEventStore::Sim(sim_store);

    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    registry.register_tenant("default", csdl, CSDL_XML.to_string(), &[("Order", ORDER_IOA)]);

    let system = ActorSystem::new("dst-hotswap");
    let mut state = ServerState::from_registry(system, registry);
    state.event_store = Some(Arc::new(store));
    state
}

// =========================================================================
// Test: Hot-swap adds new states visible to live entities
// =========================================================================

#[tokio::test]
async fn dst_hotswap_entity_sees_new_table() {
    for seed in 0..50 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let state = build_state_with_sim(seed);
        let tenant = TenantId::default();
        let agent = AgentContext::default();

        // Create an Order and advance to Confirmed.
        state
            .dispatch_tenant_action(
                &tenant,
                "Order",
                &format!("o-{seed}"),
                "AddItem",
                serde_json::json!({}),
                &agent,
            )
            .await
            .expect("AddItem");

        state
            .dispatch_tenant_action(
                &tenant,
                "Order",
                &format!("o-{seed}"),
                "SubmitOrder",
                serde_json::json!({}),
                &agent,
            )
            .await
            .expect("SubmitOrder");

        let r = state
            .dispatch_tenant_action(
                &tenant,
                "Order",
                &format!("o-{seed}"),
                "ConfirmOrder",
                serde_json::json!({}),
                &agent,
            )
            .await
            .expect("ConfirmOrder");
        assert_eq!(r.state.status, "Confirmed");

        // Hot-swap to v2 spec (adds "Archived" state and "ArchiveOrder" action).
        {
            let mut reg = state.registry.write().expect("registry lock"); // ci-ok: infallible lock
            let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
            reg.register_tenant(
                "default",
                csdl,
                CSDL_XML.to_string(),
                &[("Order", ORDER_V2_IOA)],
            );
        }

        // Advance through the remaining states using v2 table.
        for action in &["ProcessOrder", "ShipOrder", "DeliverOrder"] {
            let r = state
                .dispatch_tenant_action(
                    &tenant,
                    "Order",
                    &format!("o-{seed}"),
                    action,
                    serde_json::json!({}),
                    &agent,
                )
                .await
                .expect(action);
            assert!(r.success, "seed {seed}: {action} failed: {:?}", r.error);
        }

        // Now try the v2-only action: ArchiveOrder.
        let r = state
            .dispatch_tenant_action(
                &tenant,
                "Order",
                &format!("o-{seed}"),
                "ArchiveOrder",
                serde_json::json!({}),
                &agent,
            )
            .await
            .expect("ArchiveOrder");
        assert!(
            r.success,
            "seed {seed}: ArchiveOrder (v2 action) should succeed after hot-swap: {:?}",
            r.error
        );
        assert_eq!(r.state.status, "Archived");
    }
}

// =========================================================================
// Test: Version monotonically increases on hot-swap
// =========================================================================

#[tokio::test]
async fn dst_hotswap_version_increases() {
    let (_guard, _clock, _id_gen) = install_deterministic_context(42);
    let state = build_state_with_sim(42);
    let tenant = TenantId::default();

    // Get initial version.
    let v1 = {
        let reg = state.registry.read().expect("registry lock"); // ci-ok: infallible lock
        let spec = reg.get_spec(&tenant, "Order").expect("Order spec");
        spec.swap_controller().version()
    };

    // Hot-swap.
    {
        let mut reg = state.registry.write().expect("registry lock"); // ci-ok: infallible lock
        let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
        reg.register_tenant(
            "default",
            csdl,
            CSDL_XML.to_string(),
            &[("Order", ORDER_V2_IOA)],
        );
    }

    let v2 = {
        let reg = state.registry.read().expect("registry lock"); // ci-ok: infallible lock
        let spec = reg.get_spec(&tenant, "Order").expect("Order spec");
        spec.swap_controller().version()
    };

    assert!(
        v2 > v1,
        "version should increase after hot-swap: v1={v1}, v2={v2}"
    );
}
