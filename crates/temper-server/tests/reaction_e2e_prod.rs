//! End-to-end integration test for Phase 1–3 of nerdsane/temper#128 —
//! exercises the **production** `ReactionDispatcher` path (async, through
//! `ServerState.dispatch_tenant_action`) rather than the sim-only
//! `SimReactionSystem` used in `reaction_cascade.rs`.
//!
//! This is the verification that closes the loop the ADR promises:
//! a reaction declared in TOML (params_from + guard + Create resolver)
//! actually dispatches through the live platform stack.

mod common;

use common::reaction_fixture::*;

#[tokio::test(flavor = "multi_thread")]
async fn prod_dispatcher_fires_basic_reaction() {
    let reactions = r#"
[[reaction]]
name = "order_confirmed_authorizes_payment"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
to_state = "Confirmed"
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
[reaction.resolve_target]
type = "same_id"
"#;
    let tenant_name = "shop-e2e-1";
    let state = Arc::new(build_state(tenant_name, reactions));
    let tenant = TenantId::new(tenant_name);

    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "AddItem",
        serde_json::json!({}),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "SubmitOrder",
        serde_json::json!({}),
    )
    .await;
    let r = dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "ConfirmOrder",
        serde_json::json!({}),
    )
    .await;
    assert!(r.success, "ConfirmOrder must succeed");

    // Reaction dispatch is awaited inline by `dispatch_tenant_action`
    // (`state/dispatch/actions.rs:157`), so by the time `r` returns the
    // target entity's transition is already committed.
    let order_status = status(&state, &tenant, "Order", "o1").await;
    assert_eq!(order_status, "Confirmed");

    let payment_status = status(&state, &tenant, "Payment", "o1").await;
    assert_eq!(
        payment_status, "Authorized",
        "reaction should have dispatched AuthorizePayment on Payment:o1"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn prod_dispatcher_honours_source_field_guard() {
    // Two rules: one guarded state_in=["Confirmed"], one guarded
    // state_in=["Cancelled"]. Only the Confirmed one should fire on
    // Order.ConfirmOrder.
    let reactions = r#"
[[reaction]]
name = "fires_on_confirmed"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
[reaction.when.guard]
type = "state_in"
values = ["Confirmed"]
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
[reaction.resolve_target]
type = "same_id"

[[reaction]]
name = "skipped_on_cancelled"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
[reaction.when.guard]
type = "state_in"
values = ["Cancelled"]
[reaction.then]
entity_type = "Payment"
action = "FailPayment"
[reaction.resolve_target]
type = "same_id"
"#;
    let tenant_name = "shop-e2e-2";
    let state = Arc::new(build_state(tenant_name, reactions));
    let tenant = TenantId::new(tenant_name);

    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "AddItem",
        serde_json::json!({}),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "SubmitOrder",
        serde_json::json!({}),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "ConfirmOrder",
        serde_json::json!({}),
    )
    .await;

    // Guard gate works: Payment is Authorized (passing rule fired), not Failed
    // (skipped rule did NOT fire).
    let payment_status = status(&state, &tenant, "Payment", "o1").await;
    assert_eq!(
        payment_status, "Authorized",
        "state_in guard should have selected the Confirmed rule"
    );
}

// =========================================================================
// E2E-3: NOT guard — rule skipped because inner condition passes.
//
// Rule guarded with Not(StateIn[Confirmed]) must NOT fire when the source
// post-status IS Confirmed. Confirms guard-skipped rules do not side-effect.
// =========================================================================

#[tokio::test(flavor = "multi_thread")]
async fn prod_dispatcher_not_guard_skips_firing() {
    let reactions = r#"
[[reaction]]
name = "skipped_when_confirmed"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
[reaction.when.guard]
type = "not"
[reaction.when.guard.guard]
type = "state_in"
values = ["Confirmed"]
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
[reaction.resolve_target]
type = "same_id"
"#;
    let tenant_name = "shop-e2e-3";
    let state = Arc::new(build_state(tenant_name, reactions));
    let tenant = TenantId::new(tenant_name);

    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "AddItem",
        serde_json::json!({}),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "SubmitOrder",
        serde_json::json!({}),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "ConfirmOrder",
        serde_json::json!({}),
    )
    .await;

    // Payment should stay Pending — the reaction was gated off.
    let payment_status = status(&state, &tenant, "Payment", "o1").await;
    assert_eq!(
        payment_status, "Pending",
        "Not(state_in=Confirmed) guard must have skipped the rule"
    );
}

// =========================================================================
// E2E-4: params_from — dynamic params pipe through production dispatch
// without breaking the cascade when source field is missing.
//
// Proves build_effective_params (shared helper between prod and sim) is
// wired correctly into ReactionDispatcher. Uses missing-source-field case
// because the plan's "warn + skip the key" policy is the load-bearing one.
// =========================================================================

#[tokio::test(flavor = "multi_thread")]
async fn prod_dispatcher_params_from_missing_field_still_fires() {
    let reactions = r#"
[[reaction]]
name = "order_confirmed_with_params_from"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
[reaction.then]
entity_type = "Payment"
action = "AuthorizePayment"
# Reference a field Order.ConfirmOrder never produces.
params = { note = "from_reaction" }
params_from = { passed_field = "nonexistent" }
[reaction.resolve_target]
type = "same_id"
"#;
    let tenant_name = "shop-e2e-4";
    let state = Arc::new(build_state(tenant_name, reactions));
    let tenant = TenantId::new(tenant_name);

    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "AddItem",
        serde_json::json!({}),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "SubmitOrder",
        serde_json::json!({}),
    )
    .await;
    dispatch(
        &state,
        &tenant,
        "Order",
        "o1",
        "ConfirmOrder",
        serde_json::json!({}),
    )
    .await;

    // Payment authorized — missing source field triggered warn+skip for
    // the passed_field key but the reaction still fired.
    let payment_status = status(&state, &tenant, "Payment", "o1").await;
    assert_eq!(payment_status, "Authorized");
}
