//! End-to-end integration test for Phase 1–3 of nerdsane/temper#128 —
//! exercises the **production** `ReactionDispatcher` path (async, through
//! `ServerState.dispatch_tenant_action`) rather than the sim-only
//! `SimReactionSystem` used in `reaction_cascade.rs`.
//!
//! This is the verification that closes the loop the ADR promises:
//! a reaction declared in TOML (params_from + guard + Create resolver)
//! actually dispatches through the live platform stack.

use std::sync::Arc;

use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventMetadata, PersistenceEnvelope};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::storage::{BoxedEventStore, StorageStack};
use temper_server::trigger::delivery::{
    PersistedReactionIntent, ReactionDeliveryRecord, ReactionDeliveryStatus,
    append_delivery_record, attach_intents, delivery_journal_id, extract_intents, extract_receipt,
    stable_delivery_id,
};
use temper_server::trigger::registry::parse_reactions;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.ReactE2E" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Order">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Payment">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Orders"   EntityType="Temper.ReactE2E.Order"/>
        <EntitySet Name="Payments" EntityType="Temper.ReactE2E.Payment"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const ORDER_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted", "Confirmed", "Cancelled"]
initial = "Draft"

[[state]]
name = "items"
type = "counter"
initial = "0"

[[action]]
name = "AddItem"
kind = "input"
from = ["Draft"]

[[action]]
name = "SubmitOrder"
kind = "internal"
from = ["Draft"]
to = "Submitted"
guard = "items > 0"

[[action]]
name = "ConfirmOrder"
kind = "internal"
from = ["Submitted"]
to = "Confirmed"

[[action]]
name = "CancelOrder"
kind = "input"
from = ["Draft", "Submitted"]
to = "Cancelled"
"#;

const PAYMENT_IOA: &str = r#"
[automaton]
name = "Payment"
states = ["Pending", "Authorized", "Captured", "Failed"]
initial = "Pending"

[[action]]
name = "AuthorizePayment"
kind = "internal"
from = ["Pending"]
to = "Authorized"

[[action]]
name = "CapturePayment"
kind = "internal"
from = ["Authorized"]
to = "Captured"

[[action]]
name = "FailPayment"
kind = "internal"
from = ["Pending", "Authorized"]
to = "Failed"
"#;

/// Build a ServerState with Order + Payment registered under the given
/// tenant plus the supplied reaction rules. Rebuilds the reaction dispatcher
/// so reactions fire through the production code path.
fn build_state(tenant: &str, reactions_toml: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    let reactions = parse_reactions(reactions_toml).expect("reactions TOML should parse");
    registry
        .try_register_tenant_with_reactions(
            tenant,
            csdl,
            CSDL_XML.to_string(),
            &[("Order", ORDER_IOA), ("Payment", PAYMENT_IOA)],
            reactions,
        )
        .expect("tenant registration");

    let system = ActorSystem::new("reaction-e2e-prod");
    let state = ServerState::from_registry(system, registry);
    state.rebuild_reaction_dispatcher();
    state
}

fn build_durable_state(tenant: &str, reactions_toml: &str) -> (ServerState, SimEventStore) {
    let store = SimEventStore::no_faults(414);
    let mut state = build_state(tenant, reactions_toml);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    (state, store)
}

async fn dispatch(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    params: serde_json::Value,
) -> temper_server::entity_actor::EntityResponse {
    state
        .dispatch_tenant_action(
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            &AgentContext::default(),
        )
        .await
        .unwrap_or_else(|e| panic!("dispatch {entity_type}.{action} failed: {e}"))
}

async fn status(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
) -> String {
    state
        .get_tenant_entity_state(tenant, entity_type, entity_id)
        .await
        .unwrap_or_else(|e| panic!("get_entity_state {entity_type}:{entity_id} failed: {e}"))
        .state
        .status
}

// =========================================================================
// E2E-1: Basic reaction fires through production dispatcher.
//
// Proves the whole stack wires up: parse_reactions → try_register_tenant
// → build_reaction_registry → ReactionDispatcher → dispatch_tenant_action
// → reaction target action completes.
// =========================================================================

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

