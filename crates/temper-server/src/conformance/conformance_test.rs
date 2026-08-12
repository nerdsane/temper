//! Conformance checker tests.
//!
//! Every violation kind gets a case asserting the exact index and kind, so a
//! change in walk order or in what counts as a violation shows up as a failing
//! assertion rather than a silently different report.

use super::*;
use temper_ots::models::{
    DecisionType, OTSChoice, OTSConsequence, OTSDecision, OTSMetadata, OTSTrajectory, OTSTurn,
    OutcomeType,
};
use temper_spec::automaton::parse_automaton;

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

/// An actor whose only action constrains its source states through a
/// `state_in` guard rather than a `from` list.
const GUARD_ONLY_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "Working", "Closed"]
initial = "Open"

[[action]]
name = "Work"
kind = "input"
guard = [{ type = "state_in", values = ["Open"] }]
to = "Working"

[[action]]
name = "Close"
kind = "input"
from = ["Working"]
to = "Closed"
"#;

/// An actor with an always-enabled input action: no `from`, no `state_in`.
const ALWAYS_ENABLED_IOA: &str = r#"
[automaton]
name = "Beacon"
states = ["Active", "Closed"]
initial = "Active"

[[action]]
name = "Close"
kind = "input"
from = ["Active"]
to = "Closed"

[[action]]
name = "Heartbeat"
kind = "input"
"#;

fn order_automaton() -> temper_spec::automaton::Automaton {
    parse_automaton(ORDER_IOA).expect("order fixture parses")
}

/// A successful entity-sourced row for `action`, moving `from` -> `to`.
fn row(action: &str, from: Option<&str>, to: Option<&str>) -> TursoTrajectoryRow {
    TursoTrajectoryRow {
        tenant: "default".to_string(),
        entity_type: "Order".to_string(),
        entity_id: "order-1".to_string(),
        action: action.to_string(),
        success: true,
        from_status: from.map(str::to_string),
        to_status: to.map(str::to_string),
        error: None,
        agent_id: Some("agent-1".to_string()),
        session_id: Some("session-1".to_string()),
        authz_denied: None,
        denied_resource: None,
        denied_module: None,
        source: Some("Entity".to_string()),
        spec_governed: Some(true),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        request_body: None,
        intent: None,
        matched_policy_ids: None,
        capture_seq: None,
    }
}

fn denied(action: &str, from: Option<&str>) -> TursoTrajectoryRow {
    TursoTrajectoryRow {
        success: false,
        to_status: None,
        authz_denied: Some(true),
        error: Some("Cedar denied".to_string()),
        source: Some("Authz".to_string()),
        ..row(action, from, None)
    }
}

fn failed(action: &str, from: Option<&str>) -> TursoTrajectoryRow {
    TursoTrajectoryRow {
        success: false,
        to_status: None,
        error: Some("dispatch failed".to_string()),
        ..row(action, from, None)
    }
}

/// Run the checker over a complete read of a run whose governing spec is
/// known: the tests that care about truncation or about an unresolved
/// governing spec build their own [`ConformanceInput`].
fn check(
    automaton: &temper_spec::automaton::Automaton,
    kernel_rows: &[TursoTrajectoryRow],
    ots_trajectory: Option<&OTSTrajectory>,
) -> ConformanceReport {
    check_conformance(ConformanceInput {
        automaton,
        kernel_rows,
        ots_trajectory,
        rows_truncated: false,
        spec_resolution: SpecResolution::Pinned,
        capture_degraded: false,
    })
}

fn only_violation(report: &ConformanceReport) -> &Violation {
    assert_eq!(
        report.violations.len(),
        1,
        "expected exactly one violation, got {:?}",
        report.violations
    );
    &report.violations[0]
}

#[test]
fn a_legal_run_passes_with_no_violations() {
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        row("SubmitOrder", Some("Draft"), Some("Submitted")),
        row("ConfirmOrder", Some("Submitted"), Some("Confirmed")),
        row("ProcessOrder", Some("Confirmed"), Some("Processing")),
        row("ShipOrder", Some("Processing"), Some("Shipped")),
        row("DeliverOrder", Some("Shipped"), Some("Delivered")),
    ];

    let report = check(&order_automaton(), &rows, None);

    assert!(report.passed, "violations: {:?}", report.violations);
    assert!(report.violations.is_empty());
    assert_eq!(report.stats.stream_length, 6);
    assert_eq!(report.stats.actor_rows, 6);
    assert_eq!(report.stats.transitions_unchecked, 0);
    assert_eq!(
        report.stats.terminal_entities, 0,
        "Delivered still has InitiateReturn leaving it, so it is not terminal"
    );
    assert!(report.stats.violations_by_kind.is_empty());
}

#[test]
fn illegal_transition_reports_the_offending_index() {
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        // ShipOrder is legal only from Processing.
        row("ShipOrder", Some("Draft"), Some("Shipped")),
    ];

    let report = check(&order_automaton(), &rows, None);

    assert!(!report.passed);
    let violation = only_violation(&report);
    assert_eq!(violation.index, 1);
    assert_eq!(violation.kind, ViolationKind::IllegalTransition);
    assert_eq!(violation.action, "ShipOrder");
    assert_eq!(violation.entity_type, "Order");
    assert!(
        violation.detail.contains("Draft") && violation.detail.contains("Processing"),
        "detail must name both the observed and the legal states: {}",
        violation.detail
    );
    assert_eq!(report.stats.violations_by_kind["illegal_transition"], 1);
}

