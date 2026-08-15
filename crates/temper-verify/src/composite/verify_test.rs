//! Tests for the bounded composite verification entry point (ADR-0150).

use super::*;
use temper_spec::automaton::parse_automaton;

fn order_ioa() -> &'static str {
    r#"
[automaton]
name = "Order"
states = ["Draft", "Confirmed"]
initial = "Draft"
allow_indefinite_states = ["Draft", "Confirmed"]

[[action]]
name = "ConfirmOrder"
from = ["Draft"]
to = "Confirmed"

[[action.triggers]]
name = "auth_payment"
kind = "entity"
target_entity = "Payment"
target_action = "AuthorizePayment"
# Best-effort: Payment may be authorized independently (benign convergence).
drop_ok = true

[action.triggers.resolve_target]
type = "same_id"
"#
}

fn payment_ioa() -> &'static str {
    r#"
[automaton]
name = "Payment"
states = ["Pending", "Authorized"]
initial = "Pending"
allow_indefinite_states = ["Pending", "Authorized"]

[[action]]
name = "AuthorizePayment"
from = ["Pending"]
to = "Authorized"
"#
}

fn wiki_ioa() -> &'static str {
    r#"
[automaton]
name = "Wiki"
states = ["Draft", "Published"]
initial = "Draft"
allow_indefinite_states = ["Draft", "Published"]

[[action]]
name = "Publish"
from = ["Draft"]
to = "Published"
"#
}

#[test]
fn seed_cover_groups_connected_entities_into_one_seed() {
    let order = parse_automaton(order_ioa()).unwrap();
    let payment = parse_automaton(payment_ioa()).unwrap();
    let seeds = seed_cover(&[&order, &payment]);
    // Order -> Payment are one weakly-connected component, root = "Order"
    // (lexicographically smaller than "Payment").
    assert_eq!(seeds, vec!["Order".to_string()]);
}

#[test]
fn seed_cover_includes_isolated_entities() {
    let order = parse_automaton(order_ioa()).unwrap();
    let payment = parse_automaton(payment_ioa()).unwrap();
    let wiki = parse_automaton(wiki_ioa()).unwrap();
    let seeds = seed_cover(&[&order, &payment, &wiki]);
    // Two components: {Order, Payment} -> "Order"; {Wiki} -> "Wiki".
    assert_eq!(seeds, vec!["Order".to_string(), "Wiki".to_string()]);
}

#[test]
fn verify_composite_passes_clean_chain() {
    let order = parse_automaton(order_ioa()).unwrap();
    let payment = parse_automaton(payment_ioa()).unwrap();
    let result = verify_composite(&[&order, &payment], "Order").unwrap();
    assert_eq!(result.outcome, CompositeOutcome::Verified, "{result:?}");
    assert!(result.passed());
    assert!(result.dropped_reactions.is_empty());
}

#[test]
fn verify_all_covers_every_component() {
    let order = parse_automaton(order_ioa()).unwrap();
    let payment = parse_automaton(payment_ioa()).unwrap();
    let wiki = parse_automaton(wiki_ioa()).unwrap();
    let results = verify_all(&[&order, &payment, &wiki]);
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r.passed()));
    // Payment, though not a seed, is in Order's scope — covered.
    let order_result = results.iter().find(|r| r.seed == "Order").unwrap();
    assert!(order_result.scope.contains(&"Payment".to_string()));
}

#[test]
fn verify_all_covers_reverse_only_member_of_weak_component() {
    let source = parse_automaton(
        order_ioa()
            .replace("name = \"Order\"", "name = \"ZSource\"")
            .replace("target_entity = \"Payment\"", "target_entity = \"ATarget\"")
            .as_str(),
    )
    .unwrap();
    let target = parse_automaton(
        payment_ioa()
            .replace("name = \"Payment\"", "name = \"ATarget\"")
            .as_str(),
    )
    .unwrap();
    let results = verify_all(&[&source, &target]);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].seed, "ATarget");
    assert!(results[0].scope.contains(&"ZSource".to_string()));
}

// ─── ADR-0150: no_dropped_reaction property ─────────────────────────────

/// A source that can reach the target only after the target has left its
/// required from-state. `Workspace.Freeze` moves Workspace to `Frozen`;
/// `File.Touch` then fires `Workspace.IncrementUsage`, which is enabled
/// only from `Active` — so the reaction is dropped. Mirrors the temper-fs
/// `IncrementUsage`-while-`Frozen` bug.
fn file_touch_increments_workspace() -> &'static str {
    r#"
[automaton]
name = "File"
states = ["New", "Updated"]
initial = "New"
allow_indefinite_states = ["New", "Updated"]

[[action]]
name = "Touch"
from = ["New"]
to = "Updated"

[[action.triggers]]
name = "touch_increments_usage"
kind = "entity"
target_entity = "Workspace"
target_action = "IncrementUsage"

