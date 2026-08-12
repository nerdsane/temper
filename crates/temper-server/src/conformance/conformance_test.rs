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

/// Run the checker over a complete read: the tests that care about truncation
/// build their own [`ConformanceInput`].
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
fn ots_decisions_the_kernel_never_recorded_are_checked_after_the_rows() {
    let rows = vec![row("AddItem", Some("Draft"), Some("Draft"))];
    // `AddItem` already has a row and must not be double-counted; `Frobnicate`
    // never reached the kernel at all.
    let ots = ots_with_decisions(&["AddItem", "Frobnicate"]);

    let report = check(&order_automaton(), &rows, Some(&ots));

    let violation = only_violation(&report);
    assert_eq!(
        violation.index, 1,
        "OTS decisions are indexed after the kernel rows"
    );
    assert_eq!(violation.kind, ViolationKind::UnknownAction);
    assert_eq!(violation.action, "Frobnicate");
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
            .any(|gap| gap.contains("nothing about this run was checked")),
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
    });

    assert!(report.violations.is_empty());
    assert_eq!(
        report.verdict,
        Verdict::Indeterminate,
        "the unread tail of the session could hold anything"
    );
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