#[test]
fn a_state_in_guard_stands_in_for_a_missing_from_list() {
    let automaton = parse_automaton(GUARD_ONLY_IOA).expect("guard fixture parses");
    // `Working` is neither terminal nor in Work's guard, so the only thing
    // that can flag this row is the transition check reading the guard.
    let rows = vec![TursoTrajectoryRow {
        entity_type: "Ticket".to_string(),
        ..row("Work", Some("Working"), Some("Working"))
    }];

    let report = check(&automaton, &rows, None);

    let violation = only_violation(&report);
    assert_eq!(violation.index, 0);
    assert_eq!(violation.kind, ViolationKind::IllegalTransition);
    assert!(
        violation.detail.contains("Open"),
        "the guard's values are the legal source set: {}",
        violation.detail
    );
}

#[test]
fn a_guarded_action_keeps_its_source_states_out_of_the_terminal_set() {
    let automaton = parse_automaton(GUARD_ONLY_IOA).expect("guard fixture parses");
    // `Open` is listed in no action's `from`; only Work's `state_in` guard
    // names it. Reading `from` alone would call it terminal.
    let rows = vec![TursoTrajectoryRow {
        entity_type: "Ticket".to_string(),
        ..row("Work", Some("Open"), Some("Working"))
    }];

    let report = check(&automaton, &rows, None);

    assert!(report.passed, "violations: {:?}", report.violations);
    assert_eq!(report.stats.terminal_entities, 0);
}

#[test]
fn forbidden_action_flags_a_platform_action_the_actor_does_not_declare() {
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        // `Created` is a name the kernel emits; the Order spec never declares
        // it, and this row came through the entity dispatch path.
        row("Created", Some("Draft"), Some("Draft")),
    ];

    let report = check(&order_automaton(), &rows, None);

    let violation = only_violation(&report);
    assert_eq!(violation.index, 1);
    assert_eq!(violation.kind, ViolationKind::ForbiddenAction);
    assert_eq!(violation.action, "Created");
    assert!(violation.detail.contains("platform defines"));
}

#[test]
fn unknown_action_flags_a_name_no_spec_defines() {
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        row("Frobnicate", Some("Draft"), None),
    ];

    let report = check(&order_automaton(), &rows, None);

    let violation = only_violation(&report);
    assert_eq!(violation.index, 1);
    assert_eq!(violation.kind, ViolationKind::UnknownAction);
    assert_eq!(violation.action, "Frobnicate");
    assert!(violation.detail.contains("no `[[action]]`"));
}

#[test]
fn post_terminal_flags_every_action_after_the_terminal_transition() {
    let rows = vec![
        row("CancelOrder", Some("Draft"), Some("Cancelled")),
        row("AddItem", Some("Cancelled"), Some("Cancelled")),
    ];

    let report = check(&order_automaton(), &rows, None);

    let violation = only_violation(&report);
    assert_eq!(violation.index, 1);
    assert_eq!(violation.kind, ViolationKind::PostTerminal);
    assert_eq!(violation.action, "AddItem");
    assert!(
        violation.detail.contains("index 0"),
        "detail must point back at the terminal transition: {}",
        violation.detail
    );
    assert_eq!(report.stats.terminal_entities, 1);
    assert!(
        !report
            .stats
            .violations_by_kind
            .contains_key("illegal_transition"),
        "post_terminal suppresses the redundant illegal_transition on the same row"
    );
}

#[test]
fn post_terminal_is_caught_when_the_session_starts_after_the_terminal_transition() {
    // No row in this session drove the entity into Cancelled; the source state
    // alone has to be enough.
    let rows = vec![row("AddItem", Some("Cancelled"), Some("Cancelled"))];

    let report = check(&order_automaton(), &rows, None);

    let violation = only_violation(&report);
    assert_eq!(violation.index, 0);
    assert_eq!(violation.kind, ViolationKind::PostTerminal);
    assert!(violation.detail.contains("Cancelled"));
}

#[test]
fn denied_then_retried_flags_a_blind_retry() {
    let rows = vec![
        denied("SubmitOrder", Some("Draft")),
        failed("SubmitOrder", Some("Draft")),
    ];

    let report = check(&order_automaton(), &rows, None);

    let violation = only_violation(&report);
    assert_eq!(violation.index, 1);
    assert_eq!(violation.kind, ViolationKind::DeniedThenRetried);
    assert_eq!(violation.action, "SubmitOrder");
    assert!(
        violation.detail.contains("index 0"),
        "detail must point back at the denial: {}",
        violation.detail
    );
}

#[test]
fn a_retry_after_a_state_change_is_not_a_violation() {
    let rows = vec![
        denied("CancelOrder", Some("Draft")),
        row("SubmitOrder", Some("Draft"), Some("Submitted")),
        failed("CancelOrder", Some("Submitted")),
    ];

    let report = check(&order_automaton(), &rows, None);

    assert!(
        report.passed,
        "the entity moved between denial and retry: {:?}",
        report.violations
    );
}