[action.triggers.resolve_target]
type = "field"
field = "workspace_id"
"#
}

fn freezable_workspace() -> &'static str {
    r#"
[automaton]
name = "Workspace"
states = ["Active", "Frozen"]
initial = "Active"
allow_indefinite_states = ["Active", "Frozen"]

[[action]]
name = "IncrementUsage"
from = ["Active"]
to = "Active"

[[action]]
name = "Freeze"
from = ["Active"]
to = "Frozen"
"#
}

#[test]
fn no_dropped_reaction_catches_from_state_mismatch() {
    let file = parse_automaton(file_touch_increments_workspace()).unwrap();
    let workspace = parse_automaton(freezable_workspace()).unwrap();
    let result = verify_composite(&[&file, &workspace], "File").unwrap();
    assert_eq!(result.outcome, CompositeOutcome::Violated, "{result:?}");
    assert!(!result.passed());
    let drop = result
        .dropped_reactions
        .iter()
        .find(|d| d.target_action == "IncrementUsage")
        .expect("IncrementUsage drop should be reported");
    assert_eq!(drop.source_entity, "File");
    assert_eq!(drop.source_action, "Touch");
    assert_eq!(drop.target_entity, "Workspace");
    assert_eq!(
        drop.target_state, "Frozen",
        "drop must name the wrong target state"
    );
}

/// The same shape as `no_dropped_reaction_catches_from_state_mismatch`, but
/// the source action `File.Touch` carries a cross-entity guard requiring the
/// owning `Workspace` to be `Active`. In the joint model that guard resolves
/// concretely: `Touch` is simply not enabled while the Workspace is `Frozen`,
/// so the usage-increment reaction never fires into a dropping state. This is
/// the temper-fs Fix #1/#2 shape — guarding the write-accepting transition on
/// the container's state closes the dropped-reaction finding.
#[test]
fn cross_entity_guard_on_source_suppresses_drop() {
    let file = r#"
[automaton]
name = "File"
states = ["New", "Updated"]
initial = "New"
allow_indefinite_states = ["New", "Updated"]

[[action]]
name = "Touch"
from = ["New"]
to = "Updated"
guard = [
  { type = "cross_entity_state", entity_type = "Workspace", entity_id_source = "workspace_id", required_status = ["Active"] },
]

[[action.triggers]]
name = "touch_increments_usage"
kind = "entity"
target_entity = "Workspace"
target_action = "IncrementUsage"

[action.triggers.resolve_target]
type = "field"
field = "workspace_id"
"#;
    let file = parse_automaton(file).unwrap();
    let workspace = parse_automaton(freezable_workspace()).unwrap();
    let result = verify_composite(&[&file, &workspace], "File").unwrap();
    assert_eq!(
        result.outcome,
        CompositeOutcome::Verified,
        "a cross-entity guard on the source action must suppress the drop: {result:?}"
    );
    assert!(result.passed());
    assert!(
        result.dropped_reactions.is_empty(),
        "no reaction should drop once Touch is gated on Workspace=Active: {:?}",
        result.dropped_reactions
    );
}

/// The same drop-suppression must hold when the source guard is expressed as a
/// *denylist* (`forbidden_status`) rather than an allowlist. Since the Workspace
/// states are exactly `{Active, Frozen}`, "not in [Frozen]" is equivalent to
/// "in [Active]" for the in-scope target — so `Touch` is again disabled while
/// the Workspace is `Frozen` and the usage-increment reaction never drops. This
/// is the kernel-floor shape: `File.StreamUpdated` is gated
/// `forbidden_status = ["Frozen", "Archived"]`.
#[test]
fn forbidden_status_guard_on_source_suppresses_drop() {
    let file = r#"
[automaton]
name = "File"
states = ["New", "Updated"]
initial = "New"
allow_indefinite_states = ["New", "Updated"]

[[action]]
name = "Touch"
from = ["New"]
to = "Updated"
guard = [
  { type = "cross_entity_state", entity_type = "Workspace", entity_id_source = "workspace_id", forbidden_status = ["Frozen"] },
]

[[action.triggers]]
name = "touch_increments_usage"
kind = "entity"
target_entity = "Workspace"
target_action = "IncrementUsage"

[action.triggers.resolve_target]
type = "field"
field = "workspace_id"
"#;
    let file = parse_automaton(file).unwrap();
    let workspace = parse_automaton(freezable_workspace()).unwrap();
    let result = verify_composite(&[&file, &workspace], "File").unwrap();
    assert_eq!(
        result.outcome,
        CompositeOutcome::Verified,
        "a denylist cross-entity guard on the source action must suppress the drop: {result:?}"
    );
    assert!(result.passed());
    assert!(
        result.dropped_reactions.is_empty(),
        "no reaction should drop once Touch forbids Workspace=Frozen: {:?}",
        result.dropped_reactions
    );
}

