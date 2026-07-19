//! DST hot-swap safety tests.
//!
//! Verifies that hot-swapping transition tables via `SwapController` is
//! safe while entities are live. The real `ServerState` dispatches through
//! the real `Arc<RwLock<TransitionTable>>` — no simulation abstractions.

mod common;

use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

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

fn publish_order_v2(state: &ServerState, sim_store: &SimEventStore) {
    let fingerprint = temper_store_turso::spec_content_hash(ORDER_V2_IOA);
    let revision = sim_store.persist_spec_declaration("default", "Order", &fingerprint);
    assert!(
        revision > 0,
        "hot-swap must advance durable declaration authority first"
    );
    let mut registry = state.registry.write().expect("registry lock"); // ci-ok: infallible lock
    let csdl = parse_csdl(common::CSDL_XML).expect("CSDL parse");
    registry.register_tenant(
        "default",
        csdl,
        common::CSDL_XML.to_string(),
        &[("Order", ORDER_V2_IOA)],
    );
}

// =========================================================================
// Test: Hot-swap adds new states visible to live entities
// =========================================================================

#[tokio::test]
async fn dst_hotswap_entity_sees_new_table() {
    for seed in 0..50 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let (state, sim_store) = common::build_default_state(seed, "dst-hotswap");
        let tenant = TenantId::default();

        // Create an Order and advance to Confirmed.
        common::dispatch(
            &state,
            &tenant,
            "Order",
            &format!("o-{seed}"),
            "AddItem",
            serde_json::json!({}),
        )
        .await
        .expect("AddItem");

        common::dispatch(
            &state,
            &tenant,
            "Order",
            &format!("o-{seed}"),
            "SubmitOrder",
            serde_json::json!({}),
        )
        .await
        .expect("SubmitOrder");

        let r = common::dispatch(
            &state,
            &tenant,
            "Order",
            &format!("o-{seed}"),
            "ConfirmOrder",
            serde_json::json!({}),
        )
        .await
        .expect("ConfirmOrder");
        assert_eq!(r.state.status, "Confirmed");

        // Hot-swap to v2 spec (adds "Archived" state and "ArchiveOrder" action).
        publish_order_v2(&state, &sim_store);

        // Advance through the remaining states using v2 table.
        for action in &["ProcessOrder", "ShipOrder", "DeliverOrder"] {
            let r = common::dispatch(
                &state,
                &tenant,
                "Order",
                &format!("o-{seed}"),
                action,
                serde_json::json!({}),
            )
            .await
            .expect(action);
            assert!(r.success, "seed {seed}: {action} failed: {:?}", r.error);
        }

        // Now try the v2-only action: ArchiveOrder.
        let r = common::dispatch(
            &state,
            &tenant,
            "Order",
            &format!("o-{seed}"),
            "ArchiveOrder",
            serde_json::json!({}),
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
    let (state, sim_store) = common::build_default_state(42, "dst-hotswap");
    let tenant = TenantId::default();

    // Get initial version.
    let v1 = {
        let reg = state.registry.read().expect("registry lock"); // ci-ok: infallible lock
        let spec = reg.get_spec(&tenant, "Order").expect("Order spec");
        spec.swap_controller().version()
    };

    // Hot-swap.
    publish_order_v2(&state, &sim_store);

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