#[test]
fn a_retry_that_succeeds_is_not_a_violation() {
    let rows = vec![
        denied("SubmitOrder", Some("Draft")),
        row("SubmitOrder", Some("Draft"), Some("Submitted")),
    ];

    let report = check(&order_automaton(), &rows, None);

    assert!(
        report.passed,
        "authorization allowed the retry, so an approval landed: {:?}",
        report.violations
    );
}

#[test]
fn a_denial_on_a_different_entity_does_not_arm_the_retry_check() {
    let rows = vec![
        denied("SubmitOrder", Some("Draft")),
        TursoTrajectoryRow {
            entity_id: "order-2".to_string(),
            ..failed("SubmitOrder", Some("Draft"))
        },
    ];

    let report = check(&order_automaton(), &rows, None);

    assert!(report.passed, "violations: {:?}", report.violations);
}

#[test]
fn three_denials_report_each_blind_retry() {
    let rows = vec![
        denied("SubmitOrder", Some("Draft")),
        denied("SubmitOrder", Some("Draft")),
        denied("SubmitOrder", Some("Draft")),
    ];

    let report = check(&order_automaton(), &rows, None);

    let indices: Vec<usize> = report.violations.iter().map(|v| v.index).collect();
    assert_eq!(indices, vec![1, 2]);
    assert!(
        report
            .violations
            .iter()
            .all(|v| v.kind == ViolationKind::DeniedThenRetried)
    );
    assert_eq!(report.stats.violations_by_kind["denied_then_retried"], 2);
}

#[test]
fn platform_rows_and_other_entities_are_counted_but_not_judged() {
    let rows = vec![
        // A platform-sourced bookkeeping row naming an action Order never
        // declares: kernel bookkeeping, not an actor action.
        TursoTrajectoryRow {
            source: Some("Platform".to_string()),
            ..row("EntitySetNotFound", None, None)
        },
        // Another actor's row; this checker was given only the Order spec.
        TursoTrajectoryRow {
            entity_type: "Invoice".to_string(),
            ..row("Frobnicate", Some("Nowhere"), None)
        },
        row("AddItem", Some("Draft"), Some("Draft")),
    ];

    let report = check(&order_automaton(), &rows, None);

    assert!(report.passed, "violations: {:?}", report.violations);
    assert_eq!(report.stats.stream_length, 3);
    assert_eq!(report.stats.actor_rows, 1);
    assert_eq!(report.stats.platform_rows_skipped, 1);
    assert_eq!(report.stats.other_entity_rows_skipped, 1);
}

#[test]
fn a_row_without_a_source_state_is_counted_as_unchecked() {
    let rows = vec![row("ShipOrder", None, Some("Shipped"))];

    let report = check(&order_automaton(), &rows, None);

    assert!(report.violations.is_empty(), "{:?}", report.violations);
    assert_eq!(report.stats.transitions_unchecked, 1);
    assert_eq!(
        report.verdict,
        Verdict::Indeterminate,
        "a transition that could not be checked is not a transition that passed"
    );
    assert!(!report.passed);
    assert!(
        report
            .evidence_gaps
            .iter()
            .any(|gap| gap.contains("source state")),
        "the gap must say what was missing: {:?}",
        report.evidence_gaps
    );
}

fn ots_with_decisions(actions: &[&str]) -> OTSTrajectory {
    let now = "2026-01-01T00:00:00Z"
        .parse::<chrono::DateTime<chrono::Utc>>()
        .expect("timestamp parses");
    let mut turn = OTSTurn::new(1, now);
    for action in actions {
        turn = turn.with_decision(OTSDecision::new(
            DecisionType::ToolSelection,
            OTSChoice::new(*action),
            OTSConsequence::failure(),
        ));
    }
    OTSTrajectory::new(OTSMetadata::new(
        "task",
        "agent-1",
        OutcomeType::Failure,
        now,
    ))
    .with_turn(turn)
}

#[test]
fn a_declared_decision_over_an_empty_session_cannot_pass() {
    // The fail-open this test exists for: a decision naming an action the spec
    // declares raises no violation, and it used to be enough to suppress the
    // "nothing was checked" gap. A caller able to upload a trajectory could
    // then get `passed: true` for a session the kernel has no row for — a run
    // nobody has any evidence ever happened.
    let ots = ots_with_decisions(&["AddItem"]);

    let report = check(&order_automaton(), &[], Some(&ots));

    assert!(report.violations.is_empty(), "AddItem is declared");
    assert_eq!(report.stats.actor_rows, 0);
    assert_eq!(report.stats.ots_decisions_checked, 1);
    assert_eq!(
        report.verdict,
        Verdict::Indeterminate,
        "a decision proves an action name is in the spec, not that the run followed it"
    );
    assert!(!report.passed);
    assert!(!report.evidence_complete);
}

#[test]
fn decisions_never_substitute_for_the_rows_a_run_did_not_leave() {
    // The same shape with several declared decisions: quantity of agent-side
    // claims must not add up to evidence the kernel never recorded.
    let ots = ots_with_decisions(&["AddItem", "SubmitOrder", "ConfirmOrder"]);

    let report = check(&order_automaton(), &[], Some(&ots));

    assert!(report.violations.is_empty());
    assert_eq!(report.stats.ots_decisions_checked, 3);
    assert!(!report.passed);
    assert!(
        report
            .evidence_gaps
            .iter()
            .any(|gap| gap.contains("no governed action")),
        "{:?}",
        report.evidence_gaps
    );
}