#[tokio::test]
async fn durable_dispatch_records_source_intent_target_receipt_and_success() {
    let (_guard, _clock, _ids) = install_deterministic_context(414);
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
    let tenant_name = "shop-durable-414";
    let (state, store) = build_durable_state(tenant_name, reactions);
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

    let source = store.dump_journal(&format!("{tenant_name}:Order:o1"));
    let source_event = source
        .iter()
        .find(|event| event.event_type == "ConfirmOrder")
        .expect("source event must be durable");
    let intents = extract_intents(&source_event.payload).expect("source intent must decode");
    assert_eq!(intents.len(), 1);

    let lifecycle = store.dump_journal(&delivery_journal_id(&intents[0]));
    let record: ReactionDeliveryRecord = serde_json::from_value(
        lifecycle
            .last()
            .expect("delivery lifecycle must be durable")
            .payload
            .clone(),
    )
    .expect("delivery record must decode");
    assert_eq!(record.status, ReactionDeliveryStatus::Succeeded);

    let target = store.dump_journal(&format!("{tenant_name}:Payment:o1"));
    let target_event = target
        .iter()
        .find(|event| event.event_type == "AuthorizePayment")
        .expect("target event must be durable");
    let receipt = extract_receipt(&target_event.payload)
        .expect("receipt must decode")
        .expect("target event must contain receipt");
    assert_eq!(receipt.delivery_id, intents[0].delivery_id);
    assert_eq!(receipt.fencing_token, record.fencing_token);
}

#[tokio::test]
async fn awaited_durable_reaction_reports_permanent_target_failure_after_source_commit() {
    let (_guard, _clock, _ids) = install_deterministic_context(417);
    let reactions = r#"
[[reaction]]
name = "invalid_capture"
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
to_state = "Confirmed"
[reaction.then]
entity_type = "Payment"
action = "CapturePayment"
[reaction.resolve_target]
type = "same_id"
"#;
    let tenant_name = "shop-await-failure-417";
    let (state, store) = build_durable_state(tenant_name, reactions);
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

    let error = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "o1",
            "ConfirmOrder",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect_err("awaited target rejection must be reported");
    assert!(error.contains("Rejected"), "unexpected error: {error}");
    assert_eq!(status(&state, &tenant, "Order", "o1").await, "Confirmed");

    let source = store.dump_journal(&format!("{tenant_name}:Order:o1"));
    let intent = extract_intents(
        &source
            .iter()
            .find(|event| event.event_type == "ConfirmOrder")
            .expect("committed source event")
            .payload,
    )
    .expect("source intent")
    .pop()
    .expect("one intent");
    let lifecycle = store.dump_journal(&delivery_journal_id(&intent));
    let record: ReactionDeliveryRecord =
        serde_json::from_value(lifecycle.last().expect("delivery outcome").payload.clone())
            .expect("delivery record");
    assert_eq!(record.status, ReactionDeliveryStatus::Rejected);
}

#[tokio::test]
async fn drop_ok_turns_permanent_target_failure_into_accepted_terminal_drop() {
    let (_guard, _clock, _ids) = install_deterministic_context(418);
    let reactions = r#"
[[reaction]]
name = "best_effort_capture"
drop_ok = true
[reaction.when]
entity_type = "Order"
action = "ConfirmOrder"
to_state = "Confirmed"
[reaction.then]
entity_type = "Payment"
action = "CapturePayment"
[reaction.resolve_target]
type = "same_id"
"#;
    let tenant_name = "shop-drop-ok-418";
    let (state, store) = build_durable_state(tenant_name, reactions);
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

    let source = store.dump_journal(&format!("{tenant_name}:Order:o1"));
    let intent = extract_intents(
        &source
            .iter()
            .find(|event| event.event_type == "ConfirmOrder")
            .expect("committed source event")
            .payload,
    )
    .expect("source intent")
    .pop()
    .expect("one intent");
    let lifecycle = store.dump_journal(&delivery_journal_id(&intent));
    let record: ReactionDeliveryRecord =
        serde_json::from_value(lifecycle.last().expect("delivery outcome").payload.clone())
            .expect("delivery record");
    assert_eq!(record.status, ReactionDeliveryStatus::DroppedAllowed);
}

