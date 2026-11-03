//! A spec version that declares new state reaches records made before it.
//!
//! A record made under the first version stores none of the counters and
//! booleans the second declares. Each reads as its declared initial value
//! wherever an action reads it: a constraint, a guard, an effect and a
//! trigger's args. The journal then holds the values the action wrote, so a
//! restart reads them back.

mod common;

use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;

/// What the second version adds to the Order spec. `brought` must rise with
/// every Bring, as an app's place counter does; `credits` starts at 3 and
/// `open` starts true, so reading either as its type's zero would refuse
/// Spend.
const ADDED: &str = r#"
[[state]]
name = "brought"
type = "counter"
initial = 0

[[state]]
name = "credits"
type = "counter"
initial = 3

[[state]]
name = "open"
type = "bool"
initial = true

[[action]]
name = "Bring"
kind = "input"
from = ["Draft"]
params = ["brought"]
effect = ["brought = params.brought"]

[[action.constraints]]
kind = "param_greater_than_field"
param = "brought"
field = "brought"

[[action]]
name = "Spend"
kind = "input"
from = ["Draft"]
guard = "open && credits > 0"
effect = ["credits -= 1"]
"#;

fn order_v2() -> String {
    format!("{}\n{ADDED}", common::ORDER_IOA)
}

fn install_v2(state: &temper_server::ServerState) {
    let v2 = order_v2();
    let mut reg = state.registry.write().expect("registry lock"); // ci-ok: infallible lock
    let csdl = parse_csdl(common::CSDL_XML).expect("CSDL parse");
    reg.register_tenant(
        "default",
        csdl,
        common::CSDL_XML.to_string(),
        &[("Order", v2.as_str())],
    );
}

async fn act(
    state: &temper_server::ServerState,
    id: &str,
    action: &str,
    params: serde_json::Value,
) -> temper_server::entity_actor::EntityResponse {
    let response = common::dispatch(state, &TenantId::default(), "Order", id, action, params)
        .await
        .expect(action);
    assert!(response.success, "{action} on {id}: {:?}", response.error);
    response
}

/// Bring and Spend on a record that stores neither counter, then the values
/// they wrote.
async fn moves_on_an_old_record(state: &temper_server::ServerState, id: &str) {
    let brought = act(state, id, "Bring", serde_json::json!({ "brought": 1 })).await;
    assert_eq!(brought.state.counters.get("brought"), Some(&1));

    let spent = act(state, id, "Spend", serde_json::json!({})).await;
    assert_eq!(spent.state.counters.get("credits"), Some(&2));

    // Once written, the stored value is what the constraint compares.
    let stale = common::dispatch(
        state,
        &TenantId::default(),
        "Order",
        id,
        "Bring",
        serde_json::json!({ "brought": 1 }),
    )
    .await
    .expect("Bring");
    assert!(!stale.success, "a Bring that does not rise is refused");
}

#[tokio::test]
async fn dst_a_live_record_reads_state_a_new_version_declares() {
    for seed in 0..20 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let (state, _store) = common::build_default_state(seed, "dst-spec-adds-state");
        let id = format!("o-{seed}");
        let made = act(&state, &id, "AddItem", serde_json::json!({})).await;
        assert!(!made.state.counters.contains_key("brought"));

        install_v2(&state);
        moves_on_an_old_record(&state, &id).await;
    }
}

#[tokio::test]
async fn dst_a_restarted_record_reads_state_a_new_version_declares() {
    for seed in 0..20 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let (first, store) = common::build_default_state(seed, "dst-spec-adds-state-a");
        let id = format!("o-{seed}");
        act(&first, &id, "AddItem", serde_json::json!({})).await;
        drop(first);

        let v2 = order_v2();
        let second = common::build_single_tenant_state_with_store(
            store.clone(),
            "dst-spec-adds-state-b",
            "default",
            &[("Order", v2.as_str())],
        );
        moves_on_an_old_record(&second, &id).await;
        drop(second);

        // The journal holds what the moves wrote.
        let third = common::build_single_tenant_state_with_store(
            store,
            "dst-spec-adds-state-c",
            "default",
            &[("Order", v2.as_str())],
        );
        let after = act(&third, &id, "Spend", serde_json::json!({})).await;
        assert_eq!(after.state.counters.get("brought"), Some(&1));
        assert_eq!(after.state.counters.get("credits"), Some(&1));
    }
}

/// An Order spec that declares no `items` counter.
const ORDER_WITHOUT_ITEMS: &str = r#"
[automaton]
name = "Order"
states = ["Draft"]
initial = "Draft"

[[action]]
name = "Touch"
kind = "input"
from = ["Draft"]
"#;

/// The next version declares `items` from 3. The kernel also keeps `items`
/// in a field of its own that starts at 0, which must not read as stored.
const ADDS_ITEMS: &str = r#"
[[state]]
name = "items"
type = "counter"
initial = 3

[[action]]
name = "Take"
kind = "input"
from = ["Draft"]
guard = "items > 0"
effect = ["items -= 1"]
"#;