#[test]
fn a_run_with_no_actor_rows_still_fails_on_a_disagreeing_decision() {
    // The other half of the rule: decisions still *fail* a run. Only the path
    // to Pass is closed to them.
    //
    // `Delete` rather than an invented name: the platform defines it, so the
    // kernel can place it as an action and read the decision as a claim about
    // one. A name nothing in this deployment defines cannot be told apart from
    // a harness tool — see `conformance::decisions` for the rule.
    let ots = ots_with_decisions(&["Delete"]);

    let report = check(&order_automaton(), &[], Some(&ots));

    assert_eq!(report.verdict, Verdict::Fail);
    assert_eq!(only_violation(&report).kind, ViolationKind::ForbiddenAction);
}

#[test]
fn ots_decisions_the_kernel_never_recorded_are_checked_after_the_rows() {
    let rows = vec![row("AddItem", Some("Draft"), Some("Draft"))];
    // `AddItem` already has a row and must not be double-counted; `Delete` is a
    // platform verb the Order spec never declares and no row accounts for.
    let ots = ots_with_decisions(&["AddItem", "Delete"]);

    let report = check(&order_automaton(), &rows, Some(&ots));

    let violation = only_violation(&report);
    assert_eq!(
        violation.index, 1,
        "OTS decisions are indexed after the kernel rows"
    );
    assert_eq!(violation.kind, ViolationKind::ForbiddenAction);
    assert_eq!(violation.action, "Delete");
    assert!(violation.detail.contains("never recorded a row"));
    assert_eq!(report.stats.ots_decisions_checked, 1);
    assert_eq!(report.stats.stream_length, 2);
}

#[test]
fn ots_decisions_on_declared_actions_raise_nothing() {
    let rows = vec![row("AddItem", Some("Draft"), Some("Draft"))];
    let ots = ots_with_decisions(&["CancelOrder"]);

    let report = check(&order_automaton(), &rows, Some(&ots));

    assert!(report.passed, "violations: {:?}", report.violations);
    assert_eq!(report.stats.ots_decisions_checked, 1);
}

#[test]
fn an_empty_session_is_indeterminate_rather_than_passing() {
    let report = check(&order_automaton(), &[], None);

    assert_eq!(
        report.verdict,
        Verdict::Indeterminate,
        "a session with no rows is no evidence of conformance"
    );
    assert!(
        !report.passed,
        "a consumer gating on `passed` must not accept a run nobody checked"
    );
    assert!(report.violations.is_empty());
    assert_eq!(report.stats.stream_length, 0);
    assert_eq!(report.stats.actor_rows, 0);
    assert!(
        report
            .evidence_gaps
            .iter()
            .any(|gap| gap.contains("no governed action")),
        "{:?}",
        report.evidence_gaps
    );
}

#[test]
fn a_truncated_read_is_indeterminate_even_with_no_violations() {
    let rows = vec![row("AddItem", Some("Draft"), Some("Draft"))];

    let report = check_conformance(ConformanceInput {
        automaton: &order_automaton(),
        kernel_rows: &rows,
        ots_trajectory: None,
        rows_truncated: true,
        spec_resolution: SpecResolution::Pinned,
        capture_degraded: false,
    });

    assert!(report.violations.is_empty());
    assert_eq!(
        report.verdict,
        Verdict::Indeterminate,
        "the unread tail of the session could hold anything"
    );
    assert!(!report.evidence_complete);
    assert!(!report.passed);
    assert!(
        report
            .evidence_gaps
            .iter()
            .any(|gap| gap.contains("row cap")),
        "{:?}",
        report.evidence_gaps
    );
}

#[test]
fn a_violation_in_a_truncated_prefix_still_fails() {
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        row("ShipOrder", Some("Draft"), Some("Shipped")),
    ];

    let report = check_conformance(ConformanceInput {
        automaton: &order_automaton(),
        kernel_rows: &rows,
        ots_trajectory: None,
        rows_truncated: true,
        spec_resolution: SpecResolution::Pinned,
        capture_degraded: false,
    });

    assert_eq!(
        report.verdict,
        Verdict::Fail,
        "a disagreement found in a prefix is still a disagreement"
    );
    assert!(!report.passed);
}

#[test]
fn a_gap_between_two_individually_legal_rows_is_a_violation() {
    // Both source states are legal for their own action. Nothing recorded ever
    // moved the entity from Submitted to Processing.
    let rows = vec![
        row("SubmitOrder", Some("Draft"), Some("Submitted")),
        row("ShipOrder", Some("Processing"), Some("Shipped")),
    ];

    let report = check(&order_automaton(), &rows, None);

    let violation = only_violation(&report);
    assert_eq!(violation.index, 1);
    assert_eq!(violation.kind, ViolationKind::StateDiscontinuity);
    assert!(
        violation.detail.contains("Submitted") && violation.detail.contains("Processing"),
        "detail must name both ends of the gap: {}",
        violation.detail
    );
}