#[test]
fn create_resolver_reaction_is_exempt() {
    // Same shape but the reaction spawns a fresh target via a `create`
    // resolver — a fresh target is always enabled, so it is never dropped.
    let file = r#"
[automaton]
name = "File"
states = ["New", "Updated"]
initial = "New"
allow_indefinite_states = ["New", "Updated"]

[[action]]
name = "Touch"
from = ["New"]
to = "Updated"

[[action.triggers]]
name = "touch_creates_version"
kind = "entity"
target_entity = "Version"
target_action = "Create"

[action.triggers.resolve_target]
type = "create"
"#;
    // Version.Create is enabled only from Current; but because the
    // resolver is `create`, a fresh Current instance is spawned — exempt.
    let version = r#"
[automaton]
name = "Version"
states = ["Current", "Superseded"]
initial = "Current"
allow_indefinite_states = ["Current", "Superseded"]

[[action]]
name = "Create"
from = ["Current"]
to = "Current"

[[action]]
name = "Supersede"
from = ["Current"]
to = "Superseded"
"#;
    let file = parse_automaton(file).unwrap();
    let version = parse_automaton(version).unwrap();
    let result = verify_composite(&[&file, &version], "File").unwrap();
    assert_eq!(
        result.outcome,
        CompositeOutcome::Verified,
        "create-resolver reaction must not be flagged: {result:?}"
    );
    assert!(result.dropped_reactions.is_empty());
}

#[test]
fn drop_ok_suppresses_violation() {
    // The same from-state mismatch as `no_dropped_reaction_catches...`,
    // but the trigger is marked drop_ok = true — suppressed.
    let file = r#"
[automaton]
name = "File"
states = ["New", "Updated"]
initial = "New"
allow_indefinite_states = ["New", "Updated"]

[[action]]
name = "Touch"
from = ["New"]
to = "Updated"

[[action.triggers]]
name = "touch_increments_usage"
kind = "entity"
target_entity = "Workspace"
target_action = "IncrementUsage"
drop_ok = true

[action.triggers.resolve_target]
type = "field"
field = "workspace_id"
"#;
    let file = parse_automaton(file).unwrap();
    let workspace = parse_automaton(freezable_workspace()).unwrap();
    let result = verify_composite(&[&file, &workspace], "File").unwrap();
    assert_eq!(
        result.outcome,
        CompositeOutcome::Verified,
        "drop_ok must suppress the violation: {result:?}"
    );
    assert!(result.dropped_reactions.is_empty());
}

#[test]
fn incomplete_when_budget_exhausted() {
    // Six counter entities (each ranging 0..=3 under the per-entity
    // ceiling) give a joint space of 4^6 = 4096 states, well above
    // Stateright's 1500-state work-block granularity. Counter0 fans out a
    // trigger to every other counter so all six land in one scope; the
    // targets are always enabled (no guard, single state) so nothing
    // drops — the run is clean but too large for the budget. A budget of
    // 100 forces the BFS to stop early, yielding an honest INCOMPLETE
    // (never a silent pass).
    // Counter0 declares an unreachable `Idle` state and gates every
    // fan-out trigger on `to_state = "Idle"`. Inc goes to "Counting", so
    // the triggers never actually fire: the edges exist only to pull all
    // six counters into one scope, keeping them independent (4^6 states).
    let mut hub = String::from(
        r#"
[automaton]
name = "Counter0"
states = ["Counting", "Idle"]
initial = "Counting"
allow_indefinite_states = ["Counting", "Idle"]

[[state]]
name = "n"
type = "counter"
initial = "0"

[[action]]
name = "Inc"
from = ["Counting"]
to = "Counting"
effect = "increment n"
"#,
    );
    for i in 1..6 {
        hub.push_str(&format!(
            r#"
[[action.triggers]]
name = "fanout_{i}"
kind = "entity"
to_state = "Idle"
target_entity = "Counter{i}"
target_action = "Inc"
drop_ok = true

[action.triggers.resolve_target]
type = "same_id"
"#
        ));
    }
    let mut specs = vec![hub];
    for i in 1..6 {
        specs.push(format!(
            r#"
[automaton]
name = "Counter{i}"
states = ["Counting"]
initial = "Counting"
allow_indefinite_states = ["Counting"]

[[state]]
name = "n"
type = "counter"
initial = "0"

[[action]]
name = "Inc"
from = ["Counting"]
to = "Counting"
effect = "increment n"
"#
        ));
    }
    let parsed: Vec<_> = specs.iter().map(|s| parse_automaton(s).unwrap()).collect();
    let refs: Vec<&Automaton> = parsed.iter().collect();
    let result = verify_composite_with_budget(&refs, "Counter0", 100).unwrap();
    assert_eq!(
        result.outcome,
        CompositeOutcome::Incomplete,
        "exhausted budget must report INCOMPLETE: {result:?}"
    );
    assert!(!result.passed(), "incomplete is never a pass");
}