#[tokio::test]
async fn restart_after_target_commit_reconciles_without_duplicate_target_event() {
    let (_guard, _clock, _ids) = install_deterministic_context(415);
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
    let tenant_name = "shop-restart-415";
    let (state, store) = build_durable_state(tenant_name, reactions);
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

    let source = store.dump_journal(&format!("{tenant_name}:Order:o1"));
    let intent = extract_intents(
        &source
            .iter()
            .find(|event| event.event_type == "ConfirmOrder")
            .expect("source event must exist")
            .payload,
    )
    .expect("source intent must decode")
    .pop()
    .expect("source intent must exist");
    let lifecycle_id = delivery_journal_id(&intent);
    let lifecycle = store.dump_journal(&lifecycle_id);
    let mut ambiguous: ReactionDeliveryRecord = serde_json::from_value(
        lifecycle
            .last()
            .expect("lifecycle must exist")
            .payload
            .clone(),
    )
    .expect("lifecycle must decode");
    ambiguous.status = ReactionDeliveryStatus::Dispatching;
    ambiguous.lease_expires_at = Some(sim_now() - chrono::Duration::seconds(1));
    append_delivery_record(
        &temper_server::storage::BoxedEventStore::new(store.clone()),
        lifecycle
            .last()
            .expect("lifecycle sequence must exist")
            .sequence_nr,
        &ambiguous,
    )
    .await
    .expect("ambiguous crash state must persist");
    drop(state);

    let mut restarted = build_state(tenant_name, reactions);
    restarted.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    let dispatcher = restarted
        .reaction_dispatcher
        .read()
        .expect("dispatcher lock")
        .clone()
        .expect("dispatcher");
    dispatcher
        .dispatch_committed_intent(&restarted, intent)
        .await
        .expect("expired delivery should recover");

    let target = store.dump_journal(&format!("{tenant_name}:Payment:o1"));
    assert_eq!(
        target
            .iter()
            .filter(|event| event.event_type == "AuthorizePayment")
            .count(),
        1,
        "target idempotency identity must suppress duplicate commit after restart"
    );
    let recovered = store.dump_journal(&lifecycle_id);
    let latest: ReactionDeliveryRecord = serde_json::from_value(
        recovered
            .last()
            .expect("recovered lifecycle")
            .payload
            .clone(),
    )
    .expect("recovered lifecycle must decode");
    assert_eq!(latest.status, ReactionDeliveryStatus::Succeeded);
    assert_eq!(
        latest.fencing_token, ambiguous.fencing_token,
        "receipt reconciliation must finish without another target attempt"
    );
}

#[tokio::test]
async fn recovery_scan_delivers_intent_left_pending_before_process_restart() {
    let (_guard, _clock, _ids) = install_deterministic_context(416);
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
    let tenant_name = "shop-recovery-416";
    let store = SimEventStore::no_faults(416);
    let boxed = BoxedEventStore::new(store.clone());
    let rule = parse_reactions(reactions)
        .expect("reaction must parse")
        .pop()
        .expect("reaction must exist");
    let delivery_id =
        stable_delivery_id(tenant_name, "Order", "o1", "ConfirmOrder", 1, &rule.name, 0);
    let authority = AgentContext::for_service("recovery-test")
        .security_ctx
        .expect("service authority");
    let intent = PersistedReactionIntent {
        delivery_id: delivery_id.clone(),
        root_delivery_id: delivery_id,
        tenant: tenant_name.to_string(),
        source_entity_type: "Order".to_string(),
        source_entity_id: "o1".to_string(),
        source_action: "ConfirmOrder".to_string(),
        source_sequence: 1,
        source_to_state: "Confirmed".to_string(),
        source_fields: serde_json::json!({}),
        target_entity_id: Some("o1".to_string()),
        trigger_name: rule.name.clone(),
        trigger_index: 0,
        depth: 0,
        rule: serde_json::to_value(rule).expect("rule must serialize"),
        authority: serde_json::to_value(authority).expect("authority must serialize"),
        created_at: sim_now(),
    };
    let mut payload = serde_json::json!({
        "action": "ConfirmOrder",
        "from_status": "Submitted",
        "to_status": "Confirmed",
        "timestamp": sim_now(),
        "params": {},
        "idempotency_key": "source-416"
    });
    attach_intents(&mut payload, std::slice::from_ref(&intent)).expect("intent must attach");
    boxed
        .append(
            &format!("{tenant_name}:Order:o1"),
            0,
            &[PersistenceEnvelope {
                sequence_nr: 1,
                event_type: "ConfirmOrder".to_string(),
                payload,
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: format!("{tenant_name}:Order:o1"),
                },
            }],
        )
        .await
        .expect("source event and intent must persist");

    let mut restarted = build_state(tenant_name, reactions);
    restarted.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    let dispatcher = restarted
        .reaction_dispatcher
        .read()
        .expect("dispatcher lock")
        .clone()
        .expect("dispatcher");
    assert_eq!(
        dispatcher
            .recover_tenant_deliveries(&restarted, &TenantId::new(tenant_name), 10)
            .await
            .expect("recovery scan must succeed"),
        1
    );
    assert_eq!(
        status(&restarted, &TenantId::new(tenant_name), "Payment", "o1").await,
        "Authorized"
    );
    let lifecycle = store.dump_journal(&delivery_journal_id(&intent));
    let latest: ReactionDeliveryRecord = serde_json::from_value(
        lifecycle
            .last()
            .expect("lifecycle must exist")
            .payload
            .clone(),
    )
    .expect("lifecycle must decode");
    assert_eq!(latest.status, ReactionDeliveryStatus::Succeeded);
}

// =========================================================================
// E2E-2: Guarded reaction — source field gate.
//
// Two rules on the same trigger; a source-field guard picks exactly one.
// Proves ReactionGuard evaluation works through the production path.
// =========================================================================

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