#[test]
fn a_success_landing_somewhere_the_action_does_not_go_is_a_violation() {
    // SubmitOrder is legal from Draft and lands in Submitted, never Cancelled.
    let rows = vec![row("SubmitOrder", Some("Draft"), Some("Cancelled"))];

    let report = check(&order_automaton(), &rows, None);

    let violation = only_violation(&report);
    assert_eq!(violation.index, 0);
    assert_eq!(violation.kind, ViolationKind::UnexpectedTargetState);
    assert!(
        violation.detail.contains("Cancelled") && violation.detail.contains("Submitted"),
        "detail must name both the observed and the declared target: {}",
        violation.detail
    );
}

#[test]
fn an_action_with_no_target_must_leave_the_state_where_it_was() {
    // AddItem declares no `to`, so the state holds.
    let rows = vec![row("AddItem", Some("Draft"), Some("Submitted"))];

    let report = check(&order_automaton(), &rows, None);

    let violation = only_violation(&report);
    assert_eq!(violation.kind, ViolationKind::UnexpectedTargetState);
    assert!(
        violation.detail.contains("declares no `to`"),
        "{}",
        violation.detail
    );
}

#[test]
fn an_always_enabled_action_empties_the_terminal_set() {
    // `Heartbeat` is an input action with neither `from` nor a state_in guard,
    // so the kernel enables it from every state — including `Closed`, which no
    // other action lists as a source.
    let automaton = parse_automaton(ALWAYS_ENABLED_IOA).expect("fixture parses");
    let rows = vec![TursoTrajectoryRow {
        entity_type: "Beacon".to_string(),
        ..row("Heartbeat", Some("Closed"), Some("Closed"))
    }];

    let report = check(&automaton, &rows, None);

    assert!(
        report.passed,
        "an always-enabled action is legal from a state nothing else leaves: {:?}",
        report.violations
    );
    assert_eq!(report.stats.terminal_entities, 0);
}

#[test]
fn a_caller_supplied_audit_row_cannot_inject_a_violation() {
    // POST /api/audit writes rows with a caller-chosen session, entity type,
    // and action name, marked spec_governed = false.
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        TursoTrajectoryRow {
            spec_governed: Some(false),
            ..row("Frobnicate", Some("Draft"), Some("Draft"))
        },
    ];

    let report = check(&order_automaton(), &rows, None);

    assert!(
        report.passed,
        "a non-governed audit record is not this actor executing its spec: {:?}",
        report.violations
    );
    assert_eq!(report.stats.non_governed_rows_skipped, 1);
    assert_eq!(report.stats.actor_rows, 1);
}

#[test]
fn a_row_on_another_entity_does_not_account_for_a_decision_on_this_one() {
    // The kernel recorded PayInvoice against Invoice. That says nothing about
    // whether the agent's PayInvoice decision against Order ever reached the
    // governed path.
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        TursoTrajectoryRow {
            entity_type: "Invoice".to_string(),
            ..row("PayInvoice", Some("Due"), Some("Paid"))
        },
    ];
    let ots = ots_with_decisions(&["PayInvoice"]);

    let report = check(&order_automaton(), &rows, Some(&ots));

    let violation = only_violation(&report);
    assert_eq!(violation.kind, ViolationKind::UnknownAction);
    assert_eq!(violation.action, "PayInvoice");
    assert_eq!(violation.entity_type, "Order");
}

#[test]
fn a_thinking_decision_is_not_reported_as_an_action() {
    let rows = vec![row("AddItem", Some("Draft"), Some("Draft"))];
    let now = "2026-01-01T00:00:00Z"
        .parse::<chrono::DateTime<chrono::Utc>>()
        .expect("timestamp parses");
    let ots = OTSTrajectory::new(OTSMetadata::new(
        "task",
        "agent-1",
        OutcomeType::Success,
        now,
    ))
    .with_turn(OTSTurn::new(1, now).with_decision(OTSDecision::new(
        DecisionType::ReasoningStep,
        OTSChoice::new("compare shipping options"),
        OTSConsequence::success(),
    )));

    let report = check(&order_automaton(), &rows, Some(&ots));

    assert!(
        report.passed,
        "a reasoning step names a thought, not a callable: {:?}",
        report.violations
    );
    assert_eq!(report.stats.ots_decisions_checked, 0);
    assert_eq!(report.stats.ots_decisions_skipped_as_thinking, 1);
}

/// The row the capture path writes when it fails to store an entry for a
/// session (`crate::trajectory_outbox::capture_loss_marker`).
fn capture_loss_marker_row() -> TursoTrajectoryRow {
    TursoTrajectoryRow {
        entity_type: CAPTURE_LOSS_ENTITY_TYPE.to_string(),
        entity_id: "session-1".to_string(),
        success: false,
        to_status: None,
        from_status: None,
        source: Some("Platform".to_string()),
        spec_governed: Some(false),
        error: Some("trajectory capture lost at least one entry for this session".to_string()),
        ..row(CAPTURE_LOSS_ACTION, None, None)
    }
}

