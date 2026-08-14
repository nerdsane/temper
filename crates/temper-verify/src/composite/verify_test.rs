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
fn seed_cover_joins_entities_coupled_only_by_a_guard() {
    let file = parse_automaton(
        r#"
[automaton]
name = "File"
states = ["Draft", "Ready"]
initial = "Draft"
allow_indefinite_states = ["Draft", "Ready"]

[[action]]
name = "Submit"
from = ["Draft"]
to = "Ready"
guard = [{ type = "cross_entity_state", entity_type = "Workspace", entity_id_source = "workspace_id", required_status = ["Active"] }]
"#,
    )
    .unwrap();
    let workspace = parse_automaton(
        r#"
[automaton]
name = "Workspace"
states = ["Active", "Frozen"]
initial = "Active"
allow_indefinite_states = ["Active", "Frozen"]

[[action]]
name = "Freeze"
from = ["Active"]
to = "Frozen"
"#,
    )
    .unwrap();
    let seeds = seed_cover(&[&file, &workspace]);
    assert_eq!(
        seeds,
        vec!["File".to_string()],
        "a status guard is a joint coupling even with no trigger"
    );
    let result = verify_composite(&[&file, &workspace], "File").unwrap();
    assert!(
        result.scope.contains(&"Workspace".to_string()),
        "scope must include the entity the guard reads: {:?}",
        result.scope
    );
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
fn katagami_shaped_submit_cannot_record_verdict() {
    // The live Katagami join: Language.SubmitForReview must not fire
    // Reviewer.RecordVerdict. RecordVerdict is only enabled from Reviewing;
    // the reviewer is still SubmissionReceived at submit time. If this
    // ever reports Verified, the composite checker is vacuous.
    let language = parse_automaton(
        r#"
[automaton]
name = "DesignLanguage"
states = ["Draft", "UnderReview"]
initial = "Draft"
allow_indefinite_states = ["Draft", "UnderReview"]

[[action]]
name = "SubmitForReview"
from = ["Draft"]
to = "UnderReview"

[[action.triggers]]
name = "submit_records_verdict_too_soon"
kind = "entity"
target_entity = "ReviewAgent"
target_action = "RecordVerdict"

[action.triggers.resolve_target]
type = "same_id"
"#,
    )
    .unwrap();
    let reviewer = parse_automaton(
        r#"
[automaton]
name = "ReviewAgent"
states = ["SubmissionReceived", "Reviewing", "VerdictRecorded"]
initial = "SubmissionReceived"
allow_indefinite_states = ["VerdictRecorded"]

[[action]]
name = "BeginReview"
from = ["SubmissionReceived"]
to = "Reviewing"

[[action]]
name = "RecordVerdict"
from = ["Reviewing"]
to = "VerdictRecorded"
"#,
    )
    .unwrap();
    let result = verify_composite(&[&language, &reviewer], "DesignLanguage").unwrap();
    assert_eq!(result.outcome, CompositeOutcome::Violated, "{result:?}");
    let drop = result
        .dropped_reactions
        .iter()
        .find(|d| d.target_action == "RecordVerdict")
        .expect("RecordVerdict drop must be reported");
    assert_eq!(drop.source_entity, "DesignLanguage");
    assert_eq!(drop.source_action, "SubmitForReview");
    assert_eq!(drop.target_entity, "ReviewAgent");
    assert_eq!(drop.target_state, "SubmissionReceived");
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

/// Six independent 4-status chains. Hub fans out a never-firing trigger
/// (gated on unreachable `Idle`) so all six land in one scope. Product is
/// 4^6 = 4096 unique joint statuses. Budget 100 must report INCOMPLETE.
fn independent_status_product(n: usize) -> Vec<Automaton> {
    let mut hub = String::from(
        r#"
[automaton]
name = "Chain0"
states = ["S0", "S1", "S2", "S3", "Idle"]
initial = "S0"
allow_indefinite_states = ["S0", "S1", "S2", "S3", "Idle"]

[[action]]
name = "Step0"
from = ["S0"]
to = "S1"

[[action]]
name = "Step1"
from = ["S1"]
to = "S2"

[[action]]
name = "Step2"
from = ["S2"]
to = "S3"
"#,
    );
    for i in 1..n {
        hub.push_str(&format!(
            r#"
[[action.triggers]]
name = "fanout_{i}"
kind = "entity"
to_state = "Idle"
target_entity = "Chain{i}"
target_action = "Step0"
drop_ok = true

[action.triggers.resolve_target]
type = "same_id"
"#
        ));
    }
    let mut specs = vec![hub];
    for i in 1..n {
        specs.push(format!(
            r#"
[automaton]
name = "Chain{i}"
states = ["S0", "S1", "S2", "S3"]
initial = "S0"
allow_indefinite_states = ["S0", "S1", "S2", "S3"]

[[action]]
name = "Step0"
from = ["S0"]
to = "S1"

[[action]]
name = "Step1"
from = ["S1"]
to = "S2"

[[action]]
name = "Step2"
from = ["S2"]
to = "S3"
"#
        ));
    }
    specs.iter().map(|s| parse_automaton(s).unwrap()).collect()
}

#[test]
fn incomplete_when_budget_exhausted() {
    let parsed = independent_status_product(6);
    let refs: Vec<&Automaton> = parsed.iter().collect();
    let result = verify_composite_with_budget(&refs, "Chain0", 100).unwrap();
    assert_eq!(
        result.outcome,
        CompositeOutcome::Incomplete,
        "exhausted budget must report INCOMPLETE: {result:?}"
    );
    assert!(!result.passed(), "incomplete is never a pass");
}

#[test]
fn status_product_of_eight_chains_completes_under_million() {
    // 4^8 = 65_536 unique statuses. After join-vector projection this is
    // the shape of a multi-app compose; it must finish under the default
    // unique budget rather than report Incomplete.
    let parsed = independent_status_product(8);
    let refs: Vec<&Automaton> = parsed.iter().collect();
    let result = verify_composite_with_budget(&refs, "Chain0", 100_000).unwrap();
    assert_eq!(result.outcome, CompositeOutcome::Verified, "{result:?}");
    assert!(
        result.states_explored > 60_000 && result.states_explored < 70_000,
        "expected ~4^8 unique statuses, got {}",
        result.states_explored
    );
}

#[test]
fn fat_catalog_bools_do_not_multiply_joint_states() {
    // A catalog type with 8 independent flags has 2^8 local combinations.
    // Composite only reads `status` (the actor's cross-entity guard), so
    // those flags must not appear in the joint vector. Expected unique
    // states: Language {{Draft, Published}} × Actor {{Idle, Done}} = 4.
    let language = r#"
[automaton]
name = "Language"
states = ["Draft", "Published"]
initial = "Draft"
allow_indefinite_states = ["Draft", "Published"]

[[state]]
name = "flag0"
type = "bool"
initial = "false"

[[state]]
name = "flag1"
type = "bool"
initial = "false"

[[state]]
name = "flag2"
type = "bool"
initial = "false"

[[state]]
name = "flag3"
type = "bool"
initial = "false"

[[state]]
name = "flag4"
type = "bool"
initial = "false"

[[state]]
name = "flag5"
type = "bool"
initial = "false"

[[state]]
name = "flag6"
type = "bool"
initial = "false"

[[state]]
name = "flag7"
type = "bool"
initial = "false"

[[action]]
name = "SetFlag0"
from = ["Draft"]
to = "Draft"
effect = "set flag0 true"

[[action]]
name = "SetFlag1"
from = ["Draft"]
to = "Draft"
effect = "set flag1 true"

[[action]]
name = "SetFlag2"
from = ["Draft"]
to = "Draft"
effect = "set flag2 true"

[[action]]
name = "SetFlag3"
from = ["Draft"]
to = "Draft"
effect = "set flag3 true"

[[action]]
name = "SetFlag4"
from = ["Draft"]
to = "Draft"
effect = "set flag4 true"

[[action]]
name = "SetFlag5"
from = ["Draft"]
to = "Draft"
effect = "set flag5 true"

[[action]]
name = "SetFlag6"
from = ["Draft"]
to = "Draft"
effect = "set flag6 true"

[[action]]
name = "SetFlag7"
from = ["Draft"]
to = "Draft"
effect = "set flag7 true"

[[action]]
name = "Publish"
from = ["Draft"]
to = "Published"
"#;
    let actor = r#"
[automaton]
name = "Actor"
states = ["Idle", "Done"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Done"]

[[action]]
name = "Finish"
from = ["Idle"]
to = "Done"
guard = [
  { type = "cross_entity_state", entity_type = "Language", entity_id_source = "language_id", required_status = ["Published"] },
]
"#;
    let language = parse_automaton(language).unwrap();
    let actor = parse_automaton(actor).unwrap();
    let result = verify_composite(&[&language, &actor], "Actor").unwrap();
    assert_eq!(result.outcome, CompositeOutcome::Verified, "{result:?}");
    assert!(
        result.states_explored < 20,
        "join vector is status-only; 8 catalog flags must not multiply unique joint states, got {}",
        result.states_explored
    );
}

#[test]
fn unique_budget_counts_states_not_generated_edges() {
    // Three statuses × 20 self-loops produce many generated edges per
    // unique state. Coupled to a 3-status peer via a never-firing
    // trigger. Unique space is 3×3 = 9. A unique budget of 50 must
    // exhaust that space (VERIFIED), not stop on generated-edge count.
    let mut hub = String::from(
        r#"
[automaton]
name = "Busy0"
states = ["A", "B", "C", "Idle"]
initial = "A"
allow_indefinite_states = ["A", "B", "C", "Idle"]

[[action]]
name = "ToB"
from = ["A"]
to = "B"

[[action]]
name = "ToC"
from = ["B"]
to = "C"
"#,
    );
    for i in 0..20 {
        hub.push_str(&format!(
            r#"
[[action]]
name = "StayA{i}"
from = ["A"]
to = "A"

[[action]]
name = "StayB{i}"
from = ["B"]
to = "B"

[[action]]
name = "StayC{i}"
from = ["C"]
to = "C"
"#
        ));
    }
    hub.push_str(
        r#"
[[action.triggers]]
name = "fanout"
kind = "entity"
to_state = "Idle"
target_entity = "Busy1"
target_action = "ToB"
drop_ok = true

[action.triggers.resolve_target]
type = "same_id"
"#,
    );
    let mut peer = String::from(
        r#"
[automaton]
name = "Busy1"
states = ["A", "B", "C"]
initial = "A"
allow_indefinite_states = ["A", "B", "C"]

[[action]]
name = "ToB"
from = ["A"]
to = "B"

[[action]]
name = "ToC"
from = ["B"]
to = "C"
"#,
    );
    for i in 0..20 {
        peer.push_str(&format!(
            r#"
[[action]]
name = "StayA{i}"
from = ["A"]
to = "A"

[[action]]
name = "StayB{i}"
from = ["B"]
to = "B"

[[action]]
name = "StayC{i}"
from = ["C"]
to = "C"
"#
        ));
    }
    let busy0 = parse_automaton(&hub).unwrap();
    let busy1 = parse_automaton(&peer).unwrap();
    let result = verify_composite_with_budget(&[&busy0, &busy1], "Busy0", 50).unwrap();
    assert_eq!(
        result.outcome,
        CompositeOutcome::Verified,
        "unique space is 9; budget 50 must complete, not stop on generated edges: {result:?}"
    );
    assert!(
        result.states_explored < 20,
        "unique joint states should be the 3×3 status product, got {}",
        result.states_explored
    );
}