#[tokio::test]
async fn dst_a_record_reads_a_newly_declared_items_counter_from_its_initial_value() {
    for seed in 0..20 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let (state, _store) = common::build_single_tenant_state(
            seed,
            "dst-spec-adds-items",
            "default",
            &[("Order", ORDER_WITHOUT_ITEMS)],
        );
        let id = format!("o-{seed}");
        act(&state, &id, "Touch", serde_json::json!({})).await;

        let v2 = format!("{ORDER_WITHOUT_ITEMS}\n{ADDS_ITEMS}");
        {
            let mut reg = state.registry.write().expect("registry lock"); // ci-ok: infallible lock
            let csdl = parse_csdl(common::CSDL_XML).expect("CSDL parse");
            reg.register_tenant(
                "default",
                csdl,
                common::CSDL_XML.to_string(),
                &[("Order", v2.as_str())],
            );
        }
        let taken = act(&state, &id, "Take", serde_json::json!({})).await;
        assert_eq!(taken.state.counters.get("items"), Some(&2));
        assert_eq!(taken.state.item_count, 2);
    }
}

/// A record made under this version stores no `installs`.
const ORDER_BEFORE_INSTALLS: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Closed"]
initial = "Draft"

[[action]]
name = "Touch"
kind = "input"
from = ["Draft"]
"#;

/// The next version counts `installs` from 3, so reading it as a counter's
/// zero would refuse Close as surely as not sending it. AskClose sends Close
/// the record's count, which Close checks against the record: the shape of an
/// app install's AskPurge and Purge. AskCloseStale sends a count that is not
/// the record's.
const ADDS_INSTALLS: &str = r#"
[[state]]
name = "installs"
type = "counter"
initial = 3

[[action]]
name = "AskClose"
kind = "input"
from = ["Draft"]

[[action.triggers]]
name = "close"
kind = "entity"
target_entity = "Order"
target_action = "Close"
resolve_target = { kind = "same_id" }
args = { installs = "installs" }

[[action]]
name = "AskCloseStale"
kind = "input"
from = ["Draft"]

[[action.triggers]]
name = "close_stale"
kind = "entity"
target_entity = "Order"
target_action = "Close"
resolve_target = { kind = "same_id" }
args = { installs = "9" }

[[action]]
name = "Close"
kind = "input"
from = ["Draft"]
to = "Closed"
params = [{ name = "installs", type = "counter" }]
constraints = [{ kind = "param_equals_field", param = "installs", field = "installs" }]
"#;

/// A record made before the new version, under the new version's triggers.
fn old_record_state(seed: u64) -> temper_server::ServerState {
    let (state, _store) = common::build_single_tenant_state(
        seed,
        "dst-spec-adds-trigger-arg",
        "default",
        &[("Order", ORDER_BEFORE_INSTALLS)],
    );
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("policy parses");
    state
}

fn install_installs(state: &temper_server::ServerState) {
    let v2 = format!("{ORDER_BEFORE_INSTALLS}\n{ADDS_INSTALLS}");
    {
        let mut reg = state.registry.write().expect("registry lock"); // ci-ok: infallible lock
        let csdl = parse_csdl(common::CSDL_XML).expect("CSDL parse");
        reg.register_tenant(
            "default",
            csdl,
            common::CSDL_XML.to_string(),
            &[("Order", v2.as_str())],
        );
    }
    state.rebuild_reaction_dispatcher();
}

async fn order_status(state: &temper_server::ServerState, id: &str) -> String {
    state
        .get_tenant_entity_state(&TenantId::default(), "Order", id)
        .await
        .unwrap_or_else(|e| panic!("read Order {id}: {e}"))
        .state
        .status
}

#[tokio::test]
async fn dst_a_trigger_arg_reads_a_newly_declared_field_as_its_initial_value() {
    for seed in 0..20 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let state = old_record_state(seed);
        let id = format!("o-{seed}");
        let made = act(&state, &id, "Touch", serde_json::json!({})).await;
        assert!(made.state.fields.get("installs").is_none());

        install_installs(&state);
        act(&state, &id, "AskClose", serde_json::json!({})).await;

        assert_eq!(
            order_status(&state, &id).await,
            "Closed",
            "seed {seed}: the trigger sends the declared 3, which Close accepts"
        );
    }
}

#[tokio::test]
async fn dst_a_refused_trigger_move_is_on_the_source_records_observe_stream() {
    for seed in 0..20 {
        let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
        let state = old_record_state(seed);
        let id = format!("o-{seed}");
        act(&state, &id, "Touch", serde_json::json!({})).await;

        install_installs(&state);
        // The asking move commits: a trigger does not undo it.
        act(&state, &id, "AskCloseStale", serde_json::json!({})).await;
        assert_eq!(order_status(&state, &id).await, "Draft", "seed {seed}");

        let refused: Vec<_> = state
            .entity_observe_replay("default", "Order", &id, 0)
            .events
            .into_iter()
            .filter(|event| event.event_name == "reaction_refused")
            .collect();
        assert_eq!(refused.len(), 1, "seed {seed}: {refused:?}");
        let data = &refused[0].data;
        assert_eq!(data["action"], "AskCloseStale");
        assert_eq!(data["rule"], "Order:AskCloseStale:close_stale");
        assert_eq!(data["target_entity_type"], "Order");
        assert_eq!(data["target_entity_id"], id.as_str());
        assert_eq!(data["target_action"], "Close");
        assert_eq!(data["target_status"], "Draft");
        assert_eq!(data["refusal"]["code"], "ParameterConstraint");
        assert_eq!(data["refusal"]["parameter"], "installs");
        assert!(
            data["reason"]
                .as_str()
                .is_some_and(|r| r.contains("installs")),
            "seed {seed}: {data}"
        );
    }
}