#[test]
fn a_run_missing_captured_rows_cannot_pass() {
    // Every row the checker can see agrees with the spec. The marker says the
    // ones it cannot see were never stored, so the run is unproven rather than
    // conforming.
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        capture_loss_marker_row(),
        row("SubmitOrder", Some("Draft"), Some("Submitted")),
    ];

    let report = check(&order_automaton(), &rows, None);

    assert!(report.violations.is_empty());
    assert_eq!(report.stats.capture_loss_markers, 1);
    assert_eq!(
        report.stats.actor_rows, 2,
        "the marker is not an action and must not be judged as one"
    );
    assert_eq!(report.verdict, Verdict::Indeterminate);
    assert!(
        !report.passed,
        "a run whose record is known to have holes in it cannot pass"
    );
    assert!(!report.evidence_complete);
    assert!(
        report
            .evidence_gaps
            .iter()
            .any(|gap| gap.contains("loss marker")),
        "{:?}",
        report.evidence_gaps
    );
}

#[test]
fn a_capture_loss_marker_is_not_read_as_another_entity_s_row() {
    // The marker carries the capture path's own entity type. Classified by the
    // entity comparison alone it would be counted as another actor's row and
    // the evidence gap would vanish.
    let rows = vec![capture_loss_marker_row()];

    let report = check(&order_automaton(), &rows, None);

    assert_eq!(report.stats.capture_loss_markers, 1);
    assert_eq!(report.stats.other_entity_rows_skipped, 0);
}

#[test]
fn a_run_whose_governing_spec_is_unresolved_cannot_pass() {
    let rows = vec![row("AddItem", Some("Draft"), Some("Draft"))];

    let report = check_conformance(ConformanceInput {
        automaton: &order_automaton(),
        kernel_rows: &rows,
        ots_trajectory: None,
        rows_truncated: false,
        spec_resolution: SpecResolution::Unresolved,
        capture_degraded: false,
    });

    assert!(report.violations.is_empty());
    assert_eq!(report.spec_resolution, SpecResolution::Unresolved);
    assert_eq!(
        report.verdict,
        Verdict::Indeterminate,
        "agreeing with a spec that may not be the one in force proves nothing"
    );
    assert!(!report.passed);
    assert!(!report.evidence_complete);
    assert!(
        report
            .evidence_gaps
            .iter()
            .any(|gap| gap.contains("spec version")),
        "{:?}",
        report.evidence_gaps
    );
}

#[test]
fn a_resolved_spec_and_a_complete_read_report_complete_evidence() {
    let rows = vec![row("AddItem", Some("Draft"), Some("Draft"))];

    let report = check(&order_automaton(), &rows, None);

    assert_eq!(report.spec_resolution, SpecResolution::Pinned);
    assert!(report.evidence_complete);
    assert!(report.passed);
}

/// One MCP `execute` turn: the decision names the submitted code, and the
/// governed actions the code called are recorded inside it
/// (`temper_mcp::runtime::record_execute_turn`).
fn mcp_execute_trajectory(code: &str, nested_actions: &[&str]) -> OTSTrajectory {
    let now = "2026-01-01T00:00:00Z"
        .parse::<chrono::DateTime<chrono::Utc>>()
        .expect("timestamp parses");
    let mut choice = OTSChoice::new(format!("execute: {code}"));
    if !nested_actions.is_empty() {
        choice = choice.with_arguments(serde_json::json!({
            "trajectory_actions": nested_actions
                .iter()
                .map(|action| serde_json::json!({ "action": action, "params": {} }))
                .collect::<Vec<_>>(),
        }));
    }
    OTSTrajectory::new(OTSMetadata::new(
        "task",
        "agent-1",
        OutcomeType::Success,
        now,
    ))
    .with_turn(OTSTurn::new(1, now).with_decision(OTSDecision::new(
        DecisionType::ToolSelection,
        choice,
        OTSConsequence::success(),
    )))
}

#[test]
fn an_mcp_execute_decision_is_not_reported_as_an_action() {
    // Every MCP-produced decision would otherwise surface as one
    // `unknown_action` violation naming a hundred characters of Python.
    let rows = vec![row("AddItem", Some("Draft"), Some("Draft"))];
    let ots = mcp_execute_trajectory("print('hello')", &[]);

    let report = check(&order_automaton(), &rows, Some(&ots));

    assert!(
        report.passed,
        "an execute envelope names the harness's tool, not this actor: {:?}",
        report.violations
    );
    assert_eq!(report.stats.ots_decisions_checked, 0);
    assert_eq!(report.stats.ots_decisions_skipped_as_harness_tool, 1);
}

#[test]
fn the_governed_actions_inside_an_execute_decision_are_checked() {
    // The envelope is not an action; the actions the code called are, and the
    // kernel recorded a row for neither.
    let rows = vec![row("AddItem", Some("Draft"), Some("Draft"))];
    let ots = mcp_execute_trajectory(
        "temper.action('default', 'Order', 'Frobnicate', {})",
        &["Frobnicate"],
    );

    let report = check(&order_automaton(), &rows, Some(&ots));

    let violation = only_violation(&report);
    assert_eq!(violation.kind, ViolationKind::UnknownAction);
    assert_eq!(violation.action, "Frobnicate");
    assert_eq!(report.stats.ots_decisions_checked, 1);
    assert_eq!(report.stats.ots_decisions_skipped_as_harness_tool, 0);
}

