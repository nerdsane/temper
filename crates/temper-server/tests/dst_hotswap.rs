//! DST hot-swap safety tests.
//!
//! Verifies that hot-swapping transition tables via `SwapController` is
//! safe while entities are live. The real `ServerState` dispatches through
//! the real `Arc<RwLock<TransitionTable>>` — no simulation abstractions.

mod common;

use temper_runtime::scheduler::{install_deterministic_context, sim_now};
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::{SimEventStore, SimFaultConfig};

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

/// Extend the current fixture without removing or changing its verified safety model.
fn order_v2_ioa() -> String {
    let extended_states = ORDER_IOA.replacen(
        "\"ReturnRequested\", \"Returned\", \"Refunded\"]",
        "\"ReturnRequested\", \"Returned\", \"Refunded\", \"Archived\"]",
        1,
    );
    format!(
        r#"{extended_states}

[[action]]
name = "ArchiveOrder"
from = ["Delivered"]
to = "Archived"
kind = "input"
"#
    )
}

// =========================================================================
// Test: Hot-swap adds new states visible to live entities
// =========================================================================

#[tokio::test]
async fn dst_hotswap_entity_sees_new_table() {
    for seed in 0..50 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let (state, _sim_store) = common::build_default_state(seed, "dst-hotswap");
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
        {
            let mut reg = state.registry.write().expect("registry lock"); // ci-ok: infallible lock
            let csdl = parse_csdl(common::CSDL_XML).expect("CSDL parse");
            let order_v2 = order_v2_ioa();
            reg.register_tenant(
                "default",
                csdl,
                common::CSDL_XML.to_string(),
                &[("Order", &order_v2)],
            );
        }

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
    let (state, _sim_store) = common::build_default_state(42, "dst-hotswap");
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
        let csdl = parse_csdl(common::CSDL_XML).expect("CSDL parse");
        let order_v2 = order_v2_ioa();
        reg.register_tenant(
            "default",
            csdl,
            common::CSDL_XML.to_string(),
            &[("Order", &order_v2)],
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

#[tokio::test]
async fn dst_hotswap_replays_pre_and_post_swap_journal_after_restart() {
    for seed in 0..20 {
        let shared_store = SimEventStore::no_faults(seed);
        let tenant = TenantId::default();
        let entity_id = format!("o-replay-{seed}");

        {
            let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
            let state = common::build_default_state_with_store(
                shared_store.clone(),
                "dst-hotswap-replay-before",
            );
            for action in &["AddItem", "SubmitOrder", "ConfirmOrder"] {
                let result = common::dispatch(
                    &state,
                    &tenant,
                    "Order",
                    &entity_id,
                    action,
                    serde_json::json!({}),
                )
                .await
                .unwrap_or_else(|error| panic!("seed {seed}: {action} failed: {error}"));
                assert!(result.success, "seed {seed}: {action}: {:?}", result.error);
            }

            register_order_v2(&state);
            for action in &["ProcessOrder", "ShipOrder", "DeliverOrder", "ArchiveOrder"] {
                let result = common::dispatch(
                    &state,
                    &tenant,
                    "Order",
                    &entity_id,
                    action,
                    serde_json::json!({}),
                )
                .await
                .unwrap_or_else(|error| panic!("seed {seed}: {action} failed: {error}"));
                assert!(result.success, "seed {seed}: {action}: {:?}", result.error);
            }
        }

        let (_guard, _clock, _id_gen) = install_deterministic_context(seed + 10_000);
        let restarted =
            common::build_default_state_with_store(shared_store, "dst-hotswap-replay-after");
        register_order_v2(&restarted);
        let recovered = restarted
            .get_tenant_entity_state(&tenant, "Order", &entity_id)
            .await
            .unwrap_or_else(|error| panic!("seed {seed}: replay failed: {error}"));
        assert_eq!(recovered.state.status, "Archived", "seed {seed}");
        assert_eq!(recovered.state.item_count, 1, "seed {seed}");
    }
}

#[tokio::test]
async fn dst_hotswap_snapshot_fault_retains_live_post_swap_state() {
    let seed = 213;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::new(
        seed,
        SimFaultConfig {
            snapshot_failure_prob: 1.0,
            ..SimFaultConfig::none()
        },
    );
    let state = common::build_default_state_with_store(sim_store, "dst-hotswap-snapshot-failure");
    let tenant = TenantId::default();
    let entity_id = format!("o-snapshot-failure-{seed}");

    for action in &["AddItem", "SubmitOrder", "ConfirmOrder"] {
        let result = common::dispatch(
            &state,
            &tenant,
            "Order",
            &entity_id,
            action,
            serde_json::json!({}),
        )
        .await
        .expect("pre-swap dispatch");
        assert!(result.success, "{action}: {:?}", result.error);
    }
    register_order_v2(&state);
    for action in &["ProcessOrder", "ShipOrder", "DeliverOrder", "ArchiveOrder"] {
        let result = common::dispatch(
            &state,
            &tenant,
            "Order",
            &entity_id,
            action,
            serde_json::json!({}),
        )
        .await
        .expect("post-swap dispatch");
        assert!(result.success, "{action}: {:?}", result.error);
    }

    let actor_key = format!("{tenant}:Order:{entity_id}");
    state.last_accessed.write().unwrap().insert(
        actor_key.clone(),
        sim_now() - chrono::Duration::seconds(600),
    );
    state.passivate_idle_actors().await;

    assert!(
        state
            .actor_registry
            .read()
            .unwrap()
            .contains_key(&actor_key),
        "snapshot failure must retain the live actor with acknowledged post-swap state"
    );
    let current = state
        .get_tenant_entity_state(&tenant, "Order", &entity_id)
        .await
        .expect("retained actor state");
    assert_eq!(current.state.status, "Archived");
    assert_eq!(current.state.item_count, 1);
}

fn register_order_v2(state: &temper_server::ServerState) {
    let mut registry = state.registry.write().expect("registry lock"); // ci-ok: infallible lock
    let csdl = parse_csdl(common::CSDL_XML).expect("CSDL parse");
    let order_v2 = order_v2_ioa();
    registry.register_tenant(
        "default",
        csdl,
        common::CSDL_XML.to_string(),
        &[("Order", &order_v2)],
    );
}