#[test]
fn a_governed_action_inside_an_execute_decision_that_reached_the_kernel_is_not_double_counted() {
    let rows = vec![row("AddItem", Some("Draft"), Some("Draft"))];
    let ots = mcp_execute_trajectory(
        "temper.action('default', 'Order', 'AddItem', {})",
        &["AddItem"],
    );

    let report = check(&order_automaton(), &rows, Some(&ots));

    assert!(report.passed, "{:?}", report.violations);
    assert_eq!(report.stats.ots_decisions_checked, 0);
}

#[test]
fn an_undeclared_action_that_moved_the_entity_is_reported_once() {
    // The undeclared row is the fault. If the walk forgot where that row left
    // the entity, the next legal row would look like a discontinuity and the
    // same fault would be reported a second time under another kind.
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        row("Frobnicate", Some("Draft"), Some("Submitted")),
        row("ConfirmOrder", Some("Submitted"), Some("Confirmed")),
    ];

    let report = check(&order_automaton(), &rows, None);

    let violation = only_violation(&report);
    assert_eq!(violation.index, 1);
    assert_eq!(violation.kind, ViolationKind::UnknownAction);
}

/// The tools a Claude Code trajectory records one decision apiece for. Every
/// one of these has the shape of an action name, which is why they were being
/// judged as governed-action claims.
const HARNESS_TOOLS: &[&str] = &[
    "Bash",
    "Read",
    "Write",
    "Edit",
    "Agent",
    "ToolSearch",
    "SendMessage",
    "mcp__linear__list_issues",
];

#[test]
fn harness_tool_decisions_are_counted_rather_than_reported() {
    // The regression this fixture exists for: a clean Claude Code run against
    // a conforming session returned `fail` with one `unknown_action` per tool
    // call — 81 of them in the run that found this — each saying the agent
    // "decided on `Bash`, which the kernel never recorded a row for".
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        row("SubmitOrder", Some("Draft"), Some("Submitted")),
    ];
    let ots = ots_with_decisions(HARNESS_TOOLS);

    let report = check(&order_automaton(), &rows, Some(&ots));

    assert!(
        report.passed,
        "the agent's own tools are not this actor's alphabet: {:?}",
        report.violations
    );
    assert_eq!(
        report.stats.ots_decisions_skipped_as_unrecognized_name,
        HARNESS_TOOLS.len()
    );
    assert_eq!(report.stats.ots_decisions_checked, 0);
    assert_eq!(
        report.stats.ots_decisions_skipped_as_harness_tool, 0,
        "a bare tool name is not the MCP envelope shape, and the two counts say \
         different things"
    );
}

#[test]
fn harness_tools_mixed_with_governed_actions_leave_the_governed_ones_judged() {
    // The realistic stream: tool calls interleaved with the actor's own
    // actions. Skipping the tools must not skip anything else.
    let rows = vec![row("AddItem", Some("Draft"), Some("Draft"))];
    let mut stream: Vec<&str> = vec!["Bash", "AddItem", "Read"];
    // Declared, and no row accounts for it: judged, and raises nothing.
    stream.push("CancelOrder");
    // A platform verb the Order spec does not declare: still reported.
    stream.extend(["Edit", "Delete"]);
    let ots = ots_with_decisions(&stream);

    let report = check(&order_automaton(), &rows, Some(&ots));

    let violation = only_violation(&report);
    assert_eq!(violation.kind, ViolationKind::ForbiddenAction);
    assert_eq!(violation.action, "Delete");
    assert_eq!(
        report.stats.ots_decisions_skipped_as_unrecognized_name, 3,
        "Bash, Read and Edit"
    );
    assert_eq!(
        report.stats.ots_decisions_checked, 2,
        "CancelOrder and Delete; AddItem has a row and is not re-judged"
    );
}

#[test]
fn an_undeclared_action_reached_through_the_harness_is_still_reported() {
    // The check that must survive the fix. The envelope's own action list says
    // these were governed calls, so an undeclared one is a violation however
    // many tool calls surround it.
    let rows = vec![row("AddItem", Some("Draft"), Some("Draft"))];
    let mut ots = mcp_execute_trajectory(
        "temper.action('default', 'Order', 'Frobnicate', {})",
        &["Frobnicate"],
    );
    let now = "2026-01-01T00:00:00Z"
        .parse::<chrono::DateTime<chrono::Utc>>()
        .expect("timestamp parses");
    let mut noise = OTSTurn::new(2, now);
    for tool in HARNESS_TOOLS {
        noise = noise.with_decision(OTSDecision::new(
            DecisionType::ToolSelection,
            OTSChoice::new(*tool),
            OTSConsequence::success(),
        ));
    }
    ots = ots.with_turn(noise);

    let report = check(&order_automaton(), &rows, Some(&ots));

    let violation = only_violation(&report);
    assert_eq!(violation.kind, ViolationKind::UnknownAction);
    assert_eq!(violation.action, "Frobnicate");
    assert_eq!(report.stats.ots_decisions_checked, 1);
}

#[test]
fn an_action_the_session_dispatched_on_another_entity_stays_judgeable() {
    // `PayInvoice` is not in the Order alphabet and not a platform verb, but
    // the kernel dispatched it in this session, so it is an action name in this
    // deployment rather than a tool name — and a decision claiming it against
    // Order is still the fault it always was.
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        TursoTrajectoryRow {
            entity_type: "Invoice".to_string(),
            ..row("PayInvoice", Some("Due"), Some("Paid"))
        },
    ];
    let ots = ots_with_decisions(&["Bash", "PayInvoice"]);

    let report = check(&order_automaton(), &rows, Some(&ots));

    let violation = only_violation(&report);
    assert_eq!(violation.kind, ViolationKind::UnknownAction);
    assert_eq!(violation.action, "PayInvoice");
    assert_eq!(report.stats.ots_decisions_skipped_as_unrecognized_name, 1);
}

#[test]
fn a_caller_supplied_audit_row_cannot_make_a_harness_tool_look_governed() {
    // The injection the vocabulary has to refuse. `POST /api/audit` takes a
    // caller-chosen session and action name; if those names counted as
    // governed, writing one record called `Bash` into somebody else's session
    // would turn every `Bash` in their run into a violation of their spec.
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        TursoTrajectoryRow {
            spec_governed: Some(false),
            ..row("Bash", Some("Draft"), Some("Draft"))
        },
    ];
    let ots = ots_with_decisions(&["Bash"]);

    let report = check(&order_automaton(), &rows, Some(&ots));

    assert!(
        report.passed,
        "a caller does not get to decide what counts as an action: {:?}",
        report.violations
    );
    assert_eq!(report.stats.ots_decisions_skipped_as_unrecognized_name, 1);
    assert_eq!(report.stats.non_governed_rows_skipped, 1);
}

#[test]
fn an_authorization_denial_row_cannot_widen_the_vocabulary() {
    // `POST /api/authorize` reaches `record_authz_denial` with a caller-chosen
    // action name, resource type and `X-Temper-Ctx-SessionId`, and the row it
    // writes leaves `spec_governed` unset — so a filter that only rejects
    // `spec_governed = false` lets it through.
    //
    // The attack it would open: one unauthenticated call naming action `Bash`
    // against somebody else's session puts `Bash` in that session's vocabulary,
    // and every `Bash` in their clean run reports as a violation of a spec they
    // followed. A verdict anyone can flip is not a verdict.
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        TursoTrajectoryRow {
            entity_type: "Widget".to_string(),
            source: Some("Authz".to_string()),
            spec_governed: None,
            success: false,
            authz_denied: Some(true),
            to_status: None,
            ..row("Bash", None, None)
        },
    ];
    let ots = ots_with_decisions(&["Bash"]);

    let report = check(&order_automaton(), &rows, Some(&ots));

    assert!(
        report.passed,
        "an authorization denial names what a caller asked about, not what the \
         kernel dispatched: {:?}",
        report.violations
    );
    assert_eq!(report.stats.ots_decisions_skipped_as_unrecognized_name, 1);
}

#[test]
fn a_platform_bookkeeping_row_does_not_widen_the_vocabulary() {
    // Kernel-written, so the names are not a caller's — but they are already in
    // KERNEL_PLATFORM_ACTIONS, so admitting the source adds nothing and would
    // reopen the marker case. Narrow on purpose.
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        TursoTrajectoryRow {
            source: Some("Platform".to_string()),
            ..row("Bash", Some("Draft"), Some("Draft"))
        },
    ];
    let ots = ots_with_decisions(&["Bash"]);

    let report = check(&order_automaton(), &rows, Some(&ots));

    assert!(report.passed, "{:?}", report.violations);
    assert_eq!(report.stats.ots_decisions_skipped_as_unrecognized_name, 1);
    assert_eq!(report.stats.platform_rows_skipped, 1);
}

#[test]
fn a_harness_tool_named_after_a_platform_verb_is_reported() {
    // The one place the tie-break runs toward reporting, pinned so it stays a
    // decision rather than a surprise. A harness tool called `Delete` is
    // indistinguishable from an agent reaching for the platform's `Delete`, and
    // the checker reports it — the module doc says why.
    let rows = vec![row("AddItem", Some("Draft"), Some("Draft"))];
    let ots = ots_with_decisions(&["Delete"]);

    let report = check(&order_automaton(), &rows, Some(&ots));

    let violation = only_violation(&report);
    assert_eq!(violation.kind, ViolationKind::ForbiddenAction);
    assert_eq!(violation.action, "Delete");
    assert_eq!(
        report.stats.ots_decisions_skipped_as_unrecognized_name, 0,
        "a platform verb is placeable, so it is judged rather than counted"
    );
}

#[test]
fn a_capture_loss_marker_does_not_widen_the_vocabulary() {
    // The marker's action name is the capture path's own. Reading it as an
    // action would make a decision named `CaptureLost` judgeable, and the
    // marker is not an action at all.
    let rows = vec![
        row("AddItem", Some("Draft"), Some("Draft")),
        capture_loss_marker_row(),
    ];
    let ots = ots_with_decisions(&[CAPTURE_LOSS_ACTION]);

    let report = check(&order_automaton(), &rows, Some(&ots));

    assert!(report.violations.is_empty(), "{:?}", report.violations);
    assert_eq!(report.stats.ots_decisions_skipped_as_unrecognized_name, 1);
}
